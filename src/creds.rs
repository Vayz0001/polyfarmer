//! Credential store + crypto.
//!
//! Two secrets live here, both under the gitignored `data/` dir:
//!   * the dashboard **admin password** — stored only as an argon2id hash;
//!   * the wallet **private key** — encrypted at rest with ChaCha20-Poly1305.
//!
//! Unlock model (default): the encryption key is an auto-generated 32-byte file
//! (`data/master.key`, perms 0600). This lets the bot resume trading
//! automatically after a restart, and the key is entered via the dashboard —
//! never in `.env`, never in plaintext on disk. Trade-off: an attacker with full
//! disk-read access can decrypt it (inherent to any auto-start system). A
//! password-derived "unlock-on-login" mode can be layered on later.

use std::fs;
use std::path::{Path, PathBuf};

use argon2::password_hash::SaltString;
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use chacha20poly1305::aead::Aead;
use chacha20poly1305::{ChaCha20Poly1305, Key, KeyInit, Nonce};
use eyre::{eyre, Result};
use rand::rngs::OsRng;
use rand::RngCore;
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};

const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;

// ── Password hashing (argon2id) ───────────────────────────────────────────────

/// Hash a password to a PHC string (includes algorithm, params, salt).
pub fn hash_password(password: &str) -> Result<String> {
    let mut salt_bytes = [0u8; 16];
    OsRng.fill_bytes(&mut salt_bytes);
    let salt = SaltString::encode_b64(&salt_bytes).map_err(|e| eyre!("salt: {e}"))?;
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| eyre!("hash: {e}"))?
        .to_string();
    Ok(hash)
}

/// Verify a password against a stored PHC hash. Returns false on mismatch.
pub fn verify_password(password: &str, phc_hash: &str) -> bool {
    match PasswordHash::new(phc_hash) {
        Ok(parsed) => Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok(),
        Err(_) => false,
    }
}

// ── Symmetric encryption (ChaCha20-Poly1305) ──────────────────────────────────

/// Encrypt `plaintext` with a 32-byte key. Output = nonce(12) || ciphertext.
pub fn encrypt(key: &[u8; KEY_LEN], plaintext: &[u8]) -> Result<Vec<u8>> {
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    let mut nonce = [0u8; NONCE_LEN];
    OsRng.fill_bytes(&mut nonce);
    let ct = cipher
        .encrypt(Nonce::from_slice(&nonce), plaintext)
        .map_err(|e| eyre!("encrypt: {e}"))?;
    let mut out = Vec::with_capacity(NONCE_LEN + ct.len());
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Decrypt a nonce(12) || ciphertext blob with a 32-byte key.
pub fn decrypt(key: &[u8; KEY_LEN], blob: &[u8]) -> Result<Vec<u8>> {
    if blob.len() < NONCE_LEN {
        return Err(eyre!("ciphertext too short"));
    }
    let (nonce, ct) = blob.split_at(NONCE_LEN);
    let cipher = ChaCha20Poly1305::new(Key::from_slice(key));
    cipher
        .decrypt(Nonce::from_slice(nonce), ct)
        .map_err(|_| eyre!("decrypt failed (wrong key or tampered data)"))
}

// ── Persisted shapes ──────────────────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
struct AdminRecord {
    password_hash: String,
}

#[derive(Serialize, Deserialize)]
struct WalletRecord {
    private_key: String,
    proxy_wallet: String,
}

/// Decrypted wallet credentials handed to the engine.
pub struct WalletCreds {
    pub private_key: SecretString,
    pub proxy_wallet: String,
}

// ── Credential store ──────────────────────────────────────────────────────────

pub struct CredentialStore {
    dir: PathBuf,
}

impl CredentialStore {
    /// Open (creating if needed) the store at `dir`, ensuring a master key exists.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir)?;
        let store = Self { dir };
        store.ensure_master_key()?;
        Ok(store)
    }

    fn admin_path(&self) -> PathBuf { self.dir.join("admin.json") }
    fn master_path(&self) -> PathBuf { self.dir.join("master.key") }
    fn wallet_path(&self) -> PathBuf { self.dir.join("wallet.enc") }

    fn ensure_master_key(&self) -> Result<()> {
        if !self.master_path().exists() {
            let mut key = [0u8; KEY_LEN];
            OsRng.fill_bytes(&mut key);
            write_secret(&self.master_path(), &key)?;
        }
        Ok(())
    }

    fn master_key(&self) -> Result<[u8; KEY_LEN]> {
        let bytes = fs::read(self.master_path())?;
        if bytes.len() != KEY_LEN {
            return Err(eyre!("master.key is corrupt ({} bytes)", bytes.len()));
        }
        let mut key = [0u8; KEY_LEN];
        key.copy_from_slice(&bytes);
        Ok(key)
    }

    // ── admin password ────────────────────────────────────────────────────────

    /// Whether the admin password has been set (first-run wizard completed).
    pub fn is_initialized(&self) -> bool {
        self.admin_path().exists()
    }

    fn read_admin(&self) -> Result<AdminRecord> {
        let raw = fs::read_to_string(self.admin_path())?;
        Ok(serde_json::from_str(&raw)?)
    }

    pub fn verify_login(&self, password: &str) -> Result<bool> {
        Ok(verify_password(password, &self.read_admin()?.password_hash))
    }

    /// Set (or change) the admin password.
    pub fn set_password(&self, password: &str) -> Result<()> {
        let rec = AdminRecord {
            password_hash: hash_password(password)?,
        };
        write_secret(&self.admin_path(), serde_json::to_string_pretty(&rec)?.as_bytes())
    }

    // ── wallet credentials ──────────────────────────────────────────────────────

    pub fn has_wallet(&self) -> bool {
        self.wallet_path().exists()
    }

    /// Encrypt and store the wallet credentials.
    pub fn set_wallet(&self, private_key: &str, proxy_wallet: &str) -> Result<()> {
        let rec = WalletRecord {
            private_key: private_key.to_string(),
            proxy_wallet: proxy_wallet.to_string(),
        };
        let plaintext = serde_json::to_vec(&rec)?;
        let blob = encrypt(&self.master_key()?, &plaintext)?;
        write_secret(&self.wallet_path(), &blob)
    }

    /// Load and decrypt the wallet credentials, if configured.
    pub fn load_wallet(&self) -> Result<Option<WalletCreds>> {
        if !self.has_wallet() {
            return Ok(None);
        }
        let blob = fs::read(self.wallet_path())?;
        let plaintext = decrypt(&self.master_key()?, &blob)?;
        let rec: WalletRecord = serde_json::from_slice(&plaintext)?;
        Ok(Some(WalletCreds {
            private_key: SecretString::from(rec.private_key),
            proxy_wallet: rec.proxy_wallet,
        }))
    }
}

