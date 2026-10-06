//! Layer state helpers over `RepoStore::apply_layer_event` (knowledge.md "Layer states"):
//! starting a layer (shadow rebuilds keep `Ready`), failing open layers, skipping dependents,
//! switching one component of `IndexRevs` after a rebuild, and the dead-letter effects per job
//! kind.

use mage_core::domain::{IndexRevs, JobPayload, Layer, LayerEvent, LayerState, OrgId, RepoVersion, RepoVersionKey};
use mage_core::ports::RepoStore;
use mage_core::ports::jobs::NewJob;
use mage_core::ports::repos::RepoStoreError;

use super::{IndexPipeline, IndexTarget};
use crate::error::AppError;

/// Skip reasons written by the pipeline.
pub mod reasons {
    pub const SUPERSEDED: &str = "superseded";
    pub const NO_RECIPE: &str = "no recipe";
    pub const DISABLED: &str = "disabled";
    pub const FILES_FAILED: &str = "files failed";
    pub const SYMBOLS_FAILED: &str = "symbols failed";
    pub const SHA_NOT_REACHABLE: &str = "sha not reachable";
    pub const ACCESS_REVOKED: &str = "source access revoked";
}

/// What a job should do with its layer, given the layer's current state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Started {
    /// The layer is `Running` (first run or retry).
    Fresh,
    /// The layer stays `Ready` while facts are rebuilt at a new rev.
    Rebuild,
    /// `Failed` or `Skipped`: nothing to do until a `Reset`.
    Closed,
}

/// `Start` unless the layer is `Ready` (shadow rebuild) or closed.
pub async fn start(repos: &dyn RepoStore, org: OrgId, key: RepoVersionKey, layer: Layer, state: Option<&LayerState>) -> Result<Started, AppError> {
    match state.unwrap_or(&LayerState::Pending) {
        LayerState::Pending | LayerState::Running => {
            repos.apply_layer_event(org, key, layer, LayerEvent::Start, &[]).await?;
            Ok(Started::Fresh)
        }
        LayerState::Ready { .. } => Ok(Started::Rebuild),
        LayerState::Failed { .. } | LayerState::Skipped { .. } => Ok(Started::Closed),
    }
}

/// Replace one component of the version's `IndexRevs` (the others keep the version's values, or
/// `current` when the version has none yet). No write when nothing changes.
pub async fn switch_rev(
    repos: &dyn RepoStore,
    org: OrgId,
    key: RepoVersionKey,
    current: IndexRevs,
    set: impl FnOnce(&mut IndexRevs),
) -> Result<(), AppError> {
    let version = repos.version(org, key).await?;
    let before = version.revs;
    let mut revs = before.unwrap_or(current);
    set(&mut revs);
    if before != Some(revs) {
        repos.set_index_revs(org, key, &revs).await?;
    }
    Ok(())
}

