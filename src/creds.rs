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
use std::path::PathBuf;

use argon2::password_hash::SaltString;
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use chacha20poly1305::aead::Aead;
use chacha20poly1305::{ChaCha20Poly1305, Key, KeyInit, Nonce};
use eyre::{eyre, Result};
use rand::rngs::OsRng;
use rand::{Rng, RngCore};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use zeroize::Zeroizing;

use crate::fsutil::{create_private_dir, write_private_atomic};

const NONCE_LEN: usize = 12;
const KEY_LEN: usize = 32;

// ── First-run setup code ──────────────────────────────────────────────────────
// Until the admin password exists, anyone who can reach the dashboard could
// claim the setup page. Reverse proxies and Tailscale Serve connect from
// 127.0.0.1, so "is the peer local?" can't tell the owner from a stranger.
// Instead, first-run needs a one-time code that only someone with access to the
// bot's log / data folder can read.

/// No look-alikes (0/o, 1/l/i): easy to read off a log and type.
const SETUP_CODE_ALPHABET: &[u8] = b"abcdefghjkmnpqrstuvwxyz23456789";
const SETUP_CODE_LEN: usize = 8;

/// `abcdefgh` -> `abcd-efgh` for display.
fn format_setup_code(raw: &str) -> String {
    let n = normalize_code(raw);
    match n.len() {
        len if len > 4 => format!("{}-{}", &n[..4], &n[4..]),
        _ => n,
    }
}

/// Lowercase, alphanumerics only — so `ABCD-EFGH`, `abcd efgh` and `abcdefgh`
/// all match.
fn normalize_code(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_lowercase()).collect()
}

/// Compare without bailing out at the first differing byte.
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

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
    /// Wiped from memory when the record is dropped.
    private_key: Zeroizing<String>,
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
        // Created 0700 (an already-existing dir is left alone, with a warning if
        // it is readable by other users). Files inside are 0600 from creation.
        create_private_dir(&dir)?;
        let store = Self { dir };
        store.ensure_master_key()?;
        store.ensure_setup_code()?;
        Ok(store)
    }

    fn admin_path(&self) -> PathBuf { self.dir.join("admin.json") }
    fn master_path(&self) -> PathBuf { self.dir.join("master.key") }
    fn wallet_path(&self) -> PathBuf { self.dir.join("wallet.enc") }
    fn setup_code_path(&self) -> PathBuf { self.dir.join("setup.code") }

    fn ensure_master_key(&self) -> Result<()> {
        if !self.master_path().exists() {
            let mut key = Zeroizing::new([0u8; KEY_LEN]);
            OsRng.fill_bytes(&mut *key);
            write_private_atomic(&self.master_path(), &*key)?;
        }
        Ok(())
    }

    fn master_key(&self) -> Result<Zeroizing<[u8; KEY_LEN]>> {
        let bytes = Zeroizing::new(fs::read(self.master_path())?);
        if bytes.len() != KEY_LEN {
            return Err(eyre!("master.key is corrupt ({} bytes)", bytes.len()));
        }
        let mut key = Zeroizing::new([0u8; KEY_LEN]);
        key.copy_from_slice(&bytes);
        Ok(key)
    }

    // ── first-run setup code ────────────────────────────────────────────────────

    /// While no admin password exists, make sure a setup code is on disk
    /// (`setup.code`, 0600). Once the password is set the code is deleted.
    fn ensure_setup_code(&self) -> Result<()> {
        if self.is_initialized() || self.setup_code_path().exists() {
            return Ok(());
        }
        let raw: String = (0..SETUP_CODE_LEN)
            .map(|_| SETUP_CODE_ALPHABET[OsRng.gen_range(0..SETUP_CODE_ALPHABET.len())] as char)
            .collect();
        write_private_atomic(&self.setup_code_path(), format_setup_code(&raw).as_bytes())
    }

    /// The pending setup code (`abcd-efgh`), or `None` once setup is complete.
    pub fn setup_code(&self) -> Option<String> {
        if self.is_initialized() {
            return None;
        }
        fs::read_to_string(self.setup_code_path()).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
    }

    /// Whether `input` matches the pending setup code. Always false once the
    /// admin password exists.
    pub fn verify_setup_code(&self, input: &str) -> bool {
        match self.setup_code() {
            Some(code) => {
                let (want, got) = (normalize_code(&code), normalize_code(input));
                !got.is_empty() && ct_eq(want.as_bytes(), got.as_bytes())
            }
            None => false,
        }
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
        write_private_atomic(&self.admin_path(), serde_json::to_string_pretty(&rec)?.as_bytes())?;
        // First-run is over — the setup code must not outlive it.
        let _ = fs::remove_file(self.setup_code_path());
        Ok(())
    }

    // ── wallet credentials ──────────────────────────────────────────────────────

    pub fn has_wallet(&self) -> bool {
        self.wallet_path().exists()
    }

    /// Encrypt and store the wallet credentials.
    pub fn set_wallet(&self, private_key: &str, proxy_wallet: &str) -> Result<()> {
        let rec = WalletRecord {
            private_key: Zeroizing::new(private_key.to_string()),
            proxy_wallet: proxy_wallet.to_string(),
        };
        let plaintext = Zeroizing::new(serde_json::to_vec(&rec)?);
        let key = self.master_key()?;
        let blob = encrypt(&key, &plaintext)?;
        write_private_atomic(&self.wallet_path(), &blob)
    }

    /// Load and decrypt the wallet credentials, if configured.
    pub fn load_wallet(&self) -> Result<Option<WalletCreds>> {
        if !self.has_wallet() {
            return Ok(None);
        }
        let blob = fs::read(self.wallet_path())?;
        let key = self.master_key()?;
        let plaintext = Zeroizing::new(decrypt(&key, &blob)?);
        let rec: WalletRecord = serde_json::from_slice(&plaintext)?;
        Ok(Some(WalletCreds {
            // `rec` (and its Zeroizing key) is wiped when this function returns.
            private_key: SecretString::from(rec.private_key.to_string()),
            proxy_wallet: rec.proxy_wallet.clone(),
        }))
    }
}

