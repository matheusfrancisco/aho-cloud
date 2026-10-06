//! `TicketTracker` port (intake.md "`TicketTracker`"): Linear as seen by intake. Impl:
//! `adapters::driven::linear`. Only intake calls it (other areas enqueue `notify.ticket`).

use std::collections::BTreeMap;

use async_trait::async_trait;
use mage_core::domain::OrgId;
pub use mage_core::ports::source_host::AccessToken;
use time::OffsetDateTime;
use url::Url;
use uuid::Uuid;

use crate::error::AppError;

/// Linear workflow state type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TrackerStateKind {
    Triage,
    Backlog,
    Unstarted,
    Started,
    Completed,
    Canceled,
}

impl TrackerStateKind {
    /// Linear's `WorkflowState.type` value (the names `triage_states` lists).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Triage => "triage",
            Self::Backlog => "backlog",
            Self::Unstarted => "unstarted",
            Self::Started => "started",
            Self::Completed => "completed",
            Self::Canceled => "canceled",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "triage" => Self::Triage,
            "backlog" => Self::Backlog,
            "unstarted" => Self::Unstarted,
            "started" => Self::Started,
            "completed" => Self::Completed,
            "canceled" | "cancelled" => Self::Canceled,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Author {
    Human { id: String },
    Bot { id: String },
}

impl Author {
    pub fn id(&self) -> &str {
        match self {
            Self::Human { id } | Self::Bot { id } => id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackerComment {
    pub id: String,
    pub author: Author,
    pub body: String,
    pub created_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackerAttachment {
    pub url: Url,
    pub title: String,
    /// Linear `sourceType`, e.g. "slack".
    pub source_kind: Option<String>,
    pub metadata: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackerIssue {
    pub issue_id: String,
    pub identifier: String,
    pub url: Url,
    pub workspace_id: String,
    pub team_key: String,
    pub project: Option<String>,
    pub labels: Vec<String>,
    pub state_name: String,
    pub state_kind: TrackerStateKind,
    pub title: String,
    pub description: String,
    pub created_at: OffsetDateTime,
    pub updated_at: OffsetDateTime,
    pub archived: bool,
    /// Oldest first, up to 100.
    pub comments: Vec<TrackerComment>,
    pub attachments: Vec<TrackerAttachment>,
    /// Linear customers linked via customer requests.
    pub customer_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentDraft {
    pub client_id: Uuid,
    pub body_markdown: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostedComment {
    pub id: String,
    pub url: Option<Url>,
    /// `false` = a comment with this `client_id` already existed.
    pub created: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkAttachment {
    pub url: Url,
    pub title: String,
    pub subtitle: String,
    pub metadata: BTreeMap<String, String>,
}

/// A file for Linear's private storage (`fileUpload`), e.g. the repro/fix mp4.
#[derive(Clone, PartialEq, Eq)]
pub struct TrackerUpload {
    pub file_name: String,
    pub content_type: String,
    pub bytes: Vec<u8>,
}

impl std::fmt::Debug for TrackerUpload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TrackerUpload")
            .field("file_name", &self.file_name)
            .field("content_type", &self.content_type)
            .field("bytes", &self.bytes.len())
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueQuery {
    pub team_keys: Vec<String>,
    pub updated_since: OffsetDateTime,
    pub cursor: Option<String>,
    pub page_size: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueStub {
    pub issue_id: String,
    pub updated_at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuePage {
    pub items: Vec<IssueStub>,
    pub next: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackerIdentity {
    pub user_id: String,
    pub app_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackerGrant {
    pub workspace_id: String,
    pub workspace_name: String,
    pub access_token: AccessToken,
    pub refresh_token: Option<AccessToken>,
    pub expires_at: Option<OffsetDateTime>,
    pub scopes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TrackerError {
    #[error("not found")]
    NotFound,
    #[error("conflict")]
    Conflict,
    #[error("invalid tracker request: {0}")]
    Invalid(String),
    #[error("tracker unavailable: {0}")]
    Unavailable(String),
    /// Credential revoked or expired after one refresh.
    #[error("tracker access unauthorized")]
    Unauthorized,
    #[error("tracker rate limited until {retry_at}")]
    RateLimited { retry_at: OffsetDateTime },
}

#[async_trait]
pub trait TicketTracker: Send + Sync {
    async fn get_issue(&self, org: OrgId, issue_id: &str) -> Result<TrackerIssue, TrackerError>;
    async fn updated_issues(&self, org: OrgId, q: &IssueQuery) -> Result<IssuePage, TrackerError>;
    /// Create the comment with id `client_id`; if that id already exists on the issue, return it
    /// (`created: false`) without changing it. Safe to call again after an uncertain attempt.
    async fn comment(&self, org: OrgId, issue_id: &str, c: &CommentDraft) -> Result<PostedComment, TrackerError>;
    /// Idempotent by (issue, url): an existing attachment with the same url is updated.
    async fn attach_link(&self, org: OrgId, issue_id: &str, link: &LinkAttachment) -> Result<(), TrackerError>;
    /// Move the issue to the team workflow state named `state_name`; no-op if already there.
    async fn set_state(&self, org: OrgId, issue_id: &str, state_name: &str) -> Result<(), TrackerError>;
    /// Upload a file to Linear's private storage via the `fileUpload` mutation and a server-side
    /// PUT to the pre-signed URL (copying the returned headers). Returns the asset URL to embed in
    /// comment markdown. Private to the workspace.
    async fn upload_file(&self, org: OrgId, file: &TrackerUpload) -> Result<Url, TrackerError>;
    /// Who Mage acts as in this workspace (to recognize its own comments).
    async fn identity(&self, org: OrgId) -> Result<TrackerIdentity, TrackerError>;
    /// Provider half of the OAuth connect flow (routes and `state` in server.md).
    async fn complete_install(&self, code: &str, redirect_uri: &Url, pkce_verifier: &str) -> Result<TrackerGrant, TrackerError>;
}

impl From<TrackerError> for AppError {
    fn from(err: TrackerError) -> Self {
        match err {
            TrackerError::NotFound => AppError::NotFound("Linear object".into()),
            TrackerError::Conflict => AppError::Conflict("Linear conflict".into()),
            TrackerError::Invalid(msg) => AppError::Invalid(msg),
            TrackerError::Unavailable(msg) => AppError::Unavailable(msg),
            TrackerError::Unauthorized => AppError::Forbidden("Linear access revoked".into()),
            TrackerError::RateLimited { .. } => AppError::RateLimited { retry_after_secs: None },
        }
    }
}
