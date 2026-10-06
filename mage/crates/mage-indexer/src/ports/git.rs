//! `GitRepoReader` port (knowledge.md "`GitRepoReader`"): one bare mirror per repo on the indexer's
//! local disk, read by the pipeline (tree listings, blobs, bundles) and the internal API (blame,
//! diff, log, checkouts). `CommitInfo`, `FileChange`, `TreeDiffOptions`, `FileDiff`, `LogQuery`,
//! `BlameHunk` are `mage_core::wire::indexer` DTOs shared with the server's `IndexerLink`.
//!
//! Semantics every implementation follows (contract suite in `mage_indexer::testing`):
//!
//! - `fetch` creates the mirror when absent, fetches every head and tag (pruned), then fetches the
//!   wanted shas by id when still missing; shas still absent afterwards are reported in
//!   `FetchOutcome::missing` (not an error). Fetches of one mirror are serialized.
//! - `pin(sha)` keeps a commit reachable (`refs/mage/keep/<sha>`) across force-pushes and gc;
//!   `unpin` drops that ref; both are idempotent.
//! - A sha or rev that is not in the mirror is `NotFound`; a missing mirror is `NotFound` for
//!   every read.
//! - `list_tree` lists blobs, symlinks and gitlinks recursively (never tree entries), sorted by
//!   path bytes; paths that are not valid `RepoPath`s are skipped and counted.
//! - `read_blobs` returns one result per requested oid in input order; `bytes` is `None` when the
//!   blob is larger than `max_bytes`.
//! - `archive` writes the deterministic tar.zst of knowledge.md "SourceBundle"; the same tree
//!   always yields the same bytes.

use std::path::Path;

use async_trait::async_trait;
use mage_core::domain::{BlobOid, CommitSha, LineRange, OrgId, RepoId, RepoPath, TreeOid};
use mage_core::ports::source_host::FetchSpec;
use mage_core::wire::indexer::{BlameHunk, CommitInfo, FileDiff, LogQuery, TreeDiffOptions};
use time::OffsetDateTime;

use crate::error::AppError;

/// A repo's mirror; the adapter maps it to `{data_dir}/mirrors/{org}/{repo}.git`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct MirrorRef {
    pub org: OrgId,
    pub repo: RepoId,
}

