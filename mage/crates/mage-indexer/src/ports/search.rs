//! `TextSearch` port (knowledge.md "`TextSearch`"): live text/regex search over a materialized,
//! read-only checkout. Query and result DTOs are `mage_core::wire::indexer`'s (shared with the
//! server's `IndexerLink`).
//!
//! Semantics every implementation follows (contract suite in `crate::testing::contract_text_search`):
//!
//! - Hits are ordered by (path bytes, line); one hit per matching line, `col` = 1-based byte
//!   column of the first match on that line. Deterministic for a checkout and query.
//! - `q.check_limits()` violations, a bad regex and a bad glob are `Invalid`.
//! - `Literal` matches the pattern verbatim; `Regex` uses Rust `regex` syntax. `Smart` case is
//!   insensitive unless the pattern has an uppercase letter.
//! - `include` globs (any must match; empty = all) and `exclude` globs (none may match) apply to
//!   the repo-relative path; `*` also matches `/`.
//! - Files with a NUL byte (binary), larger than `max_file_bytes`, symlinks, and paths that are
//!   not valid `RepoPath`s are skipped and not counted in `files_scanned`.
//! - Keyset: with `after = (path, line)`, files whose path sorts before `path` are skipped, and in
//!   `path` itself lines `<= line`.
//! - At most `max_hits` hits. When more exist: `truncated = Limit`, `next` = position of the last
//!   returned hit. When `timeout` elapses: the hits so far, `truncated = Timeout`, `next` = the
//!   position up to which every hit has been returned (`(path, u32::MAX)` after a fully scanned
//!   file), or `None` when nothing was scanned. Otherwise `next = None`, `truncated = None`.
//!   Following `next` page by page yields exactly the hits of one unpaged run.
//! - Hit text and context lines are cut to `max_line_len` bytes (on a char boundary), without the
//!   line terminator; invalid UTF-8 is replaced.

use std::path::PathBuf;

use async_trait::async_trait;
use mage_core::domain::CommitSha;
use mage_core::wire::indexer::{TextHits, TextQuery};

use crate::error::AppError;

/// A read-only materialized tree at `sha` under `root`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkout {
    pub root: PathBuf,
    pub sha: CommitSha,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SearchError {
    #[error("checkout not found")]
    NotFound,
    #[error("conflict")]
    Conflict,
    /// Bad regex, bad glob, or a query outside `TextQuery` limits.
    #[error("invalid search: {0}")]
    Invalid(String),
    #[error("search unavailable: {0}")]
    Unavailable(String),
    #[error("search timed out")]
    Timeout,
}

#[async_trait]
pub trait TextSearch: Send + Sync {
    /// Hits ordered by (path bytes, line). Deterministic for a given checkout and query.
    async fn search(&self, checkout: &Checkout, q: &TextQuery) -> Result<TextHits, SearchError>;
}

impl From<SearchError> for AppError {
    fn from(err: SearchError) -> Self {
        match err {
            SearchError::NotFound => AppError::NotFound("checkout".into()),
            SearchError::Conflict => AppError::Conflict("search".into()),
            SearchError::Invalid(msg) => AppError::Invalid(msg),
            SearchError::Unavailable(msg) => AppError::Unavailable(format!("search: {msg}")),
            SearchError::Timeout => AppError::Unavailable("search timed out".into()),
        }
    }
}