impl WalletCreds {
    /// Expose the decrypted private key (handed to the signer).
    pub fn expose_key(&self) -> &str {
        self.private_key.expose_secret()
    }
}

// ── helpers ───────────────────────────────────────────────────────────────────

/// Atomic write of a secret file with 0600 perms (temp + rename).
fn write_secret(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    chmod_600(&tmp)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(unix)]
fn chmod_600(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}
#[cfg(not(unix))]
fn chmod_600(_path: &Path) -> Result<()> {
    Ok(())
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!("polyfarmer-test-{}", uuid::Uuid::new_v4()))
    }

    #[test]
    fn password_hash_roundtrip() {
        let h = hash_password("hunter2").unwrap();
        assert!(verify_password("hunter2", &h));
        assert!(!verify_password("wrong", &h));
        assert!(!verify_password("hunter2", "not-a-valid-hash"));
    }

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let mut key = [0u8; KEY_LEN];
        OsRng.fill_bytes(&mut key);
        let blob = encrypt(&key, b"0xdeadbeef secret key").unwrap();
        assert_eq!(decrypt(&key, &blob).unwrap(), b"0xdeadbeef secret key");

        let mut wrong = [0u8; KEY_LEN];
        OsRng.fill_bytes(&mut wrong);
        assert!(decrypt(&wrong, &blob).is_err()); // wrong key
        assert!(decrypt(&key, b"short").is_err()); // truncated
    }

    #[test]
    fn store_admin_and_wallet_lifecycle() {
        let dir = temp_dir();
        let store = CredentialStore::open(&dir).unwrap();

        assert!(!store.is_initialized());
        store.set_password("first-pass").unwrap(); // first-run wizard sets it
        assert!(store.is_initialized());
        assert!(store.verify_login("first-pass").unwrap());
        assert!(!store.verify_login("nope").unwrap());

        store.set_password("new-strong-pass").unwrap();
        assert!(store.verify_login("new-strong-pass").unwrap());

        assert!(!store.has_wallet());
        store.set_wallet("0xprivkey", "0xWalletAddr").unwrap();
        assert!(store.has_wallet());
        let w = store.load_wallet().unwrap().unwrap();
        assert_eq!(w.expose_key(), "0xprivkey");
        assert_eq!(w.proxy_wallet, "0xWalletAddr");

        // master key persists; reopening still decrypts
        let store2 = CredentialStore::open(&dir).unwrap();
        assert_eq!(store2.load_wallet().unwrap().unwrap().expose_key(), "0xprivkey");

        let _ = fs::remove_dir_all(&dir);
    }
}