/// Every layer not yet closed ends `Failed { reason }` (`Pending` passes through `Running`, as
/// the transition table requires). `Ready`, `Failed` and `Skipped` layers are left alone.
pub async fn fail_open(repos: &dyn RepoStore, org: OrgId, key: RepoVersionKey, layers: &[Layer], reason: &str) -> Result<(), AppError> {
    let version = match repos.version(org, key).await {
        Ok(version) => version,
        Err(RepoStoreError::NotFound) => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    for layer in layers {
        fail_layer(repos, org, key, *layer, version.layer(*layer), reason).await?;
    }
    Ok(())
}

async fn fail_layer(repos: &dyn RepoStore, org: OrgId, key: RepoVersionKey, layer: Layer, state: Option<&LayerState>, reason: &str) -> Result<(), AppError> {
    match state.unwrap_or(&LayerState::Pending) {
        LayerState::Pending => {
            repos.apply_layer_event(org, key, layer, LayerEvent::Start, &[]).await?;
        }
        LayerState::Running => {}
        LayerState::Ready { .. } | LayerState::Failed { .. } | LayerState::Skipped { .. } => return Ok(()),
    }
    repos.apply_layer_event(org, key, layer, LayerEvent::Fail { reason: reason.to_owned() }, &[]).await?;
    Ok(())
}

/// `Skip { reason }` for each listed layer that is still `Pending`.
pub async fn skip_pending(repos: &dyn RepoStore, org: OrgId, version: &RepoVersion, layers: &[Layer], reason: &str) -> Result<(), AppError> {
    for layer in layers {
        if matches!(version.layer(*layer), None | Some(LayerState::Pending)) {
            repos.apply_layer_event(org, version.key(), *layer, LayerEvent::Skip { reason: reason.to_owned() }, &[]).await?;
        }
    }
    Ok(())
}

/// Layers that depend on `failed` (knowledge.md "Layer states", Dependencies).
pub fn dependents(failed: Layer) -> (&'static [Layer], &'static str) {
    match failed {
        Layer::Files => (&[Layer::Symbols, Layer::Refs, Layer::ErrorSites, Layer::AppMap, Layer::Semantic], reasons::FILES_FAILED),
        Layer::Symbols => (&[Layer::ErrorSites, Layer::AppMap, Layer::Semantic], reasons::SYMBOLS_FAILED),
        Layer::Refs | Layer::ErrorSites | Layer::AppMap | Layer::Semantic | Layer::History => (&[], ""),
    }
}

/// The layer whose state a job kind drives (`None` for jobs that are not a layer's).
pub fn layer_of(payload: &JobPayload) -> Option<Layer> {
    match payload {
        JobPayload::IndexManifest { .. } => Some(Layer::Files),
        JobPayload::IndexParse { .. } => Some(Layer::Symbols),
        JobPayload::IndexErrorSites { .. } => Some(Layer::ErrorSites),
        JobPayload::IndexAppMap { .. } => Some(Layer::AppMap),
        JobPayload::IndexHistory { .. } => Some(Layer::History),
        JobPayload::IndexIngestScip { .. } => Some(Layer::Refs),
        _ => None,
    }
}

impl IndexPipeline {
    /// Dead-letter effects of an `index.*` job (knowledge.md "Failure modes"): the job's layer
    /// `Failed { error }` and its dependents `Skipped`; a dead `index.fetch` fails every open
    /// layer; a dead `index.fetch` or `index.bundle_source` still enqueues `runs.source_ready` so
    /// waiting runs error out; the periodic jobs are rescheduled so their chain survives.
    /// Idempotent: closed layers are left alone.
    pub async fn on_dead(&self, payload: &JobPayload, error: &str) -> Result<Vec<NewJob>, AppError> {
        let repos = self.deps.repos.as_ref();
        let now = self.deps.clock.now();
        let reason = if error.is_empty() { "job dead-lettered".to_owned() } else { error.to_owned() };
        match payload {
            JobPayload::IndexFetch { org_id, repo_id, sha, prio } => {
                let target = IndexTarget::new(*org_id, *repo_id, *sha, *prio);
                fail_open(repos, target.org, target.key(), &Layer::ALL, &reason).await?;
                Ok(vec![target.source_ready_job()])
            }
            JobPayload::IndexBundleSource { org_id, repo_id, sha, prio } => {
                Ok(vec![IndexTarget::new(*org_id, *repo_id, *sha, *prio).source_ready_job()])
            }
            JobPayload::IndexReconcile => Ok(vec![NewJob::new(JobPayload::IndexReconcile).after(now + super::reconcile::INTERVAL)]),
            JobPayload::IndexGc => Ok(vec![NewJob::new(JobPayload::IndexGc).after(now + super::gc::INTERVAL)]),
            other => {
                let Some(layer) = layer_of(other) else { return Ok(Vec::new()) };
                let Some((org, key)) = version_of(other) else { return Ok(Vec::new()) };
                fail_open(repos, org, key, &[layer], &reason).await?;
                let (deps, skip_reason) = dependents(layer);
                if !deps.is_empty() {
                    match repos.version(org, key).await {
                        Ok(version) if matches!(version.layer(layer), Some(LayerState::Failed { .. })) => {
                            skip_pending(repos, org, &version, deps, skip_reason).await?;
                        }
                        Ok(_) | Err(RepoStoreError::NotFound) => {}
                        Err(err) => return Err(err.into()),
                    }
                }
                Ok(Vec::new())
            }
        }
    }
}

fn version_of(payload: &JobPayload) -> Option<(OrgId, RepoVersionKey)> {
    match payload {
        JobPayload::IndexManifest { org_id, repo_id, sha, .. }
        | JobPayload::IndexParse { org_id, repo_id, sha, .. }
        | JobPayload::IndexErrorSites { org_id, repo_id, sha, .. }
        | JobPayload::IndexAppMap { org_id, repo_id, sha, .. }
        | JobPayload::IndexHistory { org_id, repo_id, sha, .. }
        | JobPayload::IndexIngestScip { org_id, repo_id, sha, .. } => Some((*org_id, RepoVersionKey::new(*repo_id, *sha))),
        _ => None,
    }
}
