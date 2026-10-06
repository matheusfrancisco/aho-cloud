//! Indexer job worker (knowledge.md "Job DAG per commit", "Indexer worker concurrency"): leases
//! `index.*` jobs of the `Indexer` lane per pool (`fetch` 2, `parse` = cores, `bundle_source` 2,
//! others 4), heartbeats every `lease / 3`, dispatches each job through the dispatch table
//! ([`IndexHandlers`]), and maps the outcome onto the queue:
//!
//! | Handler result | Queue call |
//! |---|---|
//! | `Ok(JobDone::Complete(follow_ups))` | `complete(lease, follow_ups)` |
//! | `Ok(JobDone::Defer(at))` | `defer(lease, at)` |
//! | `Err(RateLimited { retry_at })` | `fail(Retryable { retry_at })` |
//! | `Err(Unavailable \| Internal \| Conflict)` | `fail(Retryable)` (backoff) |
//! | `Err(NotFound \| Invalid \| Domain)` | `fail(Permanent)` |
//! | heartbeat `CancelRequested` | handler dropped, `fail(Cancelled)` |
//!
//! When `fail` reports the job dead, the dispatch table's dead-letter effects run (layer
//! `Failed` + dependents `Skipped`, `runs.source_ready`, periodic chains rescheduled) and their
//! jobs are enqueued. The worker also reaps expired leases periodically.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mage_core::domain::{JobKind, JobLane, JobPayload};
use mage_core::ports::jobs::{FailOutcome, Heartbeat, JobError, JobFailure, JobQueue, LeaseRequest, LeasedJob, NewJob};
use serde::Deserialize;
use tokio::sync::{Notify, Semaphore};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::app::index::appmap::IndexAppMap;
use crate::app::index::history::IndexHistory;
use crate::app::index::ingest_scip::IngestScip;
use crate::app::index::{IndexPipeline, IndexTarget, JobDone};
use crate::error::AppError;

/// `[indexer.workers]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct WorkersConfig {
    #[serde(default = "two")]
    pub fetch: usize,
    /// `0` = available cores.
    #[serde(default)]
    pub parse: usize,
    #[serde(default = "two")]
    pub bundle_source: usize,
    /// Every other `index.*` kind.
    #[serde(default = "four")]
    pub other: usize,
    /// Idle wait between lease rounds.
    #[serde(default = "poll_ms")]
    pub poll_interval_ms: u64,
    /// `reap_expired` cadence.
    #[serde(default = "reap_secs")]
    pub reap_interval_secs: u64,
    /// In-flight jobs get this long to finish after shutdown before they are aborted (their
    /// leases expire and the jobs are re-leased).
    #[serde(default = "grace_secs")]
    pub shutdown_grace_secs: u64,
}

fn two() -> usize {
    2
}
fn four() -> usize {
    4
}
fn poll_ms() -> u64 {
    1000
}
fn reap_secs() -> u64 {
    30
}
fn grace_secs() -> u64 {
    20
}

impl Default for WorkersConfig {
    fn default() -> Self {
        Self {
            fetch: two(),
            parse: 0,
            bundle_source: two(),
            other: four(),
            poll_interval_ms: poll_ms(),
            reap_interval_secs: reap_secs(),
            shutdown_grace_secs: grace_secs(),
        }
    }
}

/// Runs one job and its dead-letter effects.
#[async_trait]
pub trait Dispatch: Send + Sync {
    async fn handle(&self, payload: &JobPayload) -> Result<JobDone, AppError>;
    /// Effects of a job that died with `error`; returned jobs are enqueued.
    async fn on_dead(&self, payload: &JobPayload, error: &str) -> Result<Vec<NewJob>, AppError>;
}

/// The dispatch table: every `index.*` kind to its use case.
pub struct IndexHandlers {
    pub pipeline: Arc<IndexPipeline>,
    pub app_map: Arc<IndexAppMap>,
    pub history: Arc<IndexHistory>,
    pub ingest_scip: Arc<IngestScip>,
}

