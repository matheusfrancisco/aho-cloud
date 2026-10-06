//! UC `index.reconcile` (knowledge.md "Reconcile and GC"), every 10 minutes: catch missed pushes
//! (default-branch head differs from the stored `DefaultHead`), enqueue rev rebuilds of retained
//! versions (newest first, rate-limited), apply the dead-letter effects of `index.*` jobs the
//! lease reaper killed, and re-enqueue the job of every layer stuck `Running` (the queue's dedupe
//! makes that a no-op while the job is alive).

use mage_core::domain::{IndexPriority, JobKind, JobPayload, Layer, LayerState, OrgId, RepoId, RepoVersion, VersionReason};
use mage_core::ports::common::PageRequest;
use mage_core::ports::jobs::NewJob;
use mage_core::ports::source_host::{RepoCoords, SourceHostError};
use time::OffsetDateTime;

use super::{IndexPipeline, IndexTarget, JobDone};
use crate::error::AppError;

/// Period of the self-rescheduling chain.
pub const INTERVAL: time::Duration = time::Duration::minutes(10);
/// Dead jobs finished within this window are (re)applied each tick.
const DEAD_WINDOW: time::Duration = time::Duration::minutes(25);
const DEAD_PAGES: usize = 5;
/// A version whose `Files` layer completed this recently is (being) rebuilt already.
const REBUILD_COOLDOWN: time::Duration = time::Duration::hours(1);
/// Job kinds whose death changes layer state or notifies runs.
const DEAD_KINDS: [JobKind; 8] = [
    JobKind::IndexFetch,
    JobKind::IndexManifest,
    JobKind::IndexParse,
    JobKind::IndexErrorSites,
    JobKind::IndexAppMap,
    JobKind::IndexHistory,
    JobKind::IndexBundleSource,
    JobKind::IndexIngestScip,
];

impl IndexPipeline {
    pub async fn reconcile(&self) -> Result<JobDone, AppError> {
        let now = self.deps.clock.now();
        let mut follow_ups = self.apply_recent_dead(now).await?;

        let current = self.current_revs();
        let mut rebuilds: Vec<(OffsetDateTime, NewJob)> = Vec::new();
        for (org, repo) in self.deps.repos.active_repos_system().await? {
            match self.reconcile_repo(org, repo, current, &mut follow_ups, &mut rebuilds, now).await {
                Ok(()) => {}
                Err(err) => tracing::warn!(org = %org, repo = %repo, error = %err, "reconcile skipped repo"),
            }
        }
        rebuilds.sort_by_key(|(created, _)| std::cmp::Reverse(*created));
        follow_ups.extend(rebuilds.into_iter().take(self.config.rebuilds_per_tick as usize).map(|(_, job)| job));
        follow_ups.push(NewJob::new(JobPayload::IndexReconcile).after(now + INTERVAL));
        Ok(JobDone::Complete(follow_ups))
    }

    async fn reconcile_repo(
        &self,
        org: OrgId,
        repo_id: RepoId,
        current: mage_core::domain::IndexRevs,
        follow_ups: &mut Vec<NewJob>,
        rebuilds: &mut Vec<(OffsetDateTime, NewJob)>,
        now: OffsetDateTime,
    ) -> Result<(), AppError> {
        let repos = self.deps.repos.as_ref();
        let repo = repos.repo(org, repo_id).await?;
        let coords = RepoCoords { owner: repo.owner.clone(), name: repo.name.clone() };
        match self.deps.source.resolve_ref(repo.installation_id, &coords, &repo.default_branch).await {
            Ok(head) => {
                if repos.default_head(org, repo_id).await? != Some(head) {
                    repos.set_default_head(org, repo_id, head).await?;
                    follow_ups.push(NewJob::new(JobPayload::IndexVersion { org_id: org, repo_id, sha: head, reason: VersionReason::DefaultHead }));
                }
            }
            // Access problems are intake's to surface; other failures retry next tick.
            Err(SourceHostError::Unauthorized | SourceHostError::NotFound) => {}
            Err(err) => tracing::warn!(org = %org, repo = %repo_id, error = %err, "default branch head not resolved"),
        }

        for version in repos.versions(org, repo_id).await? {
            let Some(reason) = top_reason(&version) else { continue };
            let prio = IndexPriority::from(&reason);
            let target = IndexTarget::new(org, repo_id, version.sha, prio);
            let mut running = false;
            for (layer, state) in &version.layers {
                if *state != LayerState::Running {
                    continue;
                }
                running = true;
                match layer {
                    Layer::Files => follow_ups.push(target.manifest_job()),
                    Layer::ErrorSites => follow_ups.push(target.error_sites_job()),
                    Layer::AppMap => follow_ups.push(target.app_map_job()),
                    Layer::History => follow_ups.push(target.history_job()),
                    // Symbols batches and Refs ingests carry data only their producer knows;
                    // their dead jobs are failed by `apply_recent_dead`.
                    Layer::Symbols | Layer::Refs | Layer::Semantic => {}
                }
            }
            // A shadow rebuild keeps its layers Ready, so "in flight" shows as a recent Files
            // completion: give it REBUILD_COOLDOWN before asking again.
            let rebuilding = matches!(version.layer(Layer::Files), Some(LayerState::Ready { at }) if *at > now - REBUILD_COOLDOWN);
            if !running && !rebuilding && version.revs != Some(current) {
                let job = NewJob::new(JobPayload::IndexVersion { org_id: org, repo_id, sha: version.sha, reason });
                rebuilds.push((version.created_at, job));
            }
        }
        Ok(())
    }

    /// Dead-letter effects for `index.*` jobs that died within [`DEAD_WINDOW`] (jobs killed by
    /// `reap_expired` never reach the worker's dead-letter path). Idempotent.
    async fn apply_recent_dead(&self, now: OffsetDateTime) -> Result<Vec<NewJob>, AppError> {
        let mut out = Vec::new();
        for kind in DEAD_KINDS {
            let mut page = PageRequest::first(PageRequest::MAX_LIMIT);
            'pages: for _ in 0..DEAD_PAGES {
                let dead = self.deps.jobs.list_dead(Some(kind), page.clone()).await?;
                for record in &dead.items {
                    if record.finished_at.is_some_and(|at| at < now - DEAD_WINDOW) {
                        break 'pages;
                    }
                    out.extend(self.on_dead(&record.payload, record.last_error.as_deref().unwrap_or("")).await?);
                }
                match dead.next {
                    Some(next) => page = page.after(next),
                    None => break,
                }
            }
        }
        Ok(out)
    }
}

/// The version's highest-priority current reason.
fn top_reason(version: &RepoVersion) -> Option<VersionReason> {
    version.reasons.iter().max_by_key(|reason| IndexPriority::from(*reason)).cloned()
}
