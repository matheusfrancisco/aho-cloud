//! `SecretStore` (storage.md "`SecretStore`"): sealed secret rows and per-org data keys persisted
//! for `SecretVault` (server.md), which does all cryptography; this port never sees plaintext.

use async_trait::async_trait;
use mage_core::domain::OrgId;
use time::OffsetDateTime;

use crate::error::AppError;
use crate::ports::audit::Actor;
use crate::ports::vault::{SecretName, SecretScope, SecretState, SecretVersion};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedSecret {
    pub scope: SecretScope,
    pub name: SecretName,
    pub version: SecretVersion,
    pub state: SecretState,
    /// `None` once Destroyed.
    pub wrapped_dek: Option<Vec<u8>>,
    pub kek_version: String,
    pub nonce: [u8; 12],
    /// `None` once Destroyed.
    pub ciphertext: Option<Vec<u8>>,
    pub fingerprint: String,
    pub created_by: Actor,
    pub created_at: OffsetDateTime,
    pub state_changed_at: OffsetDateTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DataKeyState {
    Active,
    Retired,
    Destroyed,
}

impl DataKeyState {
    /// `data_keys.state` value.
    pub const fn as_str(self) -> &'static str {
        match self {
            DataKeyState::Active => "active",
            DataKeyState::Retired => "retired",
            DataKeyState::Destroyed => "destroyed",
        }
    }

    pub fn parse(s: &str) -> Option<DataKeyState> {
        [DataKeyState::Active, DataKeyState::Retired, DataKeyState::Destroyed].into_iter().find(|v| v.as_str() == s)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SealedDataKey {
    pub org: OrgId,
    pub version: u32,
    /// `None` once Destroyed.
    pub wrapped_dek: Option<Vec<u8>>,
    pub kek_version: String,
    pub state: DataKeyState,
    pub created_at: OffsetDateTime,
}

/// A wrapped DEK: a secret version or an org data key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WrapRef {
    Secret { scope: SecretScope, name: SecretName, version: SecretVersion },
    DataKey { org: OrgId, version: u32 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrapInfo {
    pub r: WrapRef,
    pub kek_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SecretStoreError {
    #[error("not found")]
    NotFound,
    #[error("conflict")]
    Conflict,
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("unavailable: {0}")]
    Unavailable(String),
}

#[async_trait]
pub trait SecretStore: Send + Sync {
    /// Max stored version + 1 (1 for a new name).
    async fn next_secret_version(&self, scope: &SecretScope, name: &SecretName) -> Result<SecretVersion, SecretStoreError>;
    /// `Conflict` unless `version = max + 1`; state `Active` retires the previous Active in the same
    /// transaction. A Destroyed row, or a live row without DEK / ciphertext → `Invalid`.
    async fn insert_secret(&self, s: &SealedSecret) -> Result<(), SecretStoreError>;
    async fn active_secret(&self, scope: &SecretScope, name: &SecretName) -> Result<Option<SealedSecret>, SecretStoreError>;
    async fn secret_version(&self, scope: &SecretScope, name: &SecretName, version: SecretVersion) -> Result<SealedSecret, SecretStoreError>;
    /// Oldest version first.
    async fn secret_versions(&self, scope: &SecretScope, name: &SecretName) -> Result<Vec<SealedSecret>, SecretStoreError>;
    /// Every version of every name in the scope, ordered by (name, version).
    async fn list_secrets(&self, scope: &SecretScope) -> Result<Vec<SealedSecret>, SecretStoreError>;
    /// Transitions per `SecretState::can_become` (same state: no-op); `Staged → Active` retires the
    /// previous Active; `Destroyed` wipes the DEK and ciphertext. Others → `Invalid`.
    async fn set_secret_state(&self, scope: &SecretScope, name: &SecretName, version: SecretVersion, state: SecretState)
    -> Result<SealedSecret, SecretStoreError>;

    /// `Conflict` if the version exists; `Active` retires the previous Active.
    async fn insert_data_key(&self, key: &SealedDataKey) -> Result<(), SecretStoreError>;
    async fn active_data_key(&self, org: OrgId) -> Result<Option<SealedDataKey>, SecretStoreError>;
    async fn data_key(&self, org: OrgId, version: u32) -> Result<SealedDataKey, SecretStoreError>;

    /// Non-destroyed wraps after `after` (secrets: platform scope first, then by org, name,
    /// version; then data keys by org, version), at most `limit` (`vault.rewrap`).
    async fn list_wraps(&self, after: Option<&WrapRef>, limit: u32) -> Result<Vec<WrapInfo>, SecretStoreError>;
    /// CAS on the old `kek_version`; a destroyed or missing wrap → `NotFound`.
    async fn rewrap(&self, r: &WrapRef, expect_kek_version: &str, wrapped_dek: &[u8], kek_version: &str) -> Result<(), SecretStoreError>;
    /// Org deletion: deletes every secret and data key row of the org; returns rows deleted.
    async fn destroy_org(&self, org: OrgId) -> Result<u64, SecretStoreError>;
}

impl From<SecretStoreError> for AppError {
    fn from(err: SecretStoreError) -> AppError {
        match err {
            SecretStoreError::NotFound => AppError::NotFound("secret".into()),
            SecretStoreError::Conflict => AppError::Conflict("secret version".into()),
            SecretStoreError::Invalid(msg) => AppError::Invalid(msg),
            SecretStoreError::Unavailable(msg) => AppError::Unavailable(msg),
        }
    }
}

/// The checks `insert_secret` makes before writing.
pub fn check_new_secret(s: &SealedSecret) -> Result<(), SecretStoreError> {
    if s.state == SecretState::Destroyed {
        return Err(SecretStoreError::Invalid("a new secret version cannot be destroyed".into()));
    }
    if s.wrapped_dek.is_none() || s.ciphertext.is_none() {
        return Err(SecretStoreError::Invalid("a live secret version needs a wrapped DEK and ciphertext".into()));
    }
    if s.version.0 == 0 {
        return Err(SecretStoreError::Invalid("secret versions start at 1".into()));
    }
    Ok(())
}

/// The checks `insert_data_key` makes before writing.
pub fn check_new_data_key(k: &SealedDataKey) -> Result<(), SecretStoreError> {
    if k.state == DataKeyState::Destroyed || k.wrapped_dek.is_none() {
        return Err(SecretStoreError::Invalid("a new data key needs a wrapped DEK and cannot be destroyed".into()));
    }
    Ok(())
}