impl IndexHandlers {
    pub fn new(pipeline: Arc<IndexPipeline>, app_map: Arc<IndexAppMap>, history: Arc<IndexHistory>, ingest_scip: Arc<IngestScip>) -> Self {
        Self { pipeline, app_map, history, ingest_scip }
    }
}

#[async_trait]
impl Dispatch for IndexHandlers {
    async fn handle(&self, payload: &JobPayload) -> Result<JobDone, AppError> {
        match payload {
            JobPayload::IndexVersion { org_id, repo_id, sha, reason } => self.pipeline.version(*org_id, *repo_id, *sha, reason.clone()).await,
            JobPayload::IndexFetch { org_id, repo_id, sha, prio } => self.pipeline.fetch(IndexTarget::new(*org_id, *repo_id, *sha, *prio)).await,
            JobPayload::IndexManifest { org_id, repo_id, sha, prio } => {
                self.pipeline.manifest(IndexTarget::new(*org_id, *repo_id, *sha, *prio)).await
            }
            JobPayload::IndexParse { org_id, repo_id, sha, prio, batch, oids } => {
                self.pipeline.parse(IndexTarget::new(*org_id, *repo_id, *sha, *prio), *batch, oids).await
            }
            JobPayload::IndexErrorSites { org_id, repo_id, sha, prio } => {
                self.pipeline.error_sites(IndexTarget::new(*org_id, *repo_id, *sha, *prio)).await
            }
            JobPayload::IndexBundleSource { org_id, repo_id, sha, prio } => {
                self.pipeline.bundle_source(IndexTarget::new(*org_id, *repo_id, *sha, *prio)).await
            }
            JobPayload::IndexAppMap { org_id, repo_id, sha, prio } => self.app_map.run(*org_id, *repo_id, *sha, *prio).await,
            JobPayload::IndexHistory { org_id, repo_id, sha, prio } => self.history.run(*org_id, *repo_id, *sha, *prio).await,
            JobPayload::IndexIngestScip { org_id, repo_id, sha, run_id, artifact_id, key } => {
                self.ingest_scip.run(*org_id, *repo_id, *sha, *run_id, *artifact_id, key).await
            }
            JobPayload::IndexReconcile => self.pipeline.reconcile().await,
            JobPayload::IndexGc => self.pipeline.gc().await,
            other => Err(AppError::Invalid(format!("{} is not an indexer job", other.kind()))),
        }
    }

    async fn on_dead(&self, payload: &JobPayload, error: &str) -> Result<Vec<NewJob>, AppError> {
        self.pipeline.on_dead(payload, error).await
    }
}

/// How the queue is told about a failed handler.
pub fn failure_of(err: &AppError) -> JobFailure {
    let error = err.to_string();
    match err {
        AppError::RateLimited { retry_at } => JobFailure::Retryable { error, retry_at: Some(*retry_at) },
        AppError::Unavailable(_) | AppError::Internal(_) | AppError::Conflict(_) => JobFailure::Retryable { error, retry_at: None },
        AppError::NotFound(_) | AppError::Invalid(_) | AppError::Domain(_) => JobFailure::Permanent { error },
    }
}

struct Pool {
    name: &'static str,
    kinds: Vec<JobKind>,
    slots: Arc<Semaphore>,
}

pub struct IndexWorker {
    jobs: Arc<dyn JobQueue>,
    dispatch: Arc<dyn Dispatch>,
    worker_id: String,
    config: WorkersConfig,
    pools: Vec<Pool>,
    /// Signalled when jobs of the lane are enqueued (e.g. `mage_infra::pg::jobs::spawn_wakeup_listener`).
    wakeup: Option<Arc<Notify>>,
}

enum Outcome {
    Done(Result<JobDone, AppError>),
    Cancelled,
    LeaseLost,
}

