//! Deployment history and manual mapping (intake.md "`DeploymentResolver`", IngestDeployment).

use mage_core::domain::{CommitSha, Deployment, DeploymentSource, OrgId, RepoId, VersionReason};
use mage_core::wire::api::DeploymentList;

use super::{ReposService, repo_err};
use crate::error::AppError;

/// Longest environment name accepted by the manual mapping.
const MAX_ENVIRONMENT_BYTES: usize = 255;

impl ReposService {
    pub async fn list_deployments(&self, org: OrgId, repo: RepoId) -> Result<DeploymentList, AppError> {
        self.repo(org, repo).await?;
        Ok(DeploymentList { deployments: self.current_deployments(org, repo).await? })
    }

    /// Manual mapping `environment → sha`: append a `Manual` observation at `now`, then (as for an
    /// observed deployment) move `Deployed(environment)` to the sha and enqueue `index.version`.
    /// The sha must be a commit of the repo.
    pub async fn set_deployment(&self, org: OrgId, repo_id: RepoId, environment: &str, sha: CommitSha) -> Result<Deployment, AppError> {
        if environment.is_empty() || environment.len() > MAX_ENVIRONMENT_BYTES || environment.trim() != environment {
            return Err(AppError::Invalid(format!("environment must be 1..={MAX_ENVIRONMENT_BYTES} bytes without surrounding whitespace")));
        }
        let repo = self.connected_repo(org, repo_id).await?;
        let resolved = match self.resolve(&repo, &sha.to_hex()).await {
            Ok(resolved) => resolved,
            Err(AppError::NotFound(_)) => return Err(AppError::Invalid(format!("{} is not a commit of {}/{}", sha.to_hex(), repo.owner, repo.name))),
            Err(err) => return Err(err),
        };
        let deployment = Deployment {
            org_id: org,
            repo_id,
            environment: environment.to_owned(),
            sha: resolved,
            observed_at: self.clock.now(),
            source: DeploymentSource::Manual,
        };
        self.repos.record_deployment(org, &deployment).await.map_err(repo_err)?;
        self.index(org, repo_id, resolved, VersionReason::Deployed(deployment.environment.clone())).await?;
        Ok(deployment)
    }
}
