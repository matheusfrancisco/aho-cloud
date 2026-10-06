//! Job-level failure of an intake use case: retry (at a provider's `retry_at`, or with the
//! queue's backoff) or give up. HTTP-facing use cases return `AppError` instead.

use mage_core::ports::{JobError, RepoStoreError};
use mage_core::ports::source_host::SourceHostError;
use time::OffsetDateTime;

use crate::error::AppError;
use crate::ports::chat::ChatError;
use crate::ports::deployments::DeploymentError;
use crate::ports::inbox::InboxError;
use crate::ports::org::OrgStoreError;
use crate::ports::outbound::OutboundError;
use crate::ports::ticket::TicketStoreError;
use crate::ports::runs::RunStoreError;
use crate::ports::tracker::TrackerError;
use crate::ports::vault::VaultError;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IntakeError {
    /// Transient: `at = Some` for a provider rate limit, `None` for the queue's backoff.
    #[error("retry: {reason}")]
    Retry { reason: String, at: Option<OffsetDateTime> },
    /// Retrying cannot help (revoked credential, invalid data).
    #[error("permanent: {0}")]
    Permanent(String),
}

impl IntakeError {
    pub fn retry(reason: impl Into<String>) -> Self {
        Self::Retry { reason: reason.into(), at: None }
    }

    pub fn retry_at(reason: impl Into<String>, at: OffsetDateTime) -> Self {
        Self::Retry { reason: reason.into(), at: Some(at) }
    }

    pub fn permanent(reason: impl Into<String>) -> Self {
        Self::Permanent(reason.into())
    }
}

impl From<IntakeError> for AppError {
    fn from(err: IntakeError) -> Self {
        match err {
            IntakeError::Retry { reason, .. } => AppError::Unavailable(reason),
            IntakeError::Permanent(reason) => AppError::Invalid(reason),
        }
    }
}

impl From<SourceHostError> for IntakeError {
    fn from(err: SourceHostError) -> Self {
        match err {
            SourceHostError::RateLimited { retry_at } => Self::retry_at("GitHub rate limited", retry_at),
            SourceHostError::Unavailable(msg) => Self::retry(format!("GitHub unavailable: {msg}")),
            SourceHostError::Conflict => Self::retry("GitHub conflict"),
            SourceHostError::Unauthorized => Self::permanent("GitHub installation access revoked"),
            SourceHostError::NotFound => Self::permanent("GitHub object not found"),
            SourceHostError::Invalid(msg) => Self::permanent(format!("GitHub rejected the request: {msg}")),
        }
    }
}

impl From<TrackerError> for IntakeError {
    fn from(err: TrackerError) -> Self {
        match err {
            TrackerError::RateLimited { retry_at } => Self::retry_at("Linear rate limited", retry_at),
            TrackerError::Unavailable(msg) => Self::retry(format!("Linear unavailable: {msg}")),
            TrackerError::Conflict => Self::retry("Linear conflict"),
            TrackerError::Unauthorized => Self::permanent("Linear access revoked"),
            TrackerError::NotFound => Self::permanent("Linear object not found"),
            TrackerError::Invalid(msg) => Self::permanent(format!("Linear rejected the request: {msg}")),
        }
    }
}

impl From<ChatError> for IntakeError {
    fn from(err: ChatError) -> Self {
        match err {
            ChatError::RateLimited { retry_at } => Self::retry_at("Slack rate limited", retry_at),
            ChatError::Unavailable(msg) => Self::retry(format!("Slack unavailable: {msg}")),
            ChatError::Conflict => Self::retry("Slack conflict"),
            ChatError::Unauthorized => Self::permanent("Slack access revoked"),
            ChatError::NotFound => Self::permanent("Slack object not found"),
            ChatError::NoAccess(msg) => Self::permanent(format!("Slack access missing: {msg}")),
            ChatError::Invalid(msg) => Self::permanent(format!("Slack rejected the request: {msg}")),
        }
    }
}

impl From<DeploymentError> for IntakeError {
    fn from(err: DeploymentError) -> Self {
        match err {
            DeploymentError::RateLimited { retry_at } => Self::retry_at("deployment source rate limited", retry_at),
            DeploymentError::Unavailable(msg) => Self::retry(format!("deployment source unavailable: {msg}")),
            DeploymentError::Conflict => Self::retry("deployment source conflict"),
            DeploymentError::Unauthorized => Self::permanent("deployment source access revoked"),
            DeploymentError::NotFound => Self::permanent("deployment source object not found"),
            DeploymentError::Invalid(msg) => Self::permanent(format!("deployment lookup invalid: {msg}")),
        }
    }
}

/// Store errors: `Conflict` (optimistic concurrency) and `Unavailable` retry; the rest is a bug or
/// a vanished row, retried with backoff a bounded number of times by the queue.
macro_rules! store_error {
    ($ty:ty, $name:literal) => {
        impl From<$ty> for IntakeError {
            fn from(err: $ty) -> Self {
                Self::retry(format!(concat!($name, ": {}"), err))
            }
        }
    };
}

store_error!(RepoStoreError, "repo store");
store_error!(JobError, "job queue");
store_error!(InboxError, "webhook inbox");
store_error!(OrgStoreError, "org store");
store_error!(TicketStoreError, "ticket store");
store_error!(OutboundError, "outbound log");
store_error!(VaultError, "secret vault");
store_error!(RunStoreError, "run store");