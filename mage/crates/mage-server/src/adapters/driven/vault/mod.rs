//! `SecretVault` adapter (server.md "Vault and signer", "Envelope design (`SecretVault`)"):
//! [`EnvelopeVault`] seals every secret version and org data key with its own AES-256-GCM DEK and
//! wraps the DEK with a key-encryption key ([`Kek`]): Cloud KMS in production ([`GcpKmsKek`]), a
//! local 32-byte key file in development ([`LocalKeyFileKek`]). Rows persist through
//! `SecretStore`; this module never opens a database connection.

mod envelope;
mod gcp_kms;
mod local_key_file;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;

use mage_core::ports::Clock;

use crate::adapters::driven::llm_upstream::adc::AdcTokenSource;
use crate::ports::secrets::SecretStore;
use crate::ports::vault::{SecretBytes, SecretVault, VaultError};

pub use envelope::{DATA_KEY_CACHE_TTL, EnvelopeVault, SECRET_CACHE_TTL};
pub use gcp_kms::{GcpKmsKek, KMS_ENDPOINT, crc32c};
pub use local_key_file::{LOCAL_KEK_VERSION, LocalKeyFileKek};

/// Key-encryption key: wraps and unwraps DEKs. `aad` binds a wrapped DEK to the row that holds it
/// (an unwrap with another AAD fails with `Integrity`).
#[async_trait]
pub trait Kek: Send + Sync {
    /// Wrapped DEK and the KEK version that wrapped it (stored as `kek_version`).
    async fn wrap(&self, aad: &[u8], dek: &[u8]) -> Result<(Vec<u8>, String), VaultError>;
    /// The DEK of a wrap made by `kek_version` of this KEK.
    async fn unwrap(&self, aad: &[u8], wrapped: &[u8], kek_version: &str) -> Result<SecretBytes, VaultError>;
    /// The version new wraps use; `rewrap` moves every other wrap onto it.
    async fn primary_version(&self) -> Result<String, VaultError>;
}

#[async_trait]
impl<K: Kek + ?Sized> Kek for Arc<K> {
    async fn wrap(&self, aad: &[u8], dek: &[u8]) -> Result<(Vec<u8>, String), VaultError> {
        (**self).wrap(aad, dek).await
    }

    async fn unwrap(&self, aad: &[u8], wrapped: &[u8], kek_version: &str) -> Result<SecretBytes, VaultError> {
        (**self).unwrap(aad, wrapped, kek_version).await
    }

    async fn primary_version(&self) -> Result<String, VaultError> {
        (**self).primary_version().await
    }
}

/// `[server.vault]` (server.md "Config").
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VaultConfig {
    pub backend: VaultBackend,
    /// Cloud KMS CryptoKey resource name (`projects/…/locations/…/keyRings/…/cryptoKeys/…`);
    /// required by `gcp-kms`.
    #[serde(default)]
    pub platform_kek: Option<String>,
    /// 32-byte raw key file, mode 0600 (`~/` expands to `$HOME`); required by `local-key-file`,
    /// refused in production.
    #[serde(default)]
    pub local_key_file: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VaultBackend {
    GcpKms,
    LocalKeyFile,
}

impl VaultConfig {
    /// The vault this config names, over `store`. `production` is `environment = "production"`.
    /// Cloud KMS authenticates with Application Default Credentials (its own token source).
    pub fn build(&self, production: bool, store: Arc<dyn SecretStore>, clock: Arc<dyn Clock>) -> Result<Arc<dyn SecretVault>, VaultError> {
        match self.backend {
            VaultBackend::GcpKms => {
                let key = self
                    .platform_kek
                    .as_deref()
                    .ok_or_else(|| VaultError::Invalid("server.vault.platform_kek is required for backend \"gcp-kms\"".into()))?;
                let tokens = AdcTokenSource::from_environment().map_err(|e| VaultError::Invalid(e.to_string()))?;
                Ok(Arc::new(EnvelopeVault::new(store, GcpKmsKek::new(key, tokens)?, clock)))
            }
            VaultBackend::LocalKeyFile => {
                let path = self.local_key_file.as_deref().ok_or_else(|| {
                    VaultError::Invalid("server.vault.local_key_file is required for backend \"local-key-file\"".into())
                })?;
                let kek = LocalKeyFileKek::open(&expand_home(path), production)?;
                Ok(Arc::new(EnvelopeVault::new(store, kek, clock)))
            }
        }
    }
}

/// `~/rest` → `$HOME/rest`; anything else unchanged.
fn expand_home(path: &Path) -> PathBuf {
    match (path.strip_prefix("~"), std::env::var_os("HOME")) {
        (Ok(rest), Some(home)) if !home.is_empty() => PathBuf::from(home).join(rest),
        _ => path.to_path_buf(),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn config_reads_both_backends_and_rejects_unknown_ones() {
        let kms: VaultConfig = toml::from_str(
            "backend = \"gcp-kms\"\nplatform_kek = \"projects/p/locations/l/keyRings/mage/cryptoKeys/mage-kek-platform\"\n",
        )
        .expect("gcp-kms config");
        assert_eq!(kms.backend, VaultBackend::GcpKms);
        assert_eq!(kms.platform_kek.as_deref(), Some("projects/p/locations/l/keyRings/mage/cryptoKeys/mage-kek-platform"));

        let local: VaultConfig = toml::from_str("backend = \"local-key-file\"\nlocal_key_file = \"~/.config/mage/dev.key\"\n").expect("local");
        assert_eq!(local.backend, VaultBackend::LocalKeyFile);

        assert!(toml::from_str::<VaultConfig>("backend = \"aws-kms\"\n").is_err());
        assert!(toml::from_str::<VaultConfig>("backend = \"local-key-file\"\nkey = \"inline\"\n").is_err());
    }

    #[test]
    fn missing_backend_fields_are_refused_at_build() {
        let clock: Arc<dyn Clock> = Arc::new(mage_core::ports::SystemClock);
        let store: Arc<dyn SecretStore> = Arc::new(crate::testing::MemSecretStore::new(clock.clone()));
        let kms = VaultConfig { backend: VaultBackend::GcpKms, platform_kek: None, local_key_file: None };
        assert!(matches!(kms.build(false, store.clone(), clock.clone()), Err(VaultError::Invalid(_))));
        let local = VaultConfig { backend: VaultBackend::LocalKeyFile, platform_kek: None, local_key_file: None };
        assert!(matches!(local.build(false, store, clock), Err(VaultError::Invalid(_))));
    }

    #[test]
    fn home_prefix_expands() {
        let Some(home) = std::env::var_os("HOME").filter(|h| !h.is_empty()) else {
            return;
        };
        assert_eq!(expand_home(Path::new("~/.config/mage/dev.key")), PathBuf::from(home).join(".config/mage/dev.key"));
        assert_eq!(expand_home(Path::new("/etc/mage/dev.key")), PathBuf::from("/etc/mage/dev.key"));
    }
}