impl MirrorRef {
    pub const fn new(org: OrgId, repo: RepoId) -> Self {
        Self { org, repo }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FetchWants {
    /// Fetch `+refs/heads/*` and `+refs/tags/*` (pruned) first.
    pub all_heads_and_tags: bool,
    /// Commits fetched by id when still missing after the ref fetch.
    pub shas: Vec<CommitSha>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct FetchOutcome {
    /// Refs whose target changed (or appeared) in this fetch, sorted by name.
    pub updated_refs: Vec<(String, CommitSha)>,
    /// Wanted shas still absent after the fetch.
    pub missing: Vec<CommitSha>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MirrorState {
    pub exists: bool,
    pub size_bytes: u64,
    pub last_fetch: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TreeEntryKind {
    File { oid: BlobOid, executable: bool },
    Symlink { oid: BlobOid },
    Submodule { commit: CommitSha },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    pub path: RepoPath,
    pub kind: TreeEntryKind,
    /// Blob size; `None` for submodules.
    pub size: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeListing {
    pub tree: TreeOid,
    /// Sorted by path bytes.
    pub entries: Vec<TreeEntry>,
    /// Non-UTF-8 or otherwise invalid paths left out.
    pub skipped_paths: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobRead {
    pub oid: BlobOid,
    pub size: u64,
    /// `None` if `size > max_bytes`.
    pub bytes: Option<Vec<u8>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArchiveInfo {
    pub tree: TreeOid,
    /// Compressed size.
    pub bytes: u64,
    /// SHA-256 of the compressed bytes.
    pub sha256: [u8; 32],
    /// Tar entries written (directories included).
    pub entries: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GitError {
    #[error("git object or mirror not found")]
    NotFound,
    #[error("git conflict")]
    Conflict,
    #[error("invalid git request: {0}")]
    Invalid(String),
    #[error("git unavailable: {0}")]
    Unavailable(String),
    /// The remote rejected the credentials (installation revoked or suspended).
    #[error("git remote rejected the credentials")]
    Unauthorized,
    /// The mirror is damaged; the adapter re-creates it on the next fetch.
    #[error("git mirror corrupt: {0}")]
    Corrupt(String),
    #[error("git command timed out")]
    Timeout,
}

impl From<GitError> for AppError {
    fn from(err: GitError) -> Self {
        match err {
            GitError::NotFound => AppError::NotFound("git object".into()),
            GitError::Conflict => AppError::Conflict("git".into()),
            GitError::Invalid(msg) => AppError::Invalid(msg),
            GitError::Unauthorized => AppError::Invalid("git remote rejected the credentials".into()),
            GitError::Unavailable(msg) => AppError::Unavailable(msg),
            GitError::Corrupt(msg) => AppError::Unavailable(format!("mirror corrupt: {msg}")),
            GitError::Timeout => AppError::Unavailable("git timed out".into()),
        }
    }
}

#[async_trait]
pub trait GitRepoReader: Send + Sync {
    async fn mirror_state(&self, m: &MirrorRef) -> Result<MirrorState, GitError>;
    /// Create the bare mirror if absent, then fetch. Serialized per mirror inside the adapter.
    async fn fetch(&self, m: &MirrorRef, spec: &FetchSpec, wants: &FetchWants) -> Result<FetchOutcome, GitError>;
    /// Pin a sha so gc never prunes it (`refs/mage/keep/<sha>`); unpin on version GC.
    async fn pin(&self, m: &MirrorRef, sha: CommitSha) -> Result<(), GitError>;
    async fn unpin(&self, m: &MirrorRef, sha: CommitSha) -> Result<(), GitError>;
    /// Branch, tag (peeled to its commit), full sha or `HEAD`.
    async fn resolve(&self, m: &MirrorRef, rev: &str) -> Result<CommitSha, GitError>;
    /// `false` when the mirror is absent.
    async fn has_commit(&self, m: &MirrorRef, sha: CommitSha) -> Result<bool, GitError>;
    /// `files` is `None`.
    async fn commit(&self, m: &MirrorRef, sha: CommitSha) -> Result<CommitInfo, GitError>;
    /// Recursive listing of blobs, symlinks, gitlinks (no tree entries), sorted by path bytes.
    async fn list_tree(&self, m: &MirrorRef, sha: CommitSha) -> Result<TreeListing, GitError>;
    async fn read_blobs(&self, m: &MirrorRef, oids: &[BlobOid], max_bytes: u64) -> Result<Vec<BlobRead>, GitError>;
    async fn diff(&self, m: &MirrorRef, from: CommitSha, to: CommitSha, opts: &TreeDiffOptions) -> Result<Vec<FileDiff>, GitError>;
    /// `git rev-list include --not exclude`, newest first.
    async fn log(&self, m: &MirrorRef, q: &LogQuery) -> Result<Vec<CommitInfo>, GitError>;
    async fn blame(&self, m: &MirrorRef, sha: CommitSha, path: &RepoPath, lines: LineRange) -> Result<Vec<BlameHunk>, GitError>;
    /// Deterministic tar.zst of the tree at `sha` written to `dest` (see SourceBundle).
    async fn archive(&self, m: &MirrorRef, sha: CommitSha, dest: &Path) -> Result<ArchiveInfo, GitError>;
    /// Write the tree at `sha` as plain files under `dest` (read-only checkout for TextSearch).
    async fn materialize(&self, m: &MirrorRef, sha: CommitSha, dest: &Path) -> Result<(), GitError>;
    /// `git maintenance run --auto`.
    async fn maintain(&self, m: &MirrorRef) -> Result<(), GitError>;
    /// Absent is `Ok`.
    async fn remove_mirror(&self, m: &MirrorRef) -> Result<(), GitError>;
    /// Every mirror on disk (`index.gc` removes mirrors of deleted repos), sorted.
    async fn list_mirrors(&self) -> Result<Vec<MirrorRef>, GitError>;
}