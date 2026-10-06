//! `Parser` port (knowledge.md "`Parser`"): symbols, name refs and string-literal sites per blob
//! (layer `Symbols`), i18n catalogs (layer `ErrorSites`), framework detection and app facts
//! (layer `AppMap`). Fact types live in `mage_core::ports::knowledge`.
//!
//! Semantics every implementation follows:
//!
//! - `parse` / `app_facts` return one result per input, in input order. A per-blob failure
//!   (`Unsupported`, `TooLarge`, `Timeout`, grammar panic) never fails the whole call.
//! - Syntax errors do not fail a blob: extraction proceeds and the status is
//!   `StageStatus::Partial { error_nodes }`.
//! - Output for a given input is deterministic for a given `rev()` / `app_rev()`.

use async_trait::async_trait;
use mage_core::domain::{Language, RepoPath};
use mage_core::ports::knowledge::{AppFactRecord, AppMapRev, BlobInput, Catalog, FrameworkId, FrameworkScope, ParseLimits, ParsedBlob, ParserRev};

use crate::error::AppError;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("not found")]
    NotFound,
    #[error("conflict")]
    Conflict,
    #[error("invalid parser input: {0}")]
    Invalid(String),
    #[error("parser unavailable: {0}")]
    Unavailable(String),
    #[error("no extraction for {0:?}")]
    Unsupported(Language),
    #[error("blob larger than the parse limit")]
    TooLarge,
    #[error("parse timed out")]
    Timeout,
}

impl From<ParseError> for AppError {
    fn from(err: ParseError) -> Self {
        match err {
            ParseError::NotFound => AppError::NotFound("parser input".into()),
            ParseError::Conflict => AppError::Conflict("parser".into()),
            ParseError::Invalid(msg) => AppError::Invalid(msg),
            ParseError::Unavailable(msg) => AppError::Unavailable(msg),
            ParseError::Unsupported(lang) => AppError::Invalid(format!("no extraction for {lang:?}")),
            ParseError::TooLarge => AppError::Invalid("blob larger than the parse limit".into()),
            ParseError::Timeout => AppError::Unavailable("parse timed out".into()),
        }
    }
}

#[async_trait]
pub trait Parser: Send + Sync {
    /// Grammars + queries + extraction code revision.
    fn rev(&self) -> ParserRev;
    /// App extractors revision.
    fn app_rev(&self) -> AppMapRev;
    /// Languages with symbol/literal extraction.
    fn languages(&self) -> &[Language];
    /// Extension / filename / shebang (`head` = first 512 bytes). None = not a known language.
    fn detect_language(&self, path: &RepoPath, head: &[u8]) -> Option<Language>;
    /// Symbols, name refs, imports, literal sites. One result per input, same order.
    async fn parse(&self, blobs: Vec<BlobInput>, limits: ParseLimits) -> Vec<Result<ParsedBlob, ParseError>>;
    /// i18n message catalog; `Ok(None)` when the path/format is not a catalog.
    async fn catalog(&self, blob: BlobInput) -> Result<Option<Catalog>, ParseError>;
    /// Frameworks per project root from dependency manifests
    /// (go.mod, package.json, deps.edn, shadow-cljs.edn, project.clj, pyproject.toml, Cargo.toml).
    async fn detect_frameworks(&self, manifests: Vec<BlobInput>) -> Result<Vec<FrameworkScope>, ParseError>;
    /// App-map facts; `frameworks` are those whose root contains the blob path.
    async fn app_facts(&self, blobs: Vec<(BlobInput, Vec<FrameworkId>)>, limits: ParseLimits) -> Vec<Result<Vec<AppFactRecord>, ParseError>>;
}
