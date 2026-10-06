//! SourceReady (job `runs.source_ready {org_id, repo_id, sha}`, runs.md): the indexer recorded a
//! `SourceBundleRef` (→ mark the waiting runs leasable) or gave up bundling (→ `InfraError` with
//! `SourceUnavailable` for every `Queued` run still waiting).

use mage_core::domain::{CommitSha, OrgId, RepoId, RepoVersionKey, RunError, RunErrorCode, RunStatus, RunStatusEvent};
use mage_core::ports::repos::RepoStoreError;

use crate::app::runs::machine::Initiator;
use crate::app::runs::{RunManager, is_stale, repo_err};
use crate::error::AppError;
use crate::ports::runs::{RunQuery, RunWrite};

impl RunManager {
    /// Returns how many runs changed.
    pub async fn source_ready(&self, org: OrgId, repo: RepoId, sha: CommitSha) -> Result<u32, AppError> {
        let bundle = match self.d.repos.version(org, RepoVersionKey::new(repo, sha)).await {
            Ok(v) => v.bundle,
            Err(RepoStoreError::NotFound) => None,
            Err(e) => return Err(repo_err(e)),
        };
        if bundle.is_some() {
            return Ok(self.d.runs.mark_source_ready(org, repo, sha).await?);
        }
        let waiting = self
            .d
            .runs
            .list(org, &RunQuery { repo: Some(repo), sha: Some(sha), statuses: vec![RunStatus::Queued], limit: RunQuery::MAX_LIMIT, ..RunQuery::default() })
            .await?;
        let mut changed = 0;
        for mut rec in waiting.into_iter().filter(|r| !r.source_ready) {
            let now = self.now();
            let mut w = RunWrite::default();
            rec.run.set_error(RunError { code: RunErrorCode::SourceUnavailable, message: format!("source bundle for {sha} could not be built") });
            self.step_and_queue(rec, RunStatusEvent::InfraError, Initiator::Server, now, &mut w).await?;
            match self.d.runs.commit(org, w).await {
                Ok(_) => changed += 1,
                // A run that changed meanwhile is no longer waiting on this bundle as read.
                Err(e) if is_stale(&e) => {}
                Err(e) => return Err(e.into()),
            }
        }
        Ok(changed)
    }
}
