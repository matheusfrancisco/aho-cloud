//! `ChatNotifier` port (intake.md "`ChatNotifier`"): Slack threads linked from Linear issues.
//! Impl: `adapters::driven::slack`.

use async_trait::async_trait;
use mage_core::domain::{OrgId, SlackThreadRef};
pub use mage_core::ports::source_host::AccessToken;
use time::OffsetDateTime;
use url::Url;

use crate::error::AppError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChatAuthor {
    Human { user_id: String },
    /// `ours` = posted by Mage's bot user.
    Bot { id: String, ours: bool },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatMessageView {
    pub ts: String,
    pub author: ChatAuthor,
    pub text: String,
    pub at: OffsetDateTime,
    pub file_names: Vec<String>,
    /// Our message metadata key, if any.
    pub mage_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatThread {
    pub thread: SlackThreadRef,
    pub messages: Vec<ChatMessageView>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatPost {
    /// OutboundKey string; stored as Slack message metadata.
    pub key: String,
    /// Plain text; the adapter escapes it.
    pub text: String,
    /// Mage-built link and label.
    pub link: Option<(Url, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostedChat {
    pub ts: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatGrant {
    pub team_id: String,
    pub team_domain: String,
    pub bot_user_id: String,
    pub bot_token: AccessToken,
    pub scopes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ChatError {
    #[error("not found")]
    NotFound,
    /// `not_in_channel`, `missing_scope`: permanent until an admin acts.
    #[error("no access: {0}")]
    NoAccess(String),
    #[error("conflict")]
    Conflict,
    #[error("invalid chat request: {0}")]
    Invalid(String),
    #[error("chat unavailable: {0}")]
    Unavailable(String),
    #[error("chat access unauthorized")]
    Unauthorized,
    #[error("chat rate limited until {retry_at}")]
    RateLimited { retry_at: OffsetDateTime },
}

#[async_trait]
pub trait ChatNotifier: Send + Sync {
    async fn read_thread(&self, org: OrgId, thread: &SlackThreadRef, max_messages: u16) -> Result<ChatThread, ChatError>;
    async fn post_reply(&self, org: OrgId, thread: &SlackThreadRef, post: &ChatPost) -> Result<PostedChat, ChatError>;
    /// Our reply in `thread` whose metadata key equals `key`, posted at or after `since`.
    async fn find_reply(&self, org: OrgId, thread: &SlackThreadRef, key: &str, since: OffsetDateTime) -> Result<Option<PostedChat>, ChatError>;
    async fn complete_install(&self, code: &str, redirect_uri: &Url) -> Result<ChatGrant, ChatError>;
}

impl From<ChatError> for AppError {
    fn from(err: ChatError) -> Self {
        match err {
            ChatError::NotFound => AppError::NotFound("Slack object".into()),
            ChatError::NoAccess(msg) => AppError::Forbidden(msg),
            ChatError::Conflict => AppError::Conflict("Slack conflict".into()),
            ChatError::Invalid(msg) => AppError::Invalid(msg),
            ChatError::Unavailable(msg) => AppError::Unavailable(msg),
            ChatError::Unauthorized => AppError::Forbidden("Slack access revoked".into()),
            ChatError::RateLimited { .. } => AppError::RateLimited { retry_after_secs: None },
        }
    }
}
