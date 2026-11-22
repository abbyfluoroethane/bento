//! SSH key material for the frontend (SPEC 10): one host key every
//! connection sees, and one keypair the frontend uses toward guests. Both are
//! ed25519, generated on first use, and stored under the operator key directory.

use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};
use russh::keys::{Algorithm, PrivateKey, PublicKey, ssh_key};

use crate::setup::App;

pub(crate) const HOST_KEY_FILE: &str = "ssh_host_ed25519_key";
pub(crate) const FRONTEND_KEY_FILE: &str = "frontend_ed25519_key";

pub(crate) fn key_path(app: &App, file: &str) -> PathBuf {
    Path::new(&app.cfg.key_dir).join(file)
}

/// Loads an OpenSSH private key, generating it and a `.pub` sibling when it
/// does not exist. New private keys are created mode 0600.
///
/// `serve` and `sshd` start together and both call this. The key is written
/// to a temporary file first and then linked into place. A link is atomic and
/// fails when the name exists, so no process ever sees a half-written key,
/// a crash leaves only a stale temporary file, and the loser of a race loads
/// the key of the winner (SPEC 10).
pub(crate) fn ensure_key(path: &Path, comment: &str) -> Result<Arc<PrivateKey>> {
    let key = match read_key(path)? {
        Some(key) => key,
        None => create_key(path, comment)?,
    };
    // A crash after the private key left no `.pub`. Write it again from the
    // key that is on disk.
    let public_path = PathBuf::from(format!("{}.pub", path.display()));
    if !public_path.exists() {
        let line = authorized_key_line(key.public_key(), comment)?;
        let temp = write_temp(&public_path, 0o644, format!("{line}\n").as_bytes())?;
        // Rename replaces the target in one step. Two writers write the same
        // text, so the one that lands last changes nothing.
        std::fs::rename(&temp, &public_path).inspect_err(|_| drop(std::fs::remove_file(&temp)))?;
    }
    Ok(Arc::new(key))
}

fn read_key(path: &Path) -> Result<Option<PrivateKey>> {
    match std::fs::read_to_string(path) {
        Ok(data) => Ok(Some(
            russh::keys::decode_secret_key(&data, None)
                .with_context(|| format!("parse {}", path.display()))?,
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn create_key(path: &Path, comment: &str) -> Result<PrivateKey> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    let mut key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)?;
    key.set_comment(comment.to_owned());
    let encoded = key.to_openssh(ssh_key::LineEnding::LF)?;
    let temp = write_temp(path, 0o600, encoded.as_bytes())?;
    let linked = std::fs::hard_link(&temp, path);
    drop(std::fs::remove_file(&temp));
    match linked {
        Ok(()) => Ok(key),
        // Another process won. Use its key, not ours.
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            read_key(path)?.with_context(|| format!("{} vanished after a key race", path.display()))
        }
        Err(error) => Err(error.into()),
    }
}

/// Writes `data` to a new file beside `path` and returns its name. The name
/// holds the process ID and a counter, so two processes or two threads never
/// share one. A stale file of a crashed process with the same name is
/// removed first.
fn write_temp(path: &Path, mode: u32, data: &[u8]) -> Result<PathBuf> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let name = path
        .file_name()
        .map_or_else(String::new, |name| name.to_string_lossy().into_owned());
    let temp = path.with_file_name(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    drop(std::fs::remove_file(&temp));
    let result = (|| -> std::io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&temp)?;
        file.write_all(data)?;
        file.sync_all()
    })();
    if let Err(error) = result {
        drop(std::fs::remove_file(&temp));
        return Err(error.into());
    }
    Ok(temp)
}

pub(crate) fn authorized_key_line(public: &PublicKey, comment: &str) -> Result<String> {
    let mut public = public.clone();
    public.set_comment(comment.to_owned());
    Ok(public.to_openssh()?.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_key_creates_and_reloads() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("keys").join(FRONTEND_KEY_FILE);
        let first = ensure_key(&path, "bento-frontend").unwrap();
        let second = ensure_key(&path, "bento-frontend").unwrap();
        let first = authorized_key_line(first.public_key(), "").unwrap();
        let second = authorized_key_line(second.public_key(), "").unwrap();
        assert_eq!(first, second);
        assert!(first.starts_with("ssh-ed25519 "));
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(
            std::fs::read_to_string(format!("{}.pub", path.display()))
                .unwrap()
                .contains("bento-frontend")
        );
    }

    #[test]
    fn a_stale_temporary_file_does_not_stop_ensure_key() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(HOST_KEY_FILE);
        // What a crash between create and link leaves.
        std::fs::write(
            temp.path().join(format!(".{HOST_KEY_FILE}.1.0.tmp")),
            b"half",
        )
        .unwrap();
        ensure_key(&path, "bento-host").unwrap();
        assert!(ensure_key(&path, "bento-host").is_ok());
    }

    #[test]
    fn concurrent_callers_get_one_key() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("keys").join(HOST_KEY_FILE);
        let lines: Vec<String> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    scope.spawn(|| {
                        let key = ensure_key(&path, "bento-host").unwrap();
                        authorized_key_line(key.public_key(), "").unwrap()
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert!(lines.iter().all(|line| *line == lines[0]));
        let on_disk = ensure_key(&path, "bento-host").unwrap();
        assert_eq!(
            authorized_key_line(on_disk.public_key(), "").unwrap(),
            lines[0]
        );
        let leftovers = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter(|entry| {
                entry
                    .as_ref()
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .ends_with(".tmp")
            })
            .count();
        assert_eq!(leftovers, 0);
    }

    #[test]
    fn a_missing_public_file_is_written_again() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(FRONTEND_KEY_FILE);
        let key = ensure_key(&path, "bento-frontend").unwrap();
        let public_path = format!("{}.pub", path.display());
        std::fs::remove_file(&public_path).unwrap();
        ensure_key(&path, "bento-frontend").unwrap();
        let line = authorized_key_line(key.public_key(), "bento-frontend").unwrap();
        assert_eq!(std::fs::read_to_string(public_path).unwrap().trim(), line);
    }
}
