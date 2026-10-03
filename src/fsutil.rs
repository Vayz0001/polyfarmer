//! Private-file helpers for everything the bot persists under its data dir.
//!
//! Secrets (wallet ciphertext, master key, admin hash, setup code) must never
//! exist on disk with looser permissions than 0600, not even briefly. The old
//! pattern — `fs::write` then `chmod 0600` — creates the file with the default
//! umask (typically 0644) first, leaving a window where another local user can
//! read it. Here the file is created with its final mode in the same
//! `open(2)` call (`O_CREAT|O_EXCL`, mode 0600), written, fsynced, and only then
//! renamed into place, so a reader sees either the old file or the complete new
//! one — never a partial or world-readable one.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use eyre::{Result, WrapErr};

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};

/// Create `dir` (and missing parents) readable only by the owner (0700).
///
/// A directory that already exists is left exactly as it is — the user may
/// have pointed `DATA_DIR` at a folder they share or manage themselves, and
/// silently chmod-ing it could break their setup — but a warning is logged if
/// other users could read it.
pub fn create_private_dir(dir: &Path) -> Result<()> {
    if dir.exists() {
        warn_if_accessible(dir);
        return Ok(());
    }
    #[cfg(unix)]
    {
        fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(dir)?;
    }
    Ok(())
}

#[cfg(unix)]
fn warn_if_accessible(dir: &Path) {
    if let Ok(meta) = fs::metadata(dir) {
        let mode = meta.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            tracing::warn!(
                "data directory {} is accessible to other users (mode {:o}); run `chmod 700 {}` — secrets inside are 0600, but the folder listing and state files are not private",
                dir.display(),
                mode,
                dir.display()
            );
        }
    }
}

#[cfg(not(unix))]
fn warn_if_accessible(_dir: &Path) {}

/// `OpenOptions` for a brand-new file that is private from the instant it exists.
fn new_private_file(path: &Path) -> std::io::Result<fs::File> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    opts.mode(0o600);
    opts.open(path)
}

/// Sibling temp path: `markets.json` -> `markets.json.tmp`.
fn tmp_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(".tmp");
    path.with_file_name(name)
}

/// Atomically replace `path` with `bytes`, owner-only (0600) from creation.
///
/// temp file (created 0600, `O_EXCL`) -> write -> fsync -> rename -> fsync the
/// directory. A crash leaves either the old file or the new one.
pub fn write_private_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let tmp = tmp_path(path);
    // A leftover temp from a crash would make O_EXCL fail; it holds no state we need.
    let _ = fs::remove_file(&tmp);

    let result = (|| -> Result<()> {
        let mut file = new_private_file(&tmp).wrap_err_with(|| format!("creating {}", tmp.display()))?;
        file.write_all(bytes)?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(e);
    }
    if let Err(e) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(e).wrap_err_with(|| format!("replacing {}", path.display()));
    }
    sync_parent(path);
    Ok(())
}

/// Open `path` for appending, creating it owner-only (0600) if missing.
pub fn open_private_append(path: &Path) -> std::io::Result<fs::File> {
    let mut opts = fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    opts.mode(0o600);
    opts.open(path)
}

/// Best-effort fsync of the containing directory so the rename itself is durable.
fn sync_parent(path: &Path) {
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        let parent = if parent.as_os_str().is_empty() { Path::new(".") } else { parent };
        if let Ok(dir) = fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn tmp() -> PathBuf {
        std::env::temp_dir().join(format!("pf-fsutil-{}", uuid::Uuid::new_v4()))
    }
    fn mode(p: &Path) -> u32 {
        fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn new_dirs_are_owner_only_and_existing_dirs_are_left_alone() {
        let base = tmp();
        let nested = base.join("a").join("b");
        create_private_dir(&nested).unwrap();
        assert_eq!(mode(&nested), 0o700);
        assert_eq!(mode(&base), 0o700, "parents we had to create are private too");

        // An existing, wider directory is NOT chmod-ed (only warned about).
        let shared = tmp();
        fs::create_dir_all(&shared).unwrap();
        fs::set_permissions(&shared, fs::Permissions::from_mode(0o755)).unwrap();
        create_private_dir(&shared).unwrap();
        assert_eq!(mode(&shared), 0o755);
        let _ = fs::remove_dir_all(&base);
        let _ = fs::remove_dir_all(&shared);
    }

    #[test]
    fn atomic_write_is_private_from_creation_regardless_of_umask() {
        let dir = tmp();
        create_private_dir(&dir).unwrap();
        let path = dir.join("secret.bin");

        // Even with a permissive umask the file must come out 0600. (Setting the
        // umask is process-wide, so restore it immediately.)
        let old = unsafe { libc::umask(0o000) };
        let res = write_private_atomic(&path, b"first");
        unsafe { libc::umask(old) };
        res.unwrap();
        assert_eq!(mode(&path), 0o600);
        assert_eq!(fs::read(&path).unwrap(), b"first");

        // Replacing keeps it private and leaves no temp file behind.
        write_private_atomic(&path, b"second").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"second");
        assert_eq!(mode(&path), 0o600);
        assert!(!dir.join("secret.bin.tmp").exists());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_stale_temp_file_does_not_block_the_next_write() {
        let dir = tmp();
        create_private_dir(&dir).unwrap();
        let path = dir.join("state.json");
        fs::write(dir.join("state.json.tmp"), b"leftover from a crash").unwrap();
        write_private_atomic(&path, b"ok").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"ok");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn append_creates_private_and_appends() {
        let dir = tmp();
        create_private_dir(&dir).unwrap();
        let path = dir.join("alerts.json");
        let old = unsafe { libc::umask(0o000) };
        let mut f = open_private_append(&path).unwrap();
        unsafe { libc::umask(old) };
        f.write_all(b"one\n").unwrap();
        drop(f);
        open_private_append(&path).unwrap().write_all(b"two\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "one\ntwo\n");
        assert_eq!(mode(&path), 0o600);
        let _ = fs::remove_dir_all(&dir);
    }
}
