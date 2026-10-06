//! `IndexerLink` (knowledge.md "`IndexerLink`"): the server's client of the indexer's internal API
//! (`/internal/v1`). The server has no mirror and no worktree, so live text search, blame, tree
//! diffs and commit ranges run on the indexer. Argument and result types are the shared DTOs of
//! `mage_core::wire::indexer`.

use async_trait::async_trait;
use mage_core::domain::{CommitSha, LineRange, OrgId, RepoId, RepoPath, RepoVersionKey};
use mage_core::wire::indexer::{BlameHunk, CommitInfo, FileDiff, LogQuery, TextHits, TextQuery, TreeDiffOptions};

use crate::error::AppError;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IndexerLinkError {
    /// Unknown version/sha/path on the indexer (404).
    #[error("not found on the indexer")]
    NotFound,
    #[error("indexer conflict")]
    Conflict,
    /// The indexer refused the request (400): bad regex, bad glob, limits.
    #[error("invalid indexer request: {0}")]
    Invalid(String),
    /// Indexer down, overloaded (503), or the call could not be authenticated.
    #[error("indexer unavailable: {0}")]
    Unavailable(String),
    #[error("indexer call timed out")]
    Timeout,
}

#[async_trait]
pub trait IndexerLink: Send + Sync {
    /// Live text search at a version; `include_generated = false` drops hits in
    /// Generated/Vendored/Binary files (indexer side).
    async fn search(&self, org: OrgId, key: &RepoVersionKey, q: &TextQuery, include_generated: bool) -> Result<TextHits, IndexerLinkError>;
    /// `git blame` of at most `mage_core::wire::indexer::MAX_BLAME_LINES` lines.
    async fn blame(&self, org: OrgId, key: &RepoVersionKey, path: &RepoPath, lines: LineRange) -> Result<Vec<BlameHunk>, IndexerLinkError>;
    async fn diff(&self, org: OrgId, repo: RepoId, from: CommitSha, to: CommitSha, opts: &TreeDiffOptions) -> Result<Vec<FileDiff>, IndexerLinkError>;
    async fn log(&self, org: OrgId, repo: RepoId, q: &LogQuery) -> Result<Vec<CommitInfo>, IndexerLinkError>;
}

impl From<IndexerLinkError> for AppError {
    fn from(e: IndexerLinkError) -> Self {
        match e {
            IndexerLinkError::NotFound => AppError::NotFound("indexer: not found".into()),
            IndexerLinkError::Conflict => AppError::Conflict("indexer: conflict".into()),
            IndexerLinkError::Invalid(m) => AppError::Invalid(m),
            IndexerLinkError::Unavailable(m) => AppError::Unavailable(m),
            IndexerLinkError::Timeout => AppError::Unavailable("indexer call timed out".into()),
        }
    }
}