impl WalletCreds {
    /// Expose the decrypted private key (handed to the signer).
    pub fn expose_key(&self) -> &str {
        self.private_key.expose_secret()
    }
}

// File writes go through `crate::fsutil` (0600 from creation, atomic, fsynced).

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
    fn setup_code_lifecycle() {
        let dir = temp_dir();
        let store = CredentialStore::open(&dir).unwrap();

        // Uninitialised → a code exists, formatted abcd-efgh, from the safe alphabet.
        let code = store.setup_code().expect("code on first run");
        assert_eq!(code.len(), 9);
        assert_eq!(code.as_bytes()[4], b'-');
        assert!(code.chars().filter(|c| *c != '-').all(|c| SETUP_CODE_ALPHABET.contains(&(c as u8))));

        // Stable across reopen (so the code in the log stays valid across a restart).
        assert_eq!(CredentialStore::open(&dir).unwrap().setup_code().as_deref(), Some(code.as_str()));

        // Accepts the code however it was typed; rejects everything else.
        assert!(store.verify_setup_code(&code));
        assert!(store.verify_setup_code(&code.to_uppercase()));
        assert!(store.verify_setup_code(&format!("  {}  ", code.replace('-', " "))));
        assert!(!store.verify_setup_code(""));
        assert!(!store.verify_setup_code("----"));
        assert!(!store.verify_setup_code("aaaa-aaaa"));
        assert!(!store.verify_setup_code(&code[..8]));

        // Setting the password consumes the code.
        store.set_password("first-pass").unwrap();
        assert_eq!(store.setup_code(), None);
        assert!(!store.verify_setup_code(&code), "a used code must not work again");
        assert!(!dir.join("setup.code").exists());

        // An already-initialised install never grows a code.
        assert_eq!(CredentialStore::open(&dir).unwrap().setup_code(), None);
        assert!(!dir.join("setup.code").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    /// Every file and the folder the store creates is owner-only from the moment
    /// it exists (no create-then-chmod window), even under a permissive umask.
    #[cfg(unix)]
    #[test]
    fn data_dir_and_every_secret_file_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &std::path::Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let dir = temp_dir().join("data");

        let old = unsafe { libc::umask(0o000) };
        let store = CredentialStore::open(&dir);
        let store = store.map(|s| {
            s.set_password("a-long-enough-password").unwrap();
            s.set_wallet("0xprivkey", "0xWalletAddr").unwrap();
            s
        });
        unsafe { libc::umask(old) };
        let _store = store.unwrap();

        assert_eq!(mode(&dir), 0o700, "data dir");
        for f in ["master.key", "admin.json", "wallet.enc"] {
            assert_eq!(mode(&dir.join(f)), 0o600, "{f}");
        }
        // No temp files linger after atomic writes.
        let leftovers: Vec<_> = fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name()).filter(|n| n.to_string_lossy().ends_with(".tmp")).collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
        let _ = fs::remove_dir_all(dir.parent().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn setup_code_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir();
        let _store = CredentialStore::open(&dir).unwrap();
        let mode = fs::metadata(dir.join("setup.code")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let _ = fs::remove_dir_all(&dir);
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
