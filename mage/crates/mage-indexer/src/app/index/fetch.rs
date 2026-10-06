//! UC `index.fetch` (knowledge.md "Mirror management"): make the sha present in the repo's bare
//! mirror, pin it, and fan out to `index.manifest`, `index.history`, `index.bundle_source`.
//!
//! Permanent outcomes are handled here (not by dying): an unreachable sha or revoked access fails
//! every open layer and enqueues `runs.source_ready` so waiting runs error out.

use mage_core::domain::Layer;
use mage_core::ports::source_host::{RepoCoords, SourceHostError};

use super::layers::{fail_open, reasons};
use super::{IndexPipeline, IndexTarget, JobDone};
use crate::error::AppError;
use crate::ports::git::{FetchWants, GitError};

impl IndexPipeline {
    pub async fn fetch(&self, t: IndexTarget) -> Result<JobDone, AppError> {
        let git = self.deps.git.as_ref();
        let m = t.mirror();

        if !git.has_commit(&m, t.sha).await? {
            let repo = self.deps.repos.repo(t.org, t.repo).await?;
            let coords = RepoCoords { owner: repo.owner.clone(), name: repo.name.clone() };
            let spec = match self.deps.source.fetch_spec(repo.installation_id, &coords).await {
                Ok(spec) => spec,
                Err(SourceHostError::Unauthorized | SourceHostError::NotFound) => return self.unreachable(t, reasons::ACCESS_REVOKED).await,
                Err(err) => return Err(err.into()),
            };
            let wants = FetchWants { all_heads_and_tags: true, shas: vec![t.sha] };
            let outcome = match git.fetch(&m, &spec, &wants).await {
                Ok(outcome) => outcome,
                Err(GitError::Unauthorized) => return self.unreachable(t, reasons::ACCESS_REVOKED).await,
                Err(err) => return Err(err.into()),
            };
            tracing::debug!(org = %t.org, repo = %t.repo, sha = %t.sha, updated = outcome.updated_refs.len(), "mirror fetched");
            if outcome.missing.contains(&t.sha) {
                return self.unreachable(t, reasons::SHA_NOT_REACHABLE).await;
            }
        }
        git.pin(&m, t.sha).await?;
        Ok(JobDone::Complete(vec![t.manifest_job(), t.history_job(), t.bundle_job()]))
    }

    /// Every open layer `Failed { reason }`; `runs.source_ready` lets waiting runs error out.
    async fn unreachable(&self, t: IndexTarget, reason: &str) -> Result<JobDone, AppError> {
        tracing::warn!(org = %t.org, repo = %t.repo, sha = %t.sha, reason, "sha cannot be fetched");
        fail_open(self.deps.repos.as_ref(), t.org, t.key(), &Layer::ALL, reason).await?;
        Ok(JobDone::Complete(vec![t.source_ready_job()]))
    }
}
