//! The link adapter's credential file (runs.md "Startup and recovery": the Ed25519 key pair is
//! generated on first start, stored with mode 0600 in the runner state dir, and never leaves the
//! host; the `runner_id` is recorded once the bootstrap registration succeeds).

use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use ed25519_dalek::SigningKey;
use mage_core::domain::RunnerId;
use rand::RngCore;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialFile {
    /// Ed25519 seed, lowercase hex.
    secret_key: String,
    runner_id: Option<RunnerId>,
}

/// Runner identity: key pair plus the server-assigned id (absent until the first `register`).
pub struct Credentials {
    path: PathBuf,
    key: SigningKey,
    runner_id: Option<RunnerId>,
}

impl Credentials {
    /// Reads the credential file, or creates it with a fresh key pair (mode 0600; parent dir
    /// created with mode 0700).
    pub fn load_or_create(path: &Path) -> std::io::Result<Credentials> {
        match std::fs::read(path) {
            Ok(bytes) => {
                let file: CredentialFile = serde_json::from_slice(&bytes).map_err(std::io::Error::other)?;
                let seed: [u8; 32] = hex::decode(&file.secret_key)
                    .map_err(std::io::Error::other)?
                    .try_into()
                    .map_err(|_| std::io::Error::other("secret_key is not 32 bytes"))?;
                Ok(Credentials { path: path.to_owned(), key: SigningKey::from_bytes(&seed), runner_id: file.runner_id })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let mut seed = [0u8; 32];
                rand::rngs::OsRng.fill_bytes(&mut seed);
                let credentials = Credentials { path: path.to_owned(), key: SigningKey::from_bytes(&seed), runner_id: None };
                credentials.persist()?;
                Ok(credentials)
            }
            Err(e) => Err(e),
        }
    }

    pub fn key(&self) -> &SigningKey {
        &self.key
    }

    pub fn public_key(&self) -> [u8; 32] {
        self.key.verifying_key().to_bytes()
    }

    pub fn runner_id(&self) -> Option<RunnerId> {
        self.runner_id
    }

    /// Records the server-assigned id (atomic rewrite).
    pub fn set_runner_id(&mut self, id: RunnerId) -> std::io::Result<()> {
        if self.runner_id == Some(id) {
            return Ok(());
        }
        self.runner_id = Some(id);
        self.persist()
    }

    fn persist(&self) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent().filter(|d| !d.as_os_str().is_empty()) {
            if !dir.exists() {
                std::fs::create_dir_all(dir)?;
                std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
            }
        }
        let file = CredentialFile { secret_key: hex::encode(self.key.to_bytes()), runner_id: self.runner_id };
        let body = serde_json::to_vec_pretty(&file).map_err(std::io::Error::other)?;
        let tmp = self.path.with_extension("tmp");
        {
            let mut out = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
            out.write_all(&body)?;
            out.sync_all()?;
        }
        std::fs::rename(&tmp, &self.path)
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn key_pair_is_created_once_with_mode_0600_and_survives_reload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("link").join("credentials.json");
        let mut first = Credentials::load_or_create(&path).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(first.runner_id(), None);
        let id: RunnerId = uuid::Uuid::now_v7().into();
        first.set_runner_id(id).unwrap();
        let again = Credentials::load_or_create(&path).unwrap();
        assert_eq!(again.public_key(), first.public_key());
        assert_eq!(again.runner_id(), Some(id));
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
    }
}
