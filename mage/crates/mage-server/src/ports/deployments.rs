//! `DeploymentResolver` port (intake.md "`DeploymentResolver`"): which sha was deployed to an
//! environment at a point in time. Impls: `adapters::driven::pg::deployments_manual`
//! (recorded history) and `adapters::driven::github::deployments` (live Deployments API);
//! `app::intake::DeploymentChain` asks them in order.

use async_trait::async_trait;
use mage_core::domain::{Deployment, OrgId, Repo};
use mage_core::ports::source_host::SourceHostError;
use mage_core::ports::RepoStoreError;
use time::OffsetDateTime;

use crate::error::AppError;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DeploymentError {
    #[error("not found")]
    NotFound,
    #[error("conflict")]
    Conflict,
    #[error("invalid deployment lookup: {0}")]
    Invalid(String),
    #[error("deployment source unavailable: {0}")]
    Unavailable(String),
    #[error("deployment source unauthorized")]
    Unauthorized,
    #[error("deployment source rate limited until {retry_at}")]
    RateLimited { retry_at: OffsetDateTime },
}

#[async_trait]
pub trait DeploymentResolver: Send + Sync {
    /// Newest *successful* deployment of `environment` for `repo` with `observed_at <= as_of`.
    /// `Ok(None)` = this source has no answer, try the next one.
    /// `Err` = answer unknown; the caller retries and never falls through to a weaker source.
    async fn deployed_sha(&self, org: OrgId, repo: &Repo, environment: &str, as_of: OffsetDateTime)
    -> Result<Option<Deployment>, DeploymentError>;
}

impl From<SourceHostError> for DeploymentError {
    fn from(err: SourceHostError) -> Self {
        match err {
            SourceHostError::NotFound => Self::NotFound,
            SourceHostError::Conflict => Self::Conflict,
            SourceHostError::Invalid(msg) => Self::Invalid(msg),
            SourceHostError::Unavailable(msg) => Self::Unavailable(msg),
            SourceHostError::Unauthorized => Self::Unauthorized,
            SourceHostError::RateLimited { retry_at } => Self::RateLimited { retry_at },
        }
    }
}

impl From<RepoStoreError> for DeploymentError {
    fn from(err: RepoStoreError) -> Self {
        match err {
            RepoStoreError::NotFound => Self::NotFound,
            RepoStoreError::Conflict => Self::Conflict,
            RepoStoreError::Invalid(msg) => Self::Invalid(msg),
            RepoStoreError::Unavailable(msg) => Self::Unavailable(msg),
        }
    }
}

impl From<DeploymentError> for AppError {
    fn from(err: DeploymentError) -> Self {
        match err {
            DeploymentError::NotFound => AppError::NotFound("deployment".into()),
            DeploymentError::Conflict => AppError::Conflict("deployment".into()),
            DeploymentError::Invalid(msg) => AppError::Invalid(msg),
            DeploymentError::Unavailable(msg) => AppError::Unavailable(msg),
            DeploymentError::Unauthorized => AppError::Forbidden("deployment source access revoked".into()),
            DeploymentError::RateLimited { .. } => AppError::RateLimited { retry_after_secs: None },
        }
    }
}