impl IndexWorker {
    pub fn new(jobs: Arc<dyn JobQueue>, dispatch: Arc<dyn Dispatch>, worker_id: impl Into<String>, config: WorkersConfig) -> Self {
        let cores = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
        let parse = if config.parse == 0 { cores } else { config.parse };
        let others: Vec<JobKind> = JobKind::all()
            .iter()
            .copied()
            .filter(|kind| kind.lane() == JobLane::Indexer && !matches!(kind, JobKind::IndexFetch | JobKind::IndexParse | JobKind::IndexBundleSource))
            .collect();
        let pool = |name, kinds, size: usize| Pool { name, kinds, slots: Arc::new(Semaphore::new(size.max(1))) };
        let pools = vec![
            pool("fetch", vec![JobKind::IndexFetch], config.fetch),
            pool("parse", vec![JobKind::IndexParse], parse),
            pool("bundle_source", vec![JobKind::IndexBundleSource], config.bundle_source),
            pool("other", others, config.other),
        ];
        Self { jobs, dispatch, worker_id: worker_id.into(), config, pools, wakeup: None }
    }

    /// Cut idle waits short when `wakeup` is notified (new jobs in the lane).
    pub fn with_wakeup(mut self, wakeup: Arc<Notify>) -> Self {
        self.wakeup = Some(wakeup);
        self
    }

    /// Lease and run jobs until `shutdown`; in-flight jobs get `shutdown_grace_secs` to finish.
    pub async fn run(self: Arc<Self>, shutdown: CancellationToken) {
        let poll = Duration::from_millis(self.config.poll_interval_ms.max(10));
        let reap_every = Duration::from_secs(self.config.reap_interval_secs.max(1));
        let mut last_reap: Option<tokio::time::Instant> = None;
        let mut tasks = JoinSet::new();
        tracing::info!(worker = %self.worker_id, "indexer worker started");
        // The periodic chains reschedule themselves; seeding is a no-op while one is queued.
        let seeds = [NewJob::new(JobPayload::IndexReconcile), NewJob::new(JobPayload::IndexGc)];
        if let Err(err) = self.jobs.enqueue_all(&seeds).await {
            tracing::warn!(error = %err, "index.reconcile / index.gc not seeded");
        }
        while !shutdown.is_cancelled() {
            while tasks.try_join_next().is_some() {}
            let mut leased = 0usize;
            for pool in &self.pools {
                let free = pool.slots.available_permits();
                if free == 0 {
                    continue;
                }
                let request = LeaseRequest {
                    lane: JobLane::Indexer,
                    kinds: Some(pool.kinds.clone()),
                    worker: self.worker_id.clone(),
                    max: u16::try_from(free).unwrap_or(u16::MAX),
                };
                match self.jobs.lease(&request).await {
                    Ok(jobs) => {
                        for job in jobs {
                            let Ok(permit) = pool.slots.clone().acquire_owned().await else { continue };
                            leased += 1;
                            let worker = Arc::clone(&self);
                            tasks.spawn(async move {
                                worker.process(job).await;
                                drop(permit);
                            });
                        }
                    }
                    Err(err) => tracing::warn!(pool = pool.name, error = %err, "lease failed"),
                }
            }
            if last_reap.is_none_or(|at| at.elapsed() >= reap_every) {
                last_reap = Some(tokio::time::Instant::now());
                match self.jobs.reap_expired(100).await {
                    Ok(0) => {}
                    Ok(n) => tracing::info!(reaped = n, "expired job leases reaped"),
                    Err(err) => tracing::warn!(error = %err, "reap_expired failed"),
                }
            }
            if leased == 0 {
                let woken = async {
                    match &self.wakeup {
                        Some(notify) => notify.notified().await,
                        None => std::future::pending().await,
                    }
                };
                tokio::select! {
                    () = tokio::time::sleep(poll) => {}
                    () = woken => {}
                    () = shutdown.cancelled() => {}
                }
            }
        }
        let grace = Duration::from_secs(self.config.shutdown_grace_secs);
        if tokio::time::timeout(grace, async { while tasks.join_next().await.is_some() {} }).await.is_err() {
            tracing::warn!(in_flight = tasks.len(), "indexer jobs aborted at shutdown; their leases will expire");
            tasks.shutdown().await;
        }
        tracing::info!(worker = %self.worker_id, "indexer worker stopped");
    }

