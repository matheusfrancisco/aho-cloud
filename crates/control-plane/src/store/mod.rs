//! Durable state that is not a ledger: identities, credentials, orgs, projects, encrypted
//! secrets, upload indexes, sandboxes and their encrypted environment. Bearer tokens and device
//! codes are stored as blake3 hashes only.
//!
//! Two implementations: [`MemStore`] (tests, local runs) and [`PgStore`] (Postgres).

mod mem;
mod pg;

use async_trait::async_trait;
use cloud_api::{SandboxSettings, SandboxState};
use cloud_contract::ledger::{Approver, Stage};
use cloud_contract::policy::ReleasePolicy;
use serde::{Deserialize, Serialize};

pub use mem::MemStore;
pub use pg::PgStore;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// A uniqueness rule refused the write; the payload names what already exists.
    #[error("{0} already exists")]
    Conflict(String),
    /// A per-org limit refused the write; the payload says which.
    #[error("{0}")]
    Limit(String),
    #[error("store: {0}")]
    Backend(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserRec {
    pub id: String,
    /// Lowercase.
    pub email: String,
    pub name: String,
}

/// A pending `ahocloud login`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceCodeRec {
    /// blake3 hex of the device code.
    pub code_hash: String,
    /// `XXXX-XXXX`.
    pub user_code: String,
    pub device_name: String,
    /// Ed25519 public key, base64url.
    pub public_key: String,
    pub expires_at: u64,
    pub approved_by: Option<String>,
}

/// A bearer session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenRec {
    /// blake3 hex of the token.
    pub hash: String,
    pub user_id: String,
    pub expires_at: u64,
}

/// An Ed25519 key bound to a user by the device flow.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeviceKeyRec {
    pub key_id: String,
    pub user_id: String,
    pub name: String,
    /// 32 bytes, base64url.
    pub public_key: String,
    pub created_at: u64,
}

/// A registered WebAuthn credential (ES256).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PasskeyRec {
    /// base64url of the raw credential id.
    pub credential_id: String,
    pub user_id: String,
    /// SEC1 uncompressed P-256 point.
    pub public_key: Vec<u8>,
    pub sign_count: u32,
    pub created_at: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CeremonyKind {
    Register,
    Login,
}

impl CeremonyKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CeremonyKind::Register => "register",
            CeremonyKind::Login => "login",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "register" => Some(CeremonyKind::Register),
            "login" => Some(CeremonyKind::Login),
            _ => None,
        }
    }
}

/// One outstanding WebAuthn challenge; consumed on first use.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CeremonyRec {
    pub id: String,
    pub kind: CeremonyKind,
    pub challenge: Vec<u8>,
    /// Existing or to-be-created user; empty for a login to an unknown email.
    pub user_id: String,
    pub email: String,
    pub name: String,
    /// Registration creates the user on finish.
    pub new_user: bool,
    pub expires_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OrgRec {
    pub slug: String,
    pub name: String,
    pub policy: ReleasePolicy,
    /// The owner approval of the current policy, replayed into new project ledgers.
    pub policy_approver: Option<Approver>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemberRec {
    pub org: String,
    pub user_id: String,
    pub owner: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectRec {
    pub slug: String,
    pub org: String,
    pub created_at: u64,
    /// Deleted projects keep their slug forever so their hostnames stay tombstoned.
    pub deleted_at: Option<u64>,
}

/// One secret value for one stage, sealed with [`crate::crypto::SecretBox`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecretRec {
    pub project: String,
    pub name: String,
    pub stage: Stage,
    pub ciphertext: Vec<u8>,
    pub fingerprint: String,
    /// Argon2id PHC of the value, for the edge password secret only.
    pub edge_phc: Option<String>,
    pub updated_at: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlobKind {
    Archive,
    Session,
}

impl BlobKind {
    pub fn as_str(self) -> &'static str {
        match self {
            BlobKind::Archive => "archive",
            BlobKind::Session => "session",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "archive" => Some(BlobKind::Archive),
            "session" => Some(BlobKind::Session),
            _ => None,
        }
    }
}

/// An upload made to a project.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlobRec {
    pub project: String,
    /// `blake3:<hex>`.
    pub hash: String,
    pub kind: BlobKind,
    pub size: u64,
    pub created_at: u64,
}

