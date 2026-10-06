//! `SourceHostAdmin` port (intake.md "`SourceHostAdmin`"): server-only GitHub App operations that
//! need server-only secrets (OAuth client secret, Fix App key). Impl:
//! `adapters::driven::github::admin`.

use async_trait::async_trait;
use mage_core::domain::InstallationId;
use mage_core::ports::source_host::{InstallationInfo, SourceHostError};
use time::OffsetDateTime;

use crate::error::AppError;

/// Which GitHub App: the read-only app (`SourceHost`) or the Mage Fix App (fix.md F3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GithubApp {
    Read,
    Fix,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookDelivery {
    pub id: u64,
    pub guid: String,
    pub delivered_at: OffsetDateTime,
    pub redelivery: bool,
    pub status_code: u16,
    pub event: String,
    pub action: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookDeliveryPage {
    pub items: Vec<HookDelivery>,
    pub next: Option<String>,
}

#[async_trait]
pub trait SourceHostAdmin: Send + Sync {
    /// Exchange the install-time OAuth `code` for a user token and confirm that user can see
    /// `inst` (GET /user/installations). Rejects a spoofed `installation_id` with `Invalid`.
    async fn complete_install(&self, app: GithubApp, inst: InstallationId, code: &str) -> Result<InstallationInfo, SourceHostError>;
    /// App-level webhook deliveries, newest first (app JWT).
    async fn hook_deliveries(&self, app: GithubApp, cursor: Option<String>, per_page: u8) -> Result<HookDeliveryPage, SourceHostError>;
    async fn redeliver(&self, app: GithubApp, delivery_id: u64) -> Result<(), SourceHostError>;
    /// Drop every cached installation token (and conditional-request cache entry) of `inst` for
    /// `app` (installation lifecycle events: deleted, suspended, new permissions accepted).
    /// Addition to intake.md: the token cache lives in `mage_infra::github`, which the app layer
    /// cannot reach otherwise.
    fn evict_installation(&self, app: GithubApp, inst: InstallationId);
}

/// Shared by every server use case that calls `SourceHost` / `SourceHostAdmin`. Rate limits keep
/// their deadline only in jobs, which match on the port error directly.
impl From<SourceHostError> for AppError {
    fn from(err: SourceHostError) -> Self {
        match err {
            SourceHostError::NotFound => AppError::NotFound("source host object".into()),
            SourceHostError::Conflict => AppError::Conflict("source host conflict".into()),
            SourceHostError::Invalid(msg) => AppError::Invalid(msg),
            SourceHostError::Unavailable(msg) => AppError::Unavailable(msg),
            SourceHostError::Unauthorized => AppError::Forbidden("GitHub installation access revoked".into()),
            SourceHostError::RateLimited { .. } => AppError::RateLimited { retry_after_secs: None },
        }
    }
}