    /// Lease and run ready `index.*` jobs one at a time until none is ready or `max_jobs` ran;
    /// returns how many ran. For tests and one-shot runs.
    pub async fn run_until_idle(&self, max_jobs: usize) -> Result<usize, JobError> {
        let mut ran = 0;
        while ran < max_jobs {
            let request = LeaseRequest { lane: JobLane::Indexer, kinds: None, worker: self.worker_id.clone(), max: 1 };
            let Some(job) = self.jobs.lease(&request).await?.into_iter().next() else { break };
            self.process(job).await;
            ran += 1;
        }
        Ok(ran)
    }

    /// Run one leased job to its queue outcome.
    pub async fn process(&self, job: LeasedJob) {
        let lease = job.lease;
        let beat = (job.kind.retry_policy().lease / 3).max(Duration::from_secs(1));
        let outcome = {
            let handler = self.dispatch.handle(&job.payload);
            tokio::pin!(handler);
            let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + beat, beat);
            loop {
                tokio::select! {
                    result = &mut handler => break Outcome::Done(result),
                    _ = ticker.tick() => match self.jobs.heartbeat(&lease).await {
                        Ok(Heartbeat::Extended { .. }) => {}
                        Ok(Heartbeat::CancelRequested { .. }) => break Outcome::Cancelled,
                        Err(JobError::LeaseLost) => break Outcome::LeaseLost,
                        Err(err) => tracing::warn!(kind = %job.kind, error = %err, "heartbeat failed"),
                    },
                }
            }
        };
        let kind = job.kind;
        let result = match outcome {
            Outcome::Done(Ok(JobDone::Complete(follow_ups))) => self.jobs.complete(&lease, &follow_ups).await,
            Outcome::Done(Ok(JobDone::Defer(until))) => self.jobs.defer(&lease, until).await,
            Outcome::Done(Err(err)) => {
                tracing::warn!(%kind, attempt = job.attempt, error = %err, "indexer job failed");
                match self.jobs.fail(&lease, failure_of(&err)).await {
                    Ok(FailOutcome::Dead) => {
                        self.dead_letter(&job, &err.to_string()).await;
                        Ok(())
                    }
                    Ok(FailOutcome::Retrying { .. } | FailOutcome::Cancelled) => Ok(()),
                    Err(err) => Err(err),
                }
            }
            Outcome::Cancelled => self.jobs.fail(&lease, JobFailure::Cancelled).await.map(|_| ()),
            Outcome::LeaseLost => Err(JobError::LeaseLost),
        };
        match result {
            Ok(()) => {}
            Err(JobError::LeaseLost) => tracing::info!(%kind, "job lease lost; result dropped"),
            Err(err) => tracing::warn!(%kind, error = %err, "job outcome not recorded"),
        }
    }

    async fn dead_letter(&self, job: &LeasedJob, error: &str) {
        let jobs = match self.dispatch.on_dead(&job.payload, error).await {
            Ok(jobs) => jobs,
            Err(err) => {
                tracing::error!(kind = %job.kind, error = %err, "dead-letter effects failed; index.reconcile retries them");
                return;
            }
        };
        if !jobs.is_empty() {
            if let Err(err) = self.jobs.enqueue_all(&jobs).await {
                tracing::error!(kind = %job.kind, error = %err, "dead-letter follow-ups not enqueued");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use mage_core::domain::{CommitSha, IndexPriority, OrgId, RepoId};
    use mage_core::ports::Clock;
    use mage_core::ports::jobs::JobState;
    use mage_core::testing::{FakeClock, MemJobQueue, SeqIdGen};
    use parking_lot::Mutex;
    use time::macros::datetime;
    use uuid::Uuid;

    use super::*;

    /// Scripted dispatch: one result per call (by kind), records dead letters.
    struct Script {
        results: Mutex<Vec<(JobKind, Result<JobDone, AppError>)>>,
        dead: Mutex<Vec<(JobKind, String)>>,
    }

    impl Script {
        fn new(results: Vec<(JobKind, Result<JobDone, AppError>)>) -> Self {
            Self { results: Mutex::new(results), dead: Mutex::new(Vec::new()) }
        }
    }

    #[async_trait]
    impl Dispatch for Script {
        async fn handle(&self, payload: &JobPayload) -> Result<JobDone, AppError> {
            let mut results = self.results.lock();
            let at = results.iter().position(|(kind, _)| *kind == payload.kind()).expect("scripted result");
            results.remove(at).1
        }

        async fn on_dead(&self, payload: &JobPayload, error: &str) -> Result<Vec<NewJob>, AppError> {
            self.dead.lock().push((payload.kind(), error.to_owned()));
            match payload {
                JobPayload::IndexBundleSource { org_id, repo_id, sha, .. } => {
                    Ok(vec![NewJob::new(JobPayload::RunsSourceReady { org_id: *org_id, repo_id: *repo_id, sha: *sha })])
                }
                _ => Ok(Vec::new()),
            }
        }
    }

    fn ids() -> (OrgId, RepoId, CommitSha) {
        (OrgId::from_uuid(Uuid::from_u128(1)), RepoId::from_uuid(Uuid::from_u128(2)), CommitSha::from_bytes([7; 20]))
    }

    fn queue() -> (Arc<FakeClock>, Arc<MemJobQueue>) {
        let clock = Arc::new(FakeClock::at(datetime!(2026-10-05 10:00 UTC)));
        let jobs = Arc::new(MemJobQueue::new(clock.clone(), Arc::new(SeqIdGen::new())));
        (clock, jobs)
    }

    fn state_of(jobs: &MemJobQueue, kind: JobKind) -> Vec<JobState> {
        jobs.jobs().into_iter().filter(|j| j.kind == kind).map(|j| j.state).collect()
    }

    #[tokio::test]
    async fn complete_enqueues_follow_ups_and_retryable_errors_retry() {
        let (org, repo, sha) = ids();
        let (_clock, jobs) = queue();
        let t = IndexTarget::new(org, repo, sha, IndexPriority::High);
        jobs.enqueue(t.fetch_job()).await.unwrap();
        jobs.enqueue(t.manifest_job()).await.unwrap();
        let script = Arc::new(Script::new(vec![
            (JobKind::IndexFetch, Ok(JobDone::Complete(vec![t.history_job()]))),
            (JobKind::IndexManifest, Err(AppError::Unavailable("db down".into()))),
            (JobKind::IndexHistory, Ok(JobDone::done())),
        ]));
        let worker = IndexWorker::new(jobs.clone(), script.clone(), "w1", WorkersConfig::default());
        let ran = worker.run_until_idle(10).await.unwrap();
        assert_eq!(ran, 3, "fetch, manifest (fails, retry later), history");
        assert_eq!(state_of(&jobs, JobKind::IndexFetch), vec![JobState::Succeeded]);
        assert_eq!(state_of(&jobs, JobKind::IndexHistory), vec![JobState::Succeeded]);
        let manifest = jobs.jobs().into_iter().find(|j| j.kind == JobKind::IndexManifest).unwrap();
        assert_eq!(manifest.state, JobState::Queued);
        assert!(manifest.last_error.unwrap().contains("db down"));
        assert!(script.dead.lock().is_empty());
    }

    #[tokio::test]
    async fn permanent_errors_dead_letter_and_enqueue_its_jobs() {
        let (org, repo, sha) = ids();
        let (_clock, jobs) = queue();
        let t = IndexTarget::new(org, repo, sha, IndexPriority::Normal);
        jobs.enqueue(t.bundle_job()).await.unwrap();
        let script = Arc::new(Script::new(vec![(JobKind::IndexBundleSource, Err(AppError::Invalid("bundle too large".into())))]));
        let worker = IndexWorker::new(jobs.clone(), script.clone(), "w1", WorkersConfig::default());
        worker.run_until_idle(10).await.unwrap();
        assert_eq!(state_of(&jobs, JobKind::IndexBundleSource), vec![JobState::Dead]);
        assert_eq!(script.dead.lock().as_slice(), &[(JobKind::IndexBundleSource, "invalid: bundle too large".to_owned())]);
        // runs.source_ready is on the server lane: queued, not run by the indexer worker.
        assert_eq!(state_of(&jobs, JobKind::RunsSourceReady), vec![JobState::Queued]);
    }

    #[tokio::test]
    async fn retryable_errors_dead_letter_once_attempts_run_out() {
        let (org, repo, sha) = ids();
        let (clock, jobs) = queue();
        let t = IndexTarget::new(org, repo, sha, IndexPriority::Normal);
        jobs.enqueue(t.error_sites_job().with_max_attempts(2)).await.unwrap();
        let script = Arc::new(Script::new(vec![
            (JobKind::IndexErrorSites, Err(AppError::Unavailable("a".into()))),
            (JobKind::IndexErrorSites, Err(AppError::Unavailable("b".into()))),
        ]));
        let worker = IndexWorker::new(jobs.clone(), script.clone(), "w1", WorkersConfig::default());
        assert_eq!(worker.run_until_idle(10).await.unwrap(), 1);
        assert!(script.dead.lock().is_empty());
        clock.advance(Duration::from_secs(3600));
        assert_eq!(worker.run_until_idle(10).await.unwrap(), 1);
        assert_eq!(state_of(&jobs, JobKind::IndexErrorSites), vec![JobState::Dead]);
        assert_eq!(script.dead.lock().len(), 1);
    }

    #[tokio::test]
    async fn rate_limits_retry_at_the_given_time_and_defer_keeps_the_attempt() {
        let (org, repo, sha) = ids();
        let (clock, jobs) = queue();
        let t = IndexTarget::new(org, repo, sha, IndexPriority::Normal);
        jobs.enqueue(t.history_job()).await.unwrap();
        jobs.enqueue(t.app_map_job()).await.unwrap();
        let retry_at = clock.now() + time::Duration::minutes(7);
        let defer_to = clock.now() + time::Duration::minutes(3);
        let script = Arc::new(Script::new(vec![
            (JobKind::IndexHistory, Err(AppError::RateLimited { retry_at })),
            (JobKind::IndexAppMap, Ok(JobDone::Defer(defer_to))),
        ]));
        let worker = IndexWorker::new(jobs.clone(), script, "w1", WorkersConfig::default());
        worker.run_until_idle(10).await.unwrap();
        let all = jobs.jobs();
        let history = all.iter().find(|j| j.kind == JobKind::IndexHistory).unwrap();
        assert_eq!((history.state, history.run_after), (JobState::Queued, retry_at));
        let app_map = all.iter().find(|j| j.kind == JobKind::IndexAppMap).unwrap();
        assert_eq!((app_map.state, app_map.run_after, app_map.attempts), (JobState::Queued, defer_to, 0));
    }

    #[test]
    fn failure_mapping() {
        let at = datetime!(2026-10-05 10:00 UTC);
        assert!(matches!(failure_of(&AppError::RateLimited { retry_at: at }), JobFailure::Retryable { retry_at: Some(t), .. } if t == at));
        for err in [AppError::Unavailable("x".into()), AppError::Internal("x".into()), AppError::Conflict("x".into())] {
            assert!(matches!(failure_of(&err), JobFailure::Retryable { retry_at: None, .. }), "{err}");
        }
        for err in [AppError::NotFound("x".into()), AppError::Invalid("x".into())] {
            assert!(matches!(failure_of(&err), JobFailure::Permanent { .. }), "{err}");
        }
    }
}
