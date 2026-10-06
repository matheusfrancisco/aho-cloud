//! Use-case error for this role. Port error enums convert into it with `From` impls placed next
//! to each port; the conversions of the `mage_core` port errors live here.

use mage_core::ports::knowledge::KnowledgeError;
use mage_core::ports::repos::RepoStoreError;
use mage_core::ports::source_host::SourceHostError;
use mage_core::ports::{BlobError, JobError};

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("unavailable: {0}")]
    Unavailable(String),
    /// Retry no earlier than `retry_at` (upstream rate limit).
    #[error("rate limited until {retry_at}")]
    RateLimited { retry_at: time::OffsetDateTime },
    #[error(transparent)]
    Domain(#[from] mage_core::domain::DomainError),
    #[error("internal: {0}")]
    Internal(String),
}

impl From<RepoStoreError> for AppError {
    fn from(err: RepoStoreError) -> Self {
        match err {
            RepoStoreError::NotFound => AppError::NotFound("repo store row".into()),
            RepoStoreError::Conflict => AppError::Conflict("repo store".into()),
            RepoStoreError::Invalid(msg) => AppError::Invalid(msg),
            RepoStoreError::Unavailable(msg) => AppError::Unavailable(msg),
        }
    }
}

impl From<KnowledgeError> for AppError {
    fn from(err: KnowledgeError) -> Self {
        match err {
            KnowledgeError::NotFound => AppError::NotFound("knowledge row".into()),
            KnowledgeError::Conflict => AppError::Conflict("knowledge store".into()),
            KnowledgeError::Invalid(msg) => AppError::Invalid(msg),
            KnowledgeError::Unavailable(msg) => AppError::Unavailable(msg),
            KnowledgeError::Timeout => AppError::Unavailable("knowledge store timed out".into()),
        }
    }
}

impl From<BlobError> for AppError {
    fn from(err: BlobError) -> Self {
        match err {
            BlobError::NotFound => AppError::NotFound("object".into()),
            BlobError::Conflict => AppError::Conflict("object store".into()),
            BlobError::Invalid(msg) => AppError::Invalid(msg),
            BlobError::Unavailable(msg) => AppError::Unavailable(msg),
            BlobError::TooLarge { limit } => AppError::Invalid(format!("object larger than {limit} bytes")),
        }
    }
}

impl From<JobError> for AppError {
    fn from(err: JobError) -> Self {
        match err {
            JobError::NotFound => AppError::NotFound("job".into()),
            JobError::Conflict | JobError::LeaseLost => AppError::Conflict("job queue".into()),
            JobError::Invalid(msg) => AppError::Invalid(msg),
            JobError::Unavailable(msg) => AppError::Unavailable(msg),
        }
    }
}

impl From<SourceHostError> for AppError {
    fn from(err: SourceHostError) -> Self {
        match err {
            SourceHostError::NotFound => AppError::NotFound("source host object".into()),
            SourceHostError::Conflict => AppError::Conflict("source host".into()),
            SourceHostError::Invalid(msg) => AppError::Invalid(msg),
            SourceHostError::Unavailable(msg) => AppError::Unavailable(msg),
            SourceHostError::Unauthorized => AppError::Invalid("source access revoked".into()),
            SourceHostError::RateLimited { retry_at } => AppError::RateLimited { retry_at },
        }
    }
}