/// A session log whose chain verified at upload, up to `head_hash`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionRec {
    pub project: String,
    pub session_id: String,
    pub head_hash: String,
    pub blob_hash: String,
    pub entries: u64,
    pub created_at: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum BuildState {
    Pending,
    Built,
    Failed { reason: String },
}

/// What the control plane knows about a release beyond the ledger.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReleaseMeta {
    pub project: String,
    pub release: String,
    pub edge_toml: Option<String>,
    pub build: BuildState,
    pub build_log: String,
    pub created_at: u64,
}

/// A sandbox: a VM with its own disk, owned by an org. Its slug shares one namespace with
/// project slugs (deleted projects included).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SandboxRec {
    pub slug: String,
    pub org: String,
    /// User id.
    pub created_by: String,
    pub created_at: u64,
    /// `<repo>@sha256:<hex>` it boots from.
    pub image: String,
    pub settings: SandboxSettings,
    /// As the runtime last reported it.
    pub state: SandboxState,
    pub error: Option<String>,
    /// When the running VM is stopped.
    pub stops_at: Option<u64>,
    pub cpus: u8,
    pub mem_mib: u32,
    pub disk_mib: u32,
}

/// One environment variable of a sandbox, sealed with [`crate::crypto::SecretBox::seal_env`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvVarRec {
    pub sandbox: String,
    pub name: String,
    pub ciphertext: Vec<u8>,
    pub fingerprint: String,
    pub updated_at: u64,
}

/// The stored spelling of a [`SandboxState`] (its JSON form).
pub fn sandbox_state_str(state: SandboxState) -> &'static str {
    match state {
        SandboxState::Starting => "starting",
        SandboxState::Running => "running",
        SandboxState::Stopped => "stopped",
        SandboxState::Failed => "failed",
    }
}

pub fn parse_sandbox_state(s: &str) -> Option<SandboxState> {
    match s {
        "starting" => Some(SandboxState::Starting),
        "running" => Some(SandboxState::Running),
        "stopped" => Some(SandboxState::Stopped),
        "failed" => Some(SandboxState::Failed),
        _ => None,
    }
}

/// What [`StoreError::Limit`] says when `org` already has `max` sandboxes.
fn limit_message(org: &str, max: usize) -> String {
    format!("org {org} already has the most sandboxes it may have ({max}); delete one first")
}

/// The storage port. Every method is a single atomic step.
#[async_trait]
pub trait Store: Send + Sync {
    /// Conflict when the id or email exists.
    async fn create_user(&self, user: &UserRec) -> Result<(), StoreError>;
    async fn user(&self, id: &str) -> Result<Option<UserRec>, StoreError>;
    async fn user_by_email(&self, email: &str) -> Result<Option<UserRec>, StoreError>;

    /// Conflict when the user code is taken.
    async fn put_device_code(&self, code: &DeviceCodeRec) -> Result<(), StoreError>;
    async fn device_code(&self, code_hash: &str) -> Result<Option<DeviceCodeRec>, StoreError>;
    async fn device_code_by_user_code(&self, user_code: &str) -> Result<Option<DeviceCodeRec>, StoreError>;
    /// Binds the code to `user_id` unless already bound; `false` when it was.
    async fn approve_device_code(&self, code_hash: &str, user_id: &str) -> Result<bool, StoreError>;
    /// Deletes and returns the code: redemption happens once.
    async fn take_device_code(&self, code_hash: &str) -> Result<Option<DeviceCodeRec>, StoreError>;

    async fn put_token(&self, token: &TokenRec) -> Result<(), StoreError>;
    async fn token(&self, hash: &str) -> Result<Option<TokenRec>, StoreError>;

    async fn put_device_key(&self, key: &DeviceKeyRec) -> Result<(), StoreError>;
    async fn device_key(&self, key_id: &str) -> Result<Option<DeviceKeyRec>, StoreError>;
    async fn device_keys_for_user(&self, user_id: &str) -> Result<Vec<DeviceKeyRec>, StoreError>;

    /// Conflict when the credential id is registered.
    async fn put_passkey(&self, passkey: &PasskeyRec) -> Result<(), StoreError>;
    async fn passkey(&self, credential_id: &str) -> Result<Option<PasskeyRec>, StoreError>;
    async fn passkeys_for_user(&self, user_id: &str) -> Result<Vec<PasskeyRec>, StoreError>;
    /// Compare-and-set of the signature counter; `false` when `old` is stale.
    async fn update_sign_count(&self, credential_id: &str, old: u32, new: u32) -> Result<bool, StoreError>;

