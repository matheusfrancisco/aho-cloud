//! [`LocalKeyFileKek`]: development KEK, AES-256-GCM under one 32-byte key read from a file
//! (server.md "Vault and signer"). One key serves every scope. Refused in production and for a
//! file other users can read. Wrap format: `nonce (12) ‖ ct+tag`.

use std::path::Path;

use async_trait::async_trait;

use super::Kek;
use super::envelope::{KEY_LEN, NONCE_LEN, TAG_LEN, open, random_nonce, seal};
use crate::ports::vault::{SecretBytes, VaultError};

/// `kek_version` of every wrap made by a local key file.
pub const LOCAL_KEK_VERSION: &str = "local-v1";

pub struct LocalKeyFileKek {
    key: SecretBytes,
    version: String,
}

impl LocalKeyFileKek {
    /// Reads the raw 32-byte key at `path`. Refused when `production`, when the file grants any
    /// access to group or others (create it with mode 0600), or when it is not exactly 32 bytes.
    pub fn open(path: &Path, production: bool) -> Result<LocalKeyFileKek, VaultError> {
        if production {
            return Err(VaultError::Invalid("the local key file KEK is refused when environment = \"production\"; use gcp-kms".into()));
        }
        let shown = path.display();
        let metadata = std::fs::metadata(path).map_err(|e| VaultError::Invalid(format!("local key file {shown}: {e}")))?;
        check_mode(path, &metadata)?;
        let key = SecretBytes::new(std::fs::read(path).map_err(|e| VaultError::Invalid(format!("local key file {shown}: {e}")))?);
        if key.len() != KEY_LEN {
            return Err(VaultError::Invalid(format!("local key file {shown} must hold exactly {KEY_LEN} raw bytes, found {}", key.len())));
        }
        Ok(LocalKeyFileKek { key, version: LOCAL_KEK_VERSION.into() })
    }

    /// A KEK over `key` (tests and fakes).
    pub fn from_key(key: [u8; KEY_LEN]) -> LocalKeyFileKek {
        LocalKeyFileKek::with_version(key, LOCAL_KEK_VERSION)
    }

    /// A KEK over `key` that labels its wraps `version` (test KEK rings).
    pub(crate) fn with_version(mut key: [u8; KEY_LEN], version: &str) -> LocalKeyFileKek {
        let kek = LocalKeyFileKek { key: SecretBytes::new(key.to_vec()), version: version.into() };
        crate::ports::vault::zero(&mut key);
        kek
    }

    pub fn version(&self) -> &str {
        &self.version
    }
}

#[cfg(unix)]
fn check_mode(path: &Path, metadata: &std::fs::Metadata) -> Result<(), VaultError> {
    use std::os::unix::fs::PermissionsExt;
    let mode = metadata.permissions().mode() & 0o777;
    if !metadata.is_file() || mode & 0o077 != 0 {
        return Err(VaultError::Invalid(format!(
            "local key file {} must be a regular file with mode 0600 (found {mode:04o})",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(not(unix))]
fn check_mode(path: &Path, _metadata: &std::fs::Metadata) -> Result<(), VaultError> {
    Err(VaultError::Invalid(format!("local key file {}: file mode cannot be checked on this platform", path.display())))
}

#[async_trait]
impl Kek for LocalKeyFileKek {
    async fn wrap(&self, aad: &[u8], dek: &[u8]) -> Result<(Vec<u8>, String), VaultError> {
        let nonce = random_nonce();
        let sealed = seal(self.key.expose(), &nonce, aad, dek)?;
        let mut wrapped = Vec::with_capacity(NONCE_LEN + sealed.len());
        wrapped.extend_from_slice(&nonce);
        wrapped.extend_from_slice(&sealed);
        Ok((wrapped, self.version.clone()))
    }

    async fn unwrap(&self, aad: &[u8], wrapped: &[u8], kek_version: &str) -> Result<SecretBytes, VaultError> {
        if kek_version != self.version {
            return Err(VaultError::Invalid(format!("DEK wrapped by KEK version {kek_version:?}, this key file is {:?}", self.version)));
        }
        if wrapped.len() < NONCE_LEN + TAG_LEN {
            return Err(VaultError::Integrity);
        }
        let (nonce, sealed) = wrapped.split_at(NONCE_LEN);
        let mut nonce_bytes = [0u8; NONCE_LEN];
        nonce_bytes.copy_from_slice(nonce);
        open(self.key.expose(), &nonce_bytes, aad, sealed)
    }

    async fn primary_version(&self) -> Result<String, VaultError> {
        Ok(self.version.clone())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn key_file(dir: &Path, mode: u32, bytes: &[u8]) -> std::path::PathBuf {
        let path = dir.join("dev.key");
        let mut file = std::fs::File::create(&path).expect("create key file");
        file.write_all(bytes).expect("write key");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
        path
    }

    #[tokio::test]
    async fn key_file_round_trips_and_binds_the_aad() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = key_file(dir.path(), 0o600, &[9u8; 32]);
        let kek = LocalKeyFileKek::open(&path, false).expect("open 0600 key file");
        let (wrapped, version) = kek.wrap(b"mage-secret/v1|platform|a/b|1", &[7u8; 32]).await.expect("wrap");
        assert_eq!(version, LOCAL_KEK_VERSION);
        let dek = kek.unwrap(b"mage-secret/v1|platform|a/b|1", &wrapped, &version).await.expect("unwrap");
        assert_eq!(dek.expose(), &[7u8; 32]);

        assert_eq!(kek.unwrap(b"mage-secret/v1|platform|a/b|2", &wrapped, &version).await.err(), Some(VaultError::Integrity));
        let mut flipped = wrapped.clone();
        let last = flipped.len() - 1;
        flipped[last] ^= 1;
        assert_eq!(kek.unwrap(b"mage-secret/v1|platform|a/b|1", &flipped, &version).await.err(), Some(VaultError::Integrity));
        let other = LocalKeyFileKek::from_key([8u8; 32]);
        assert_eq!(other.unwrap(b"mage-secret/v1|platform|a/b|1", &wrapped, &version).await.err(), Some(VaultError::Integrity));
        assert!(matches!(kek.unwrap(b"mage-secret/v1|platform|a/b|1", &wrapped, "local-v2").await, Err(VaultError::Invalid(_))));
    }

    #[test]
    fn readable_by_others_production_and_wrong_length_are_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = key_file(dir.path(), 0o644, &[9u8; 32]);
        assert!(matches!(LocalKeyFileKek::open(&path, false), Err(VaultError::Invalid(m)) if m.contains("0600")));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).expect("chmod");
        assert!(LocalKeyFileKek::open(&path, false).is_err());

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        assert!(matches!(LocalKeyFileKek::open(&path, true), Err(VaultError::Invalid(m)) if m.contains("production")));
        assert!(LocalKeyFileKek::open(&path, false).is_ok());

        let short = dir.path().join("short.key");
        std::fs::write(&short, [1u8; 31]).expect("write");
        std::fs::set_permissions(&short, std::fs::Permissions::from_mode(0o600)).expect("chmod");
        assert!(matches!(LocalKeyFileKek::open(&short, false), Err(VaultError::Invalid(m)) if m.contains("32")));
        assert!(LocalKeyFileKek::open(&dir.path().join("missing.key"), false).is_err());
    }
}
