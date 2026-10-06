//! `WebhookInbox` (intake.md "`WebhookInbox`", storage.md "Webhook inbox semantics"): verified
//! provider deliveries, deduplicated by delivery id and by body hash, accepted together with the
//! job that processes them.

use async_trait::async_trait;
use mage_core::domain::{JobId, OrgId};
pub use mage_core::domain::WebhookSource;
use mage_core::ports::jobs::NewJob;
use time::OffsetDateTime;

use crate::error::AppError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewDelivery {
    pub source: WebhookSource,
    /// `X-GitHub-Delivery` | `Linear-Delivery` | Slack `event_id`.
    pub delivery_id: String,
    /// `X-GitHub-Event` | `Linear-Event` | Slack `event.type`.
    pub event: String,
    /// `None` until the job resolves the tenant.
    pub org_id: Option<OrgId>,
    /// Raw bytes as verified; the adapter computes `body_sha256` and `received_at`.
    pub body: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accepted {
    New { job_id: JobId },
    Duplicate { first_seen_at: OffsetDateTime },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    pub source: WebhookSource,
    pub delivery_id: String,
    pub event: String,
    pub org_id: Option<OrgId>,
    pub received_at: OffsetDateTime,
    /// `None` after body purge.
    pub body: Option<Vec<u8>>,
    pub body_sha256: [u8; 32],
    pub outcome: Option<InboxOutcome>,
    pub processed_at: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InboxOutcome {
    Processed,
    Ignored(String),
    Rejected(String),
}

impl InboxOutcome {
    /// `webhook_inbox.outcome` value.
    pub fn as_str(&self) -> &'static str {
        match self {
            InboxOutcome::Processed => "processed",
            InboxOutcome::Ignored(_) => "ignored",
            InboxOutcome::Rejected(_) => "rejected",
        }
    }

    /// `webhook_inbox.outcome_note` value.
    pub fn note(&self) -> Option<&str> {
        match self {
            InboxOutcome::Processed => None,
            InboxOutcome::Ignored(note) | InboxOutcome::Rejected(note) => Some(note),
        }
    }

    /// Inverse of ([`Self::as_str`], [`Self::note`]).
    pub fn from_parts(outcome: &str, note: Option<String>) -> Option<InboxOutcome> {
        match outcome {
            "processed" => Some(InboxOutcome::Processed),
            "ignored" => Some(InboxOutcome::Ignored(note.unwrap_or_default())),
            "rejected" => Some(InboxOutcome::Rejected(note.unwrap_or_default())),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InboxError {
    #[error("not found")]
    NotFound,
    #[error("conflict")]
    Conflict,
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("unavailable: {0}")]
    Unavailable(String),
}

/// Batch size of `purge` statements.
pub const INBOX_PURGE_BATCH: i64 = 1000;

#[async_trait]
pub trait WebhookInbox: Send + Sync {
    /// One transaction: a new delivery (by `(source, delivery_id)` and by `(source, body_sha256)`)
    /// is stored and `job` enqueued; a duplicate stores nothing and reports the earliest matching
    /// row. `job` must be `IntakeDelivery { source, delivery_id }` of this delivery (`Invalid`).
    async fn accept(&self, d: NewDelivery, job: NewJob) -> Result<Accepted, InboxError>;
    async fn get(&self, source: WebhookSource, delivery_id: &str) -> Result<Delivery, InboxError>;
    /// Sets the tenant once; a different org later → `Conflict`.
    async fn resolve_org(&self, source: WebhookSource, delivery_id: &str, org: OrgId) -> Result<(), InboxError>;
    async fn mark_processed(&self, source: WebhookSource, delivery_id: &str, outcome: InboxOutcome) -> Result<(), InboxError>;
    /// Drops bodies of deliveries processed before `bodies_before` (unprocessed bodies stay), then
    /// deletes rows received before `rows_before`, both in batches. Returns bodies dropped + rows
    /// deleted.
    async fn purge(&self, bodies_before: OffsetDateTime, rows_before: OffsetDateTime) -> Result<u64, InboxError>;
}

impl From<InboxError> for AppError {
    fn from(err: InboxError) -> AppError {
        match err {
            InboxError::NotFound => AppError::NotFound("webhook delivery".into()),
            InboxError::Conflict => AppError::Conflict("webhook delivery".into()),
            InboxError::Invalid(msg) => AppError::Invalid(msg),
            InboxError::Unavailable(msg) => AppError::Unavailable(msg),
        }
    }
}

/// The job `accept` requires: `IntakeDelivery` naming this delivery.
pub fn check_delivery_job(d: &NewDelivery, job: &NewJob) -> Result<(), InboxError> {
    use mage_core::domain::JobPayload;
    match &job.payload {
        JobPayload::IntakeDelivery { source, delivery_id } if *source == d.source && *delivery_id == d.delivery_id => Ok(()),
        other => Err(InboxError::Invalid(format!(
            "delivery {}:{} needs its intake job, got {}",
            d.source.as_str(),
            d.delivery_id,
            other.kind().as_str()
        ))),
    }
}
