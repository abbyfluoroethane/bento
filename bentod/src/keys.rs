//! SSH key material for the frontend (SPEC 10): one host key every
//! connection sees, and one keypair the frontend uses toward guests. Both are
//! ed25519, generated on first use, and stored under the operator key directory.

use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

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
pub(crate) fn ensure_key(path: &Path, comment: &str) -> Result<Arc<PrivateKey>> {
    match std::fs::read_to_string(path) {
        Ok(data) => {
            let key = russh::keys::decode_secret_key(&data, None)
                .with_context(|| format!("parse {}", path.display()))?;
            return Ok(Arc::new(key));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
    }
    let mut key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)?;
    key.set_comment(comment.to_owned());
    let encoded = key.to_openssh(ssh_key::LineEnding::LF)?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(encoded.as_bytes())?;
    file.sync_all()?;

    let public = authorized_key_line(key.public_key(), comment)?;
    let public_path = PathBuf::from(format!("{}.pub", path.display()));
    let mut public_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o644)
        .open(public_path)?;
    writeln!(public_file, "{public}")?;
    public_file.sync_all()?;
    Ok(Arc::new(key))
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
}
