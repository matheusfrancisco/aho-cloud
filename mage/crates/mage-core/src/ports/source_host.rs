//! `SourceHost` port (intake.md "`SourceHost`"): the read-only GitHub App as seen by intake
//! (server) and knowledge (indexer). Server-only GitHub calls are on `SourceHostAdmin`
//! (`mage_server::ports`).
//!
//! Token contract: an implementation returns a cached token only with at least
//! [`MIN_TOKEN_VALIDITY`] left; callers never keep a token past `expires_at`.

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use time::OffsetDateTime;
use url::Url;

use crate::domain::ids::{CommitSha, InstallationId};

/// A cached installation token is handed out only with at least this much validity left.
pub const MIN_TOKEN_VALIDITY: Duration = Duration::from_secs(5 * 60);

/// Username of a git fetch authenticated with an installation token.
pub const FETCH_USERNAME: &str = "x-access-token";

/// `list_pull_requests` page size ceiling (GitHub's `per_page` maximum).
pub const MAX_PR_PAGE: u8 = 100;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RepoCoords {
    pub owner: String,
    pub name: String,
}

impl fmt::Display for RepoCoords {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.owner, self.name)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Access {
    Read,
    Write,
}

/// A bearer credential. `Debug`/`Display` print `***`; read it with [`Self::expose`].
#[derive(Clone, PartialEq, Eq)]
pub struct AccessToken(String);

impl AccessToken {
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

impl fmt::Display for AccessToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

/// What an installation token may do.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TokenScope {
    /// Repository names; `None` = every repo granted to the installation.
    pub repositories: Option<Vec<String>>,
    /// Permission name (`contents`, `pull_requests`, …) → access; empty = the installation's full
    /// set.
    pub permissions: BTreeMap<String, Access>,
}

impl TokenScope {
    /// The indexer's fetch scope: one repo, `contents: read`, `pull_requests: read`.
    pub fn fetch(repo_name: &str) -> Self {
        Self {
            repositories: Some(vec![repo_name.to_owned()]),
            permissions: BTreeMap::from([("contents".to_owned(), Access::Read), ("pull_requests".to_owned(), Access::Read)]),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallationToken {
    pub token: AccessToken,
    pub expires_at: OffsetDateTime,
    pub permissions: BTreeMap<String, Access>,
    pub repositories: Option<Vec<String>>,
}

/// Everything a git fetch of one repo needs; never stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchSpec {
    pub url: Url,
    /// [`FETCH_USERNAME`].
    pub username: String,
    pub token: InstallationToken,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AccountKind {
    User,
    Organization,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RepoSelection {
    All,
    Selected,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallationInfo {
    pub id: InstallationId,
    pub account_login: String,
    pub account_kind: AccountKind,
    pub permissions: BTreeMap<String, Access>,
    pub repository_selection: RepoSelection,
    pub suspended: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteRepo {
    /// GitHub repository id, stable across renames.
    pub external_id: u64,
    pub coords: RepoCoords,
    pub default_branch: String,
    pub private: bool,
    pub archived: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrState {
    Open,
    Closed,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrSort {
    Created,
    Updated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SortDirection {
    Asc,
    Desc,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrListQuery {
    pub state: PrState,
    pub sort: PrSort,
    pub direction: SortDirection,
    /// Only PRs with `updated_at >= updated_since`.
    pub updated_since: Option<OffsetDateTime>,
    /// `PrPage::next` of the previous page.
    pub page_token: Option<String>,
    /// `1..=100`.
    pub per_page: u8,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestInfo {
    pub number: u32,
    pub title: String,
    /// First 2 KiB of the body.
    pub body_head: String,
    pub head_ref: String,
    pub base_ref: String,
    pub merge_commit_sha: Option<CommitSha>,
    pub merged_at: Option<OffsetDateTime>,
    pub updated_at: OffsetDateTime,
    pub url: String,
    pub author_login: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrPage {
    pub items: Vec<PullRequestInfo>,
    pub next: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SourceHostError {
    #[error("not found")]
    NotFound,
    #[error("conflict")]
    Conflict,
    #[error("invalid source host request: {0}")]
    Invalid(String),
    #[error("source host unavailable: {0}")]
    Unavailable(String),
    /// Credential revoked or expired after one refresh (installation deleted or suspended).
    #[error("source host access unauthorized")]
    Unauthorized,
    #[error("source host rate limited until {retry_at}")]
    RateLimited { retry_at: OffsetDateTime },
}

#[async_trait]
pub trait SourceHost: Send + Sync {
    // ---- installations (app JWT) ----
    async fn installation(&self, inst: InstallationId) -> Result<InstallationInfo, SourceHostError>;
    async fn installation_repos(&self, inst: InstallationId) -> Result<Vec<RemoteRepo>, SourceHostError>;

    // ---- repo metadata (installation token) ----
    async fn repo(&self, inst: InstallationId, repo: &RepoCoords) -> Result<RemoteRepo, SourceHostError>;
    /// Branch, tag (annotated tags peeled to the commit), full sha or unique sha prefix.
    async fn resolve_ref(&self, inst: InstallationId, repo: &RepoCoords, git_ref: &str) -> Result<CommitSha, SourceHostError>;

    // ---- tokens and fetch (indexer) ----
    async fn installation_token(&self, inst: InstallationId, scope: &TokenScope) -> Result<InstallationToken, SourceHostError>;
    /// `https://github.com/{owner}/{name}.git`, [`FETCH_USERNAME`], a token of
    /// [`TokenScope::fetch`].
    async fn fetch_spec(&self, inst: InstallationId, repo: &RepoCoords) -> Result<FetchSpec, SourceHostError>;
    async fn list_pull_requests(&self, inst: InstallationId, repo: &RepoCoords, query: &PrListQuery) -> Result<PrPage, SourceHostError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn access_token_never_prints_its_secret() {
        let token = AccessToken::new("ghs_secret");
        assert_eq!(format!("{token:?} {token}"), "*** ***");
        let spec_debug = format!("{:?}", InstallationToken {
            token,
            expires_at: OffsetDateTime::UNIX_EPOCH,
            permissions: BTreeMap::new(),
            repositories: None
        });
        assert!(!spec_debug.contains("ghs_secret"));
    }
}