    async fn put_ceremony(&self, ceremony: &CeremonyRec) -> Result<(), StoreError>;
    /// Deletes and returns the ceremony: a challenge is answered once.
    async fn take_ceremony(&self, id: &str) -> Result<Option<CeremonyRec>, StoreError>;

    /// Creates the org with `owner` as its first owner. Conflict when the slug exists.
    async fn create_org(&self, org: &OrgRec, owner: &str) -> Result<(), StoreError>;
    async fn org(&self, slug: &str) -> Result<Option<OrgRec>, StoreError>;
    async fn set_org_policy(&self, slug: &str, policy: &ReleasePolicy, approver: &Approver) -> Result<(), StoreError>;
    /// Inserts or updates a membership.
    async fn put_member(&self, member: &MemberRec) -> Result<(), StoreError>;
    async fn member(&self, org: &str, user_id: &str) -> Result<Option<MemberRec>, StoreError>;
    async fn members(&self, org: &str) -> Result<Vec<MemberRec>, StoreError>;
    async fn memberships(&self, user_id: &str) -> Result<Vec<MemberRec>, StoreError>;

    /// Conflict when the slug was ever used by a project, or is a sandbox's.
    async fn create_project(&self, project: &ProjectRec) -> Result<(), StoreError>;
    /// Includes deleted projects.
    async fn project(&self, slug: &str) -> Result<Option<ProjectRec>, StoreError>;
    /// Every project, deleted ones included.
    async fn projects(&self) -> Result<Vec<ProjectRec>, StoreError>;
    async fn delete_project(&self, slug: &str, at: u64) -> Result<(), StoreError>;

    /// Inserts or replaces the value of `(project, name, stage)`.
    async fn put_secret(&self, secret: &SecretRec) -> Result<(), StoreError>;
    async fn secrets(&self, project: &str) -> Result<Vec<SecretRec>, StoreError>;

    async fn put_blob(&self, blob: &BlobRec) -> Result<(), StoreError>;
    async fn blob(&self, project: &str, hash: &str) -> Result<Option<BlobRec>, StoreError>;
    /// Whether any project uploaded an archive with this hash.
    async fn archive_exists(&self, hash: &str) -> Result<bool, StoreError>;

    async fn put_session(&self, session: &SessionRec) -> Result<(), StoreError>;
    async fn session(&self, project: &str, session_id: &str, head_hash: &str) -> Result<Option<SessionRec>, StoreError>;
    async fn sessions(&self, project: &str) -> Result<Vec<SessionRec>, StoreError>;

    /// Inserts or replaces.
    async fn put_release_meta(&self, meta: &ReleaseMeta) -> Result<(), StoreError>;
    async fn release_meta(&self, project: &str, release: &str) -> Result<Option<ReleaseMeta>, StoreError>;

    /// Conflict when the slug is a sandbox's or was ever a project's; [`StoreError::Limit`] when
    /// the org already has `max_per_org` sandboxes.
    async fn create_sandbox(&self, sandbox: &SandboxRec, max_per_org: usize) -> Result<(), StoreError>;
    async fn sandbox(&self, slug: &str) -> Result<Option<SandboxRec>, StoreError>;
    /// Ordered by slug.
    async fn org_sandboxes(&self, org: &str) -> Result<Vec<SandboxRec>, StoreError>;
    /// Every sandbox, ordered by slug.
    async fn sandboxes(&self) -> Result<Vec<SandboxRec>, StoreError>;
    /// Replaces an existing sandbox's record; a deleted one stays deleted.
    async fn update_sandbox(&self, sandbox: &SandboxRec) -> Result<(), StoreError>;
    /// Removes the sandbox and its environment.
    async fn delete_sandbox(&self, slug: &str) -> Result<(), StoreError>;
    /// Inserts or replaces `(sandbox, name)`.
    async fn put_sandbox_env(&self, var: &EnvVarRec) -> Result<(), StoreError>;
    /// Ordered by name.
    async fn sandbox_env(&self, sandbox: &str) -> Result<Vec<EnvVarRec>, StoreError>;
    /// `false` when there was no such variable.
    async fn delete_sandbox_env(&self, sandbox: &str, name: &str) -> Result<bool, StoreError>;

    /// Small documents (sealed registry snapshot, sites index).
    async fn kv_get(&self, key: &str) -> Result<Option<String>, StoreError>;
    async fn kv_put(&self, key: &str, value: &str) -> Result<(), StoreError>;
}
