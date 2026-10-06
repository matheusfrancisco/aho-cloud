//! `ScipReader` / `ScipStream` port (knowledge.md "`ScipReader`"): streaming decode of a Learn run's
//! `ScipIndex` artifact (`index.scip`, a protobuf `scip.Index`). Fact types (`ScipMetadata`,
//! `ScipDocument`, …) are `mage_core::ports::knowledge`'s, because `KnowledgeStore` persists them.
//!
//! Semantics every implementation follows (contract suite in `crate::testing::contract_scip_reader`):
//!
//! - `open` reads only what `metadata()` needs; documents are decoded on demand, so memory stays
//!   bounded by the largest document.
//! - Ranges are converted to 1-based lines (SCIP lines are 0-based); columns stay 0-based.
//!   A 3-element SCIP range is a single-line range. Occurrences with a malformed range are dropped.
//! - Limits: more than `max_documents` documents, a document with more than
//!   `max_occurrences_per_doc` occurrences, or a symbol string longer than `max_symbol_len` bytes
//!   is `LimitExceeded`.
//! - `external_symbols` is valid only after `next_documents` returned an empty `Vec`
//!   (`Invalid` before).

use std::path::Path;

use async_trait::async_trait;
use mage_core::ports::knowledge::{ScipDocument, ScipMetadata, ScipSymbolInfo};

use crate::error::AppError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScipLimits {
    pub max_documents: u32,
    pub max_occurrences_per_doc: u32,
    pub max_symbol_len: u32,
}

impl ScipLimits {
    /// knowledge.md "Layer 2: Refs" step 3.
    pub const INGEST: ScipLimits = ScipLimits { max_documents: 200_000, max_occurrences_per_doc: 200_000, max_symbol_len: 1024 };
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScipError {
    #[error("scip index not found")]
    NotFound,
    #[error("conflict")]
    Conflict,
    #[error("invalid scip index: {0}")]
    Invalid(String),
    #[error("scip reader unavailable: {0}")]
    Unavailable(String),
    #[error("scip limit exceeded: {0}")]
    LimitExceeded(String),
}

#[async_trait]
pub trait ScipReader: Send + Sync {
    /// Open a local `index.scip` file (downloaded with `ArtifactStore::get_to_file`).
    async fn open(&self, file: &Path, limits: ScipLimits) -> Result<Box<dyn ScipStream>, ScipError>;
}

#[async_trait]
pub trait ScipStream: Send {
    fn metadata(&self) -> &ScipMetadata;
    /// Next `max` documents (`max >= 1`); empty `Vec` = end of documents.
    async fn next_documents(&mut self, max: usize) -> Result<Vec<ScipDocument>, ScipError>;
    /// `Index.external_symbols`; valid after documents are exhausted.
    async fn external_symbols(&mut self) -> Result<Vec<ScipSymbolInfo>, ScipError>;
}

impl From<ScipError> for AppError {
    /// An unreadable or oversized artifact will not get better on retry: `Invalid` (permanent).
    fn from(err: ScipError) -> Self {
        match err {
            ScipError::NotFound => AppError::NotFound("scip index".into()),
            ScipError::Conflict => AppError::Conflict("scip index".into()),
            ScipError::Invalid(msg) => AppError::Invalid(format!("scip index: {msg}")),
            ScipError::LimitExceeded(msg) => AppError::Invalid(format!("scip index limit exceeded: {msg}")),
            ScipError::Unavailable(msg) => AppError::Unavailable(format!("scip reader: {msg}")),
        }
    }
}
