//! UC `index.version` (knowledge.md "UC `index.version`"): record the reason, skip superseded
//! default-branch heads, stop when every layer is done at the current revs, else reopen stale
//! layers and enqueue `index.fetch`.

use mage_core::domain::{IndexPriority, Layer, LayerEvent, LayerState, OrgId, RecipeStatus, RepoId, RepoVersion, VersionReason};
use mage_core::domain::{CommitSha, RepoVersionKey};
use mage_core::ports::repos::RepoStoreError;

use super::layers::{reasons, skip_pending};
use super::{IndexPipeline, IndexTarget, JobDone};
use crate::error::AppError;

impl IndexPipeline {
    pub async fn version(&self, org: OrgId, repo: RepoId, sha: CommitSha, reason: VersionReason) -> Result<JobDone, AppError> {
        let repos = self.deps.repos.as_ref();
        let key = RepoVersionKey::new(repo, sha);

        // 1-2. A DefaultHead job for a sha that is no longer the head must not move the head back.
        if reason == VersionReason::DefaultHead {
            match repos.default_head(org, repo).await? {
                Some(head) if head != sha => {
                    let version = match repos.version(org, key).await {
                        Ok(version) => version,
                        Err(RepoStoreError::NotFound) => return Ok(JobDone::done()),
                        Err(err) => return Err(err.into()),
                    };
                    if version.reasons.is_empty() {
                        skip_pending(repos, org, &version, &Layer::ALL, reasons::SUPERSEDED).await?;
                        return Ok(JobDone::done());
                    }
                }
                _ => {
                    repos.add_version_reason(org, repo, sha, reason.clone()).await?;
                }
            }
        } else {
            repos.add_version_reason(org, repo, sha, reason.clone()).await?;
        }
        let version = repos.version(org, key).await?;
        let current = self.current_revs();

        // 3. Nothing to do.
        let settled = Layer::ALL.iter().all(|l| matches!(version.layer(*l), Some(LayerState::Ready { .. } | LayerState::Skipped { .. })));
        if settled && version.revs == Some(current) {
            return Ok(JobDone::done());
        }

        // 4. First index: readers join facts at these revs once a layer is Ready.
        if version.revs.is_none() {
            repos.set_index_revs(org, key, &current).await?;
        }
        self.reopen_layers(org, &version).await?;
        let prio = version.reasons.iter().map(IndexPriority::from).max().unwrap_or_else(|| IndexPriority::from(&reason));
        Ok(JobDone::Complete(vec![IndexTarget::new(org, repo, sha, prio).fetch_job()]))
    }

    /// `Reset` closed layers (rev bump or manual reindex), then re-apply the standing skips:
    /// `Refs` without a validated recipe, `Semantic` until embeddings ship.
    async fn reopen_layers(&self, org: OrgId, version: &RepoVersion) -> Result<(), AppError> {
        let repos = self.deps.repos.as_ref();
        let key = version.key();
        for layer in Layer::ALL {
            if matches!(version.layer(layer), Some(LayerState::Failed { .. } | LayerState::Skipped { .. })) {
                repos.apply_layer_event(org, key, layer, LayerEvent::Reset, &[]).await?;
            }
        }
        let version = repos.version(org, key).await?;
        let validated = matches!(
            repos.latest_recipe(org, version.repo_id).await?,
            Some(recipe) if matches!(recipe.status, RecipeStatus::Validated { .. })
        );
        if !validated {
            skip_pending(repos, org, &version, &[Layer::Refs], reasons::NO_RECIPE).await?;
        }
        skip_pending(repos, org, &version, &[Layer::Semantic], reasons::DISABLED).await?;
        Ok(())
    }
}
