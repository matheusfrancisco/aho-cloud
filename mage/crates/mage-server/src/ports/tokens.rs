//! `TokenStore` (storage.md "`TokenStore`"): the server-side half of authentication — UI sessions
//! and one-time OAuth / install states (hashes only), one-time runner bootstrap tokens, runner
//! identities, and revoked `jti`s. Signed tokens themselves are verified statelessly (server.md).

use std::net::IpAddr;

use async_trait::async_trait;
use mage_core::domain::{IntegrationKind, OrgId, RunnerId, UserId};
use mage_core::wire::runner::Capabilities as RunnerCapabilities;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::error::AppError;
use crate::ports::audit::Actor;

/// Signed-token id (128 bits; the claims' uuid bytes).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Jti(pub [u8; 16]);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSession {
    /// SHA-256 of the cookie value.
    pub id_hash: [u8; 32],
    pub user_id: UserId,
    pub csrf_secret: Vec<u8>,
    pub expires_at: OffsetDateTime,
    pub ip: Option<IpAddr>,
    pub user_agent: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Session {
    pub id_hash: [u8; 32],
    pub user_id: UserId,
    pub csrf_secret: Vec<u8>,
    pub created_at: OffsetDateTime,
    pub last_seen_at: OffsetDateTime,
    pub expires_at: OffsetDateTime,
    pub ip: Option<IpAddr>,
    pub user_agent: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OAuthPurpose {
    Login,
    /// Integration install bound to an org and provider.
    Install { org: OrgId, provider: IntegrationKind },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OAuthState {
    /// SHA-256 of the `state` value.
    pub state_hash: [u8; 32],
    pub purpose: OAuthPurpose,
    /// The session user an install state is bound to.
    pub user: Option<UserId>,
    pub pkce_verifier: Option<String>,
    pub return_to: Option<String>,
    pub expires_at: OffsetDateTime,
}

/// Which orgs' runs a runner may lease. Recorded with the bootstrap token by the operator, never
/// taken from the runner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "orgs", rename_all = "snake_case")]
pub enum RunnerScope {
    Shared,
    Orgs(Vec<OrgId>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewBootstrapToken {
    /// SHA-256 of the token.
    pub token_hash: [u8; 32],
    /// The one runner name this token may register.
    pub name: String,
    pub scope: RunnerScope,
    pub expires_at: OffsetDateTime,
    pub created_by: Actor,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunnerRegistration {
    /// Used only when the name is new.
    pub id: RunnerId,
    /// Must equal the bootstrap token's name.
    pub name: String,
    /// Ed25519 public key.
    pub public_key: [u8; 32],
    pub capabilities: RunnerCapabilities,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RunnerStatus {
    Active,
    Draining,
    Revoked,
}

impl RunnerStatus {
    /// `runners.status` value.
    pub const fn as_str(self) -> &'static str {
        match self {
            RunnerStatus::Active => "active",
            RunnerStatus::Draining => "draining",
            RunnerStatus::Revoked => "revoked",
        }
    }

    pub fn parse(s: &str) -> Option<RunnerStatus> {
        [RunnerStatus::Active, RunnerStatus::Draining, RunnerStatus::Revoked].into_iter().find(|v| v.as_str() == s)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Runner {
    pub id: RunnerId,
    pub name: String,
    pub scope: RunnerScope,
    pub status: RunnerStatus,
    pub public_key: [u8; 32],
    /// Bumped on re-bootstrap and on revocation; runner tokens carry it.
    pub generation: u32,
    pub registered_at: OffsetDateTime,
    pub last_seen_at: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Revocation {
    pub jti: Jti,
    pub org_id: Option<OrgId>,
    pub reason: String,
    /// The revoked token's `exp`; the row is purged after it.
    pub expires_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TokenError {
    #[error("not found")]
    NotFound,
    #[error("conflict")]
    Conflict,
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("unavailable: {0}")]
    Unavailable(String),
}

/// How long after its first use a bootstrap token still returns the same runner to a retry with
/// the same public key (lost registration response).
pub const BOOTSTRAP_RETRY_WINDOW: time::Duration = time::Duration::minutes(10);

#[async_trait]
pub trait TokenStore: Send + Sync {
    /// `Conflict` if the hash exists.
    async fn create_session(&self, s: &NewSession) -> Result<(), TokenError>;
    /// `None` if missing or expired.
    async fn session(&self, id_hash: &[u8; 32]) -> Result<Option<Session>, TokenError>;
    /// Sets `last_seen_at` = now and the new expiry; missing or expired → `NotFound`.
    async fn touch_session(&self, id_hash: &[u8; 32], expires_at: OffsetDateTime) -> Result<(), TokenError>;
    /// Idempotent.
    async fn delete_session(&self, id_hash: &[u8; 32]) -> Result<(), TokenError>;
    /// Returns sessions deleted.
    async fn delete_user_sessions(&self, user: UserId) -> Result<u64, TokenError>;

    /// `Conflict` if the hash exists.
    async fn put_oauth_state(&self, s: &OAuthState) -> Result<(), TokenError>;
    /// One-time: deletes and returns the state; `None` if missing or expired.
    async fn take_oauth_state(&self, state_hash: &[u8; 32]) -> Result<Option<OAuthState>, TokenError>;

    /// `Conflict` if the hash exists.
    async fn create_bootstrap(&self, b: &NewBootstrapToken) -> Result<(), TokenError>;
    /// One transaction (storage.md): consumes the token, inserts the runner with the token's scope
    /// or re-bootstraps an existing name (new key, generation + 1, Active). A used token returns
    /// the same runner to a retry with the same key within [`BOOTSTRAP_RETRY_WINDOW`]; anything
    /// else about the token → `Invalid("bootstrap token invalid")`.
    async fn register_runner(&self, bootstrap_hash: &[u8; 32], reg: &RunnerRegistration) -> Result<Runner, TokenError>;
    async fn runner(&self, id: RunnerId) -> Result<Runner, TokenError>;
    /// Entering `Revoked` bumps the generation.
    async fn set_runner_status(&self, id: RunnerId, status: RunnerStatus) -> Result<Runner, TokenError>;

    /// Idempotent.
    async fn revoke(&self, r: &Revocation) -> Result<(), TokenError>;
    async fn is_revoked(&self, jti: &Jti) -> Result<bool, TokenError>;
    /// Deletes up to `limit` expired rows of each kind (sessions, states, bootstrap tokens,
    /// revocations); returns rows deleted.
    async fn purge_expired(&self, limit: u32) -> Result<u64, TokenError>;
}

impl From<TokenError> for AppError {
    fn from(err: TokenError) -> AppError {
        match err {
            TokenError::NotFound => AppError::NotFound("token record".into()),
            TokenError::Conflict => AppError::Conflict("token record exists".into()),
            TokenError::Invalid(msg) => AppError::Invalid(msg),
            TokenError::Unavailable(msg) => AppError::Unavailable(msg),
        }
    }
}

/// The error every unusable bootstrap token gets (no detail for the caller).
pub fn bootstrap_invalid() -> TokenError {
    TokenError::Invalid("bootstrap token invalid".into())
}
