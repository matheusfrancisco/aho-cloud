//! `WebhookDecoder` port (intake.md "`WebhookDecoder`"): provider JSON → provider-neutral
//! [`HookEvent`]. Synchronous and pure; implementations live next to each provider's API types
//! (`adapters::driven::{github, linear, slack}`).

use mage_core::domain::{CommitSha, InstallationId};
pub use mage_core::domain::WebhookSource;
use mage_core::ports::source_host::RepoCoords;
use time::OffsetDateTime;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LinearAction {
    Create,
    Update,
    Remove,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum InstallationAction {
    Created,
    Deleted,
    Suspend,
    Unsuspend,
    NewPermissionsAccepted,
    Other(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum RepoAction {
    Renamed,
    Edited,
    Archived,
    Unarchived,
    Deleted,
    Transferred,
    Other(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeploymentState {
    Success,
    Failure,
    Error,
    Inactive,
    InProgress,
    Queued,
    Pending,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookEvent {
    GithubPush {
        installation: InstallationId,
        repo_external_id: u64,
        repo: RepoCoords,
        git_ref: String,
        deleted: bool,
    },
    GithubInstallation {
        installation: InstallationId,
        action: InstallationAction,
        account_login: String,
    },
    GithubInstallationRepos {
        installation: InstallationId,
        added: Vec<(u64, RepoCoords)>,
        removed: Vec<u64>,
    },
    GithubRepository {
        installation: InstallationId,
        repo_external_id: u64,
        action: RepoAction,
        repo: RepoCoords,
        default_branch: String,
    },
    GithubDeploymentStatus {
        installation: InstallationId,
        repo_external_id: u64,
        deployment_id: u64,
        status_id: u64,
        environment: String,
        sha: CommitSha,
        state: DeploymentState,
        created_at: OffsetDateTime,
    },
    /// Fix App (or the single app in internal v1).
    GithubPullRequest {
        installation: InstallationId,
        repo_external_id: u64,
        number: u32,
        action: String,
        merged: bool,
        head_ref: String,
    },
    LinearIssue {
        workspace_id: String,
        issue_id: String,
        action: LinearAction,
        actor_id: Option<String>,
    },
    LinearComment {
        workspace_id: String,
        comment_id: String,
        issue_id: String,
        action: LinearAction,
        actor_id: Option<String>,
    },
    LinearAttachment {
        workspace_id: String,
        issue_id: String,
        action: LinearAction,
        actor_id: Option<String>,
    },
    LinearAppRevoked {
        workspace_id: String,
    },
    SlackAppUninstalled {
        team_id: String,
    },
    SlackTokensRevoked {
        team_id: String,
    },
    /// Optional extension, off in v1 (the Slack decoder never produces it unless enabled).
    SlackThreadMessage {
        team_id: String,
        channel: String,
        thread_ts: String,
        bot: bool,
    },
    Ignored {
        reason: String,
    },
}

/// A verified body the decoder cannot parse (provider bug or new payload shape).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("cannot decode webhook body: {0}")]
pub struct DecodeError(pub String);

pub trait WebhookDecoder: Send + Sync {
    fn source(&self) -> WebhookSource;
    fn decode(&self, event: &str, body: &[u8]) -> Result<HookEvent, DecodeError>;
}
