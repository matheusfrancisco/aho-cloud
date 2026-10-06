//! `PullRequests` port (fix.md "`PullRequests` port"): pure GitHub I/O for F3, no patch logic and
//! no policy. Every method takes the `OrgId` and the `Repo`; the adapter resolves the org's **fix**
//! installation (integration kind `GitHubFix`) and its token itself.
//!
//! `create_commit` only accepts a `CommitPlan`, whose sole constructor takes a `FixGatePassed`
//! (`mage_core::domain::fix`), so neither the adapter nor the fake can be handed an ungated
//! change set.

use std::collections::BTreeSet;
use std::time::Duration;

use async_trait::async_trait;
use mage_core::domain::fix::{BranchName, CommitPlan};
use mage_core::domain::{CommitSha, OrgId, Repo, RepoPath};
use url::Url;

pub use mage_core::domain::fix::{BaseFile, FileMode};

use crate::error::AppError;

/// Permissions granted to the fix installation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct WriteAccess {
    pub contents: bool,
    pub pull_requests: bool,
    pub workflows: bool,
    pub issues: bool,
}

impl WriteAccess {
    /// Permission names F3 needs but lacks (`contents`, `pull_requests`, and `workflows` when the
    /// change touches workflow paths).
    pub fn missing(&self, needs_workflows: bool) -> Vec<&'static str> {
        let mut missing = Vec::new();
        if !self.contents {
            missing.push("contents");
        }
        if !self.pull_requests {
            missing.push("pull_requests");
        }
        if needs_workflows && !self.workflows {
            missing.push("workflows");
        }
        missing
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Comparison {
    pub ahead_by: u32,
    pub behind_by: u32,
    pub changed_paths: BTreeSet<RepoPath>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchOutcome {
    Created,
    /// The branch already points at the requested commit.
    AlreadyAt,
    /// The branch exists at another commit; never force-updated.
    ExistsAt(CommitSha),
}

/// `POST /repos/{o}/{r}/pulls` input. (Same name as the use case `crate::app::fix::OpenPullRequest`;
/// they live in different modules.)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenPullRequest {
    pub title: String,
    pub body: String,
    pub head: BranchName,
    pub base_branch: String,
    pub draft: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestRef {
    pub number: u64,
    pub url: Url,
    pub head_commit: CommitSha,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Reviewers {
    pub users: Vec<String>,
    pub teams: Vec<String>,
}

impl Reviewers {
    pub fn is_empty(&self) -> bool {
        self.users.is_empty() && self.teams.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mergeable {
    Clean,
    Conflicting,
    /// GitHub has not computed it yet (`mergeable: null`).
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PullRequestState {
    pub open: bool,
    pub merged: bool,
    pub draft: bool,
    pub mergeable: Mergeable,
    pub head_commit: CommitSha,
}

/// Media kinds GitHub renders in a PR body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaKind {
    Mp4,
    Gif,
    Png,
}

impl MediaKind {
    pub fn content_type(self) -> &'static str {
        match self {
            MediaKind::Mp4 => "video/mp4",
            MediaKind::Gif => "image/gif",
            MediaKind::Png => "image/png",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            MediaKind::Mp4 => "mp4",
            MediaKind::Gif => "gif",
            MediaKind::Png => "png",
        }
    }

    /// GitHub's limit for this kind on any plan (10 MB; paid plans allow 100 MB video).
    pub fn max_bytes(self) -> u64 {
        10 * 1000 * 1000
    }
}

/// One file for `upload_media`.
#[derive(Clone, PartialEq, Eq)]
pub struct MediaUpload {
    /// File name shown by GitHub, e.g. `before.gif`.
    pub name: String,
    pub kind: MediaKind,
    pub bytes: Vec<u8>,
}

impl std::fmt::Debug for MediaUpload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaUpload").field("name", &self.name).field("kind", &self.kind).field("bytes", &self.bytes.len()).finish()
    }
}

/// `https://github.com/user-attachments/assets/<id>`, the only URL kind GitHub renders as an inline
/// video player.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct MediaUrl(pub Url);

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PullRequestsError {
    #[error("not found")]
    NotFound,
    #[error("conflict")]
    Conflict,
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("github unavailable: {0}")]
    Unavailable(String),
    #[error("fix app lacks permissions: {}", missing.join(", "))]
    PermissionDenied { missing: Vec<&'static str> },
    #[error("rate limited for {}s", retry_after.as_secs())]
    RateLimited { retry_after: Duration },
}

impl From<PullRequestsError> for AppError {
    fn from(err: PullRequestsError) -> Self {
        match err {
            PullRequestsError::NotFound => AppError::NotFound("github resource".into()),
            PullRequestsError::Conflict => AppError::Conflict("github resource changed".into()),
            PullRequestsError::Invalid(msg) => AppError::Invalid(msg),
            PullRequestsError::Unavailable(msg) => AppError::Unavailable(msg),
            PullRequestsError::PermissionDenied { missing } => {
                AppError::Forbidden(format!("the Mage Fix GitHub App needs: {}", missing.join(", ")))
            }
            PullRequestsError::RateLimited { retry_after } => AppError::RateLimited { retry_after_secs: Some(retry_after.as_secs().max(1)) },
        }
    }
}

#[async_trait]
pub trait PullRequests: Send + Sync {
    /// Permissions granted to the fix installation for this repo.
    async fn write_access(&self, org: OrgId, repo: &Repo) -> Result<WriteAccess, PullRequestsError>;

    /// Current head of a branch; `None` if the branch does not exist.
    async fn branch_head(&self, org: OrgId, repo: &Repo, branch: &str) -> Result<Option<CommitSha>, PullRequestsError>;

    /// Paths changed between two commits and how far `head` is ahead of `base`.
    async fn compare(&self, org: OrgId, repo: &Repo, base: CommitSha, head: CommitSha) -> Result<Comparison, PullRequestsError>;

    /// Content and mode of `paths` at `at`. Missing paths are returned as `BaseFile::Absent`.
    async fn read_files(&self, org: OrgId, repo: &Repo, at: CommitSha, paths: &[RepoPath]) -> Result<Vec<BaseFile>, PullRequestsError>;

    /// Create blobs, a tree on top of `plan.parent()`'s tree, and a commit. Moves no ref.
    /// Idempotent: same plan → same tree sha; the commit sha may differ only by timestamp.
    async fn create_commit(&self, org: OrgId, repo: &Repo, plan: &CommitPlan) -> Result<CommitSha, PullRequestsError>;

    /// Create `refs/heads/<branch>` at `commit`. Never force-updates.
    async fn create_branch(&self, org: OrgId, repo: &Repo, branch: &BranchName, commit: CommitSha)
    -> Result<BranchOutcome, PullRequestsError>;

    /// A commit's message trailers (recognises Mage's own commits on retry).
    async fn commit_trailers(&self, org: OrgId, repo: &Repo, commit: CommitSha) -> Result<Vec<(String, String)>, PullRequestsError>;

    /// Open PR whose head is `branch`, if any.
    async fn find_open(&self, org: OrgId, repo: &Repo, branch: &BranchName) -> Result<Option<PullRequestRef>, PullRequestsError>;

    async fn open(&self, org: OrgId, repo: &Repo, req: &OpenPullRequest) -> Result<PullRequestRef, PullRequestsError>;

    async fn add_labels(&self, org: OrgId, repo: &Repo, number: u64, labels: &[String]) -> Result<(), PullRequestsError>;

    /// Upload an mp4 / gif / png through GitHub's user-attachments flow (undocumented
    /// `uploads.github.com/user-attachments/assets`); callers fall back to Mage links on any error.
    async fn upload_media(&self, org: OrgId, repo: &Repo, media: &MediaUpload) -> Result<MediaUrl, PullRequestsError>;

    async fn request_reviewers(&self, org: OrgId, repo: &Repo, number: u64, who: &Reviewers) -> Result<(), PullRequestsError>;

    /// State incl. GitHub's computed mergeability (may be `Unknown` right after open).
    async fn state(&self, org: OrgId, repo: &Repo, number: u64) -> Result<PullRequestState, PullRequestsError>;
}

/// Git trailers (`Key: value` lines of the last paragraph) of a commit message.
pub fn parse_trailers(message: &str) -> Vec<(String, String)> {
    let trimmed = message.trim_end();
    let last = trimmed.rsplit("\n\n").next().unwrap_or("");
    let mut out = Vec::new();
    for line in last.lines() {
        let Some((key, value)) = line.split_once(": ") else {
            return Vec::new();
        };
        if key.is_empty() || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return Vec::new();
        }
        out.push((key.to_owned(), value.trim().to_owned()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailers_come_from_the_last_paragraph_only() {
        let msg = "fix: thing (ENG-1)\n\nSummary: not a trailer block\nbecause this line is prose\n\nLinear: https://linear.app/x/ENG-1\nMage-Run: run_1\nMage-Change-Set: abc\n";
        assert_eq!(
            parse_trailers(msg),
            vec![
                ("Linear".to_owned(), "https://linear.app/x/ENG-1".to_owned()),
                ("Mage-Run".to_owned(), "run_1".to_owned()),
                ("Mage-Change-Set".to_owned(), "abc".to_owned()),
            ]
        );
        assert!(parse_trailers("fix: x\n\njust prose here").is_empty());
        assert!(parse_trailers("").is_empty());
    }

    #[test]
    fn missing_permissions_are_named() {
        let access = WriteAccess { contents: true, pull_requests: false, workflows: false, issues: true };
        assert_eq!(access.missing(false), vec!["pull_requests"]);
        assert_eq!(access.missing(true), vec!["pull_requests", "workflows"]);
    }
}
