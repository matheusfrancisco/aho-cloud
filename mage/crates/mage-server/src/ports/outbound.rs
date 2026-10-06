//! `OutboundLog` (intake.md "`OutboundLog`"): one row per outbound Linear comment / Slack reply,
//! so `notify.ticket` retries reuse the pre-allocated client id and never post twice.

use async_trait::async_trait;
use mage_core::domain::{OrgId, TicketId};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::AppError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OutboundTarget {
    LinearComment,
    SlackReply,
}

impl OutboundTarget {
    /// `outbound_messages.target` value.
    pub const fn as_str(self) -> &'static str {
        match self {
            OutboundTarget::LinearComment => "linear_comment",
            OutboundTarget::SlackReply => "slack_reply",
        }
    }

    pub fn parse(s: &str) -> Option<OutboundTarget> {
        [OutboundTarget::LinearComment, OutboundTarget::SlackReply].into_iter().find(|v| v.as_str() == s)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct OutboundKey {
    pub ticket_id: TicketId,
    pub target: OutboundTarget,
    pub notice_key: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OutboundState {
    /// `uncertain`: a send attempt may have reached the provider (look before resending).
    Pending { uncertain: bool },
    Sent,
    Skipped { reason: String },
    Failed { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundRecord {
    pub key: OutboundKey,
    pub client_id: Uuid,
    pub state: OutboundState,
    pub attempts: u16,
    pub body_sha256: [u8; 32],
    pub external_id: Option<String>,
    pub last_error: Option<String>,
    pub created_at: OffsetDateTime,
    pub sent_at: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OutboundError {
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
pub trait OutboundLog: Send + Sync {
    /// Insert if absent and return the row; an existing row keeps its original client_id.
    async fn begin(&self, org: OrgId, key: &OutboundKey, client_id: Uuid, body_sha256: [u8; 32]) -> Result<OutboundRecord, OutboundError>;
    /// Pending rows only (`Conflict` otherwise): attempts + 1, `uncertain`, `last_error = error`.
    async fn mark_attempt(&self, org: OrgId, key: &OutboundKey, uncertain: bool, error: Option<&str>) -> Result<(), OutboundError>;
    /// Pending → Sent; repeating it with the same external id is a no-op; otherwise `Conflict`.
    async fn mark_sent(&self, org: OrgId, key: &OutboundKey, external_id: &str, at: OffsetDateTime) -> Result<(), OutboundError>;
    /// Pending → Skipped (reason kept in `last_error`); repeating it is a no-op; otherwise `Conflict`.
    async fn mark_skipped(&self, org: OrgId, key: &OutboundKey, reason: &str) -> Result<(), OutboundError>;
    /// Pending → Failed (reason kept in `last_error`); repeating it is a no-op; otherwise `Conflict`.
    async fn mark_failed(&self, org: OrgId, key: &OutboundKey, reason: &str) -> Result<(), OutboundError>;
    /// Oldest first.
    async fn list_for_ticket(&self, org: OrgId, ticket: TicketId) -> Result<Vec<OutboundRecord>, OutboundError>;
    /// Loop prevention: is this Linear comment id / Slack ts one of ours?
    async fn find_by_external(&self, org: OrgId, target: OutboundTarget, external_id: &str) -> Result<Option<OutboundRecord>, OutboundError>;
}

impl From<OutboundError> for AppError {
    fn from(err: OutboundError) -> AppError {
        match err {
            OutboundError::NotFound => AppError::NotFound("outbound message".into()),
            OutboundError::Conflict => AppError::Conflict("outbound message state".into()),
            OutboundError::Invalid(msg) => AppError::Invalid(msg),
            OutboundError::Unavailable(msg) => AppError::Unavailable(msg),
        }
    }
}
