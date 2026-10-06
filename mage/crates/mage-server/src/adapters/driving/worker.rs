//! Server-lane job worker (server.md "Background jobs: decision", storage.md "Worker loop" and
//! "Periodic chains"). Runs inside every server instance.
//!
//! Loop: lease up to the free slots of the registered kinds → run each job's handler next to a
//! heartbeat task → `complete` / `fail` / `defer` by the handler's outcome. Nothing leased → wait
//! for a wakeup ([`Worker::wake_handle`], fed by the `mage_jobs` LISTEN connection), a finished
//! job, or the poll interval. Lease errors back off exponentially. Every 15 s `reap_expired`.
//! Periodic sources ([`Periodic`]) enqueue their jobs at start and every period. Shutdown: stop
//! leasing, give running handlers the grace period, abandon the rest (their leases expire and
//! are reaped; handlers are idempotent).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use time::OffsetDateTime;
use tokio::sync::Notify;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{Instrument, debug, error, info, warn};

use mage_core::domain::job::{JobKind, JobLane, JobPayload};
use mage_core::ports::Clock;
use mage_core::ports::jobs::{Heartbeat, JobError, JobFailure, JobQueue, LeaseRequest, LeasedJob, NewJob};

use crate::error::AppError;

/// One area's handlers. `kinds` are `JobKind::as_str()` values (`"intake.github"`, …).
#[async_trait]
pub trait JobHandler: Send + Sync {
    fn kinds(&self) -> &'static [&'static str];
    async fn handle(&self, job: &LeasedJob) -> Result<JobDone, AppError>;
}

/// A handler's successful outcome. Failures are `Err(AppError)` (see [`failure_for`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobDone {
    /// Succeeded; `follow_ups` are enqueued in the same transaction (chains, fan-out).
    Complete { follow_ups: Vec<NewJob> },
    /// Not ready yet: requeue at `until` without consuming the attempt.
    Defer { until: OffsetDateTime },
    /// Retryable failure at an exact time (provider rate limit with a reset time).
    Retry { at: OffsetDateTime, error: String },
}

impl JobDone {
    pub fn complete() -> JobDone {
        JobDone::Complete { follow_ups: Vec::new() }
    }

    pub fn follow_up(jobs: Vec<NewJob>) -> JobDone {
        JobDone::Complete { follow_ups: jobs }
    }
}

/// How a handler error is reported to the queue: rate limits retry at the advertised time,
/// transient errors retry with the kind's backoff, everything else is permanent (the job dies
/// and lands in the dead-letter listing).
pub fn failure_for(error: &AppError, now: OffsetDateTime) -> JobFailure {
    let message = error.to_string();
    match error {
        AppError::RateLimited { retry_after_secs: Some(secs) } => {
            let secs = i64::try_from(*secs).unwrap_or(i64::MAX / 2);
            JobFailure::Retryable { error: message, retry_at: Some(now + time::Duration::seconds(secs)) }
        }
        AppError::RateLimited { retry_after_secs: None } | AppError::Unavailable(_) | AppError::Conflict(_) | AppError::Internal(_) => {
            JobFailure::Retryable { error: message, retry_at: None }
        }
        AppError::Invalid(_) | AppError::NotFound(_) | AppError::Forbidden(_) | AppError::Unauthorized | AppError::Domain(_) => {
            JobFailure::Permanent { error: message }
        }
    }
}

/// A source of jobs enqueued on a schedule: a ticker (`{kind}:{bucket}` dedupe) or a chain
/// bootstrap (fixed dedupe key per `(kind, org)`; `Duplicate` while the chain is alive).
#[async_trait]
pub trait Periodic: Send + Sync {
    fn name(&self) -> &str;
    fn period(&self) -> Duration;
    async fn jobs(&self, now: OffsetDateTime) -> Result<Vec<NewJob>, AppError>;
}

/// Platform ticker: one `payload` job per time bucket of `period` (dedupe `{kind}:{bucket}`,
/// bucket = unix seconds / period), so every instance can tick and one job per bucket runs.
pub struct Ticker {
    payload: JobPayload,
    period: Duration,
}

impl Ticker {
    pub fn new(payload: JobPayload, period: Duration) -> Ticker {
        Ticker { payload, period }
    }

    pub fn bucket_key(&self, now: OffsetDateTime) -> String {
        let period = self.period.as_secs().max(1);
        let bucket = u64::try_from(now.unix_timestamp()).unwrap_or(0) / period;
        format!("{}:{bucket}", self.payload.kind())
    }
}

#[async_trait]
impl Periodic for Ticker {
    fn name(&self) -> &str {
        self.payload.kind().as_str()
    }

    fn period(&self) -> Duration {
        self.period
    }

    async fn jobs(&self, now: OffsetDateTime) -> Result<Vec<NewJob>, AppError> {
        Ok(vec![NewJob::new(self.payload.clone()).dedupe(self.bucket_key(now))])
    }
}

/// `[server.jobs]` (server.md config).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobsConfig {
    /// Concurrent jobs per instance.
    #[serde(default = "JobsConfig::default_slots")]
    pub slots: u16,
    /// Kind prefixes this instance leases (`"intake."`, …); empty = every registered kind.
    #[serde(default)]
    pub kinds: Vec<String>,
}

impl JobsConfig {
    fn default_slots() -> u16 {
        8
    }
}

impl Default for JobsConfig {
    fn default() -> Self {
        JobsConfig { slots: Self::default_slots(), kinds: Vec::new() }
    }
}

/// Loop timings; defaults per storage.md "Worker loop" and server.md "Shutdown".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerTimings {
    /// Wait when nothing was leased (covers delayed `run_after` and lost notifications).
    pub poll: Duration,
    pub reap_every: Duration,
    pub reap_batch: u32,
    /// Running handlers get this long after shutdown before they are abandoned.
    pub shutdown_grace: Duration,
    /// Lease errors back off from `min` doubling to `max`.
    pub backoff_min: Duration,
    pub backoff_max: Duration,
    /// Heartbeat period; `None` = the kind's `lease / 3`.
    pub heartbeat: Option<Duration>,
}

impl Default for WorkerTimings {
    fn default() -> Self {
        WorkerTimings {
            poll: Duration::from_secs(2),
            reap_every: Duration::from_secs(15),
            reap_batch: 100,
            shutdown_grace: Duration::from_secs(7),
            backoff_min: Duration::from_millis(500),
            backoff_max: Duration::from_secs(30),
            heartbeat: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorkerError {
    #[error("handler declares unknown job kind `{0}`")]
    UnknownKind(String),
    #[error("job kind `{0}` is not a server-lane kind")]
    WrongLane(String),
    #[error("job kind `{0}` has two handlers")]
    DuplicateKind(String),
}

/// What happened to one leased job (returned by [`Worker::run_once`] for tests and logs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobOutcome {
    Completed,
    Deferred,
    Failed(JobFailure),
    /// The lease was lost (reaped, re-leased); the result was dropped.
    LeaseLost,
    /// `cancel` was requested; the handler was dropped and the job marked cancelled.
    Cancelled,
    /// The queue call after the handler failed; the lease will expire and be reaped.
    QueueError(String),
}

pub struct Worker {
    queue: Arc<dyn JobQueue>,
    clock: Arc<dyn Clock>,
    handlers: BTreeMap<JobKind, Arc<dyn JobHandler>>,
    periodic: Vec<Arc<dyn Periodic>>,
    config: JobsConfig,
    timings: WorkerTimings,
    worker_id: String,
    wake: Arc<Notify>,
}

impl Worker {
    /// `worker_id` is written to `jobs.leased_by` (e.g. Cloud Run instance id + pid).
    pub fn new(queue: Arc<dyn JobQueue>, clock: Arc<dyn Clock>, config: JobsConfig, worker_id: impl Into<String>) -> Worker {
        Worker {
            queue,
            clock,
            handlers: BTreeMap::new(),
            periodic: Vec::new(),
            config,
            timings: WorkerTimings::default(),
            worker_id: worker_id.into(),
            wake: Arc::new(Notify::new()),
        }
    }

    pub fn with_timings(mut self, timings: WorkerTimings) -> Worker {
        self.timings = timings;
        self
    }

    /// Registers `handler` for each of its kinds. Kinds outside `config.kinds` prefixes are
    /// accepted but never leased by this instance.
    pub fn register(&mut self, handler: Arc<dyn JobHandler>) -> Result<(), WorkerError> {
        for name in handler.kinds() {
            let kind: JobKind = name.parse().map_err(|_| WorkerError::UnknownKind((*name).to_owned()))?;
            if kind.lane() != JobLane::Server {
                return Err(WorkerError::WrongLane((*name).to_owned()));
            }
            if self.handlers.insert(kind, handler.clone()).is_some() {
                return Err(WorkerError::DuplicateKind((*name).to_owned()));
            }
        }
        Ok(())
    }

    pub fn add_periodic(&mut self, source: Arc<dyn Periodic>) {
        self.periodic.push(source);
    }

    /// Notify on every `mage_jobs` notification for the server lane to cut the poll wait.
    pub fn wake_handle(&self) -> Arc<Notify> {
        self.wake.clone()
    }

    /// Kinds this instance leases: registered, filtered by the configured prefixes.
    pub fn leased_kinds(&self) -> Vec<JobKind> {
        self.handlers
            .keys()
            .copied()
            .filter(|kind| self.config.kinds.is_empty() || self.config.kinds.iter().any(|p| kind.as_str().starts_with(p.as_str())))
            .collect()
    }

    fn lease_request(&self, max: u16) -> LeaseRequest {
        LeaseRequest { lane: JobLane::Server, kinds: Some(self.leased_kinds()), worker: self.worker_id.clone(), max }
    }

    /// Leases up to `slots` ready jobs and runs them to the end; returns their outcomes. The
    /// building block of [`Worker::run`], exposed for tests and one-shot CLI runs.
    pub async fn run_once(self: &Arc<Self>) -> Result<Vec<JobOutcome>, JobError> {
        let jobs = self.queue.lease(&self.lease_request(self.config.slots.max(1))).await?;
        let mut set = JoinSet::new();
        for job in jobs {
            let this = self.clone();
            set.spawn(async move { this.execute(job).await });
        }
        let mut outcomes = Vec::new();
        while let Some(done) = set.join_next().await {
            match done {
                Ok(outcome) => outcomes.push(outcome),
                Err(e) => outcomes.push(JobOutcome::QueueError(format!("handler task panicked: {e}"))),
            }
        }
        Ok(outcomes)
    }

    /// Enqueues every periodic source's jobs once (start of [`Worker::run`]).
    pub async fn tick_periodic(&self) {
        for source in &self.periodic {
            self.tick(source.as_ref()).await;
        }
    }

    async fn tick(&self, source: &dyn Periodic) {
        let now = self.clock.now();
        match source.jobs(now).await {
            Ok(jobs) if jobs.is_empty() => {}
            Ok(jobs) => {
                if let Err(e) = self.queue.enqueue_all(&jobs).await {
                    warn!(source = source.name(), error = %e, "periodic enqueue failed");
                }
            }
            Err(e) => warn!(source = source.name(), error = %e, "periodic source failed"),
        }
    }

    /// The worker loop; returns after `shutdown` is cancelled and running jobs finished or the
    /// grace period passed.
    pub async fn run(self: Arc<Self>, shutdown: CancellationToken) {
        let slots = usize::from(self.config.slots.max(1));
        let mut running: JoinSet<JobOutcome> = JoinSet::new();
        let mut backoff = self.timings.backoff_min;
        let mut reap = tokio::time::interval(self.timings.reap_every);
        reap.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut periodic: Vec<(Arc<dyn Periodic>, tokio::time::Interval)> = self
            .periodic
            .iter()
            .map(|source| {
                let mut every = tokio::time::interval(source.period().max(Duration::from_secs(1)));
                every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                (source.clone(), every)
            })
            .collect();
        info!(worker = %self.worker_id, kinds = self.leased_kinds().len(), slots, "job worker started");

        loop {
            if shutdown.is_cancelled() {
                break;
            }
            while let Some(done) = running.try_join_next() {
                log_join(done);
            }
            let free = slots.saturating_sub(running.len());
            let mut idle = true;
            if free > 0 {
                let max = u16::try_from(free).unwrap_or(u16::MAX);
                match self.queue.lease(&self.lease_request(max)).await {
                    Ok(jobs) => {
                        backoff = self.timings.backoff_min;
                        idle = jobs.is_empty();
                        for job in jobs {
                            let this = self.clone();
                            running.spawn(async move { this.execute(job).await });
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, backoff_ms = backoff.as_millis() as u64, "job lease failed");
                        tokio::select! {
                            () = shutdown.cancelled() => break,
                            () = tokio::time::sleep(backoff) => {}
                        }
                        backoff = (backoff * 2).min(self.timings.backoff_max);
                        continue;
                    }
                }
            }
            if !idle && running.len() < slots {
                continue;
            }
            // Idle or full: wait for a wakeup, a finished job, a timer, or shutdown.
            let wake = tokio::select! {
                () = shutdown.cancelled() => Wake::Stop,
                () = self.wake.notified(), if free > 0 => Wake::Lease,
                () = tokio::time::sleep(self.timings.poll), if free > 0 => Wake::Lease,
                Some(done) = running.join_next(), if !running.is_empty() => Wake::Joined(done),
                _ = reap.tick() => Wake::Reap,
                index = next_tick(&mut periodic) => Wake::Periodic(index),
            };
            match wake {
                Wake::Stop => break,
                Wake::Lease => {}
                Wake::Joined(done) => log_join(done),
                Wake::Reap => match self.queue.reap_expired(self.timings.reap_batch).await {
                    Ok(0) => {}
                    Ok(n) => info!(reaped = n, "requeued or killed expired job leases"),
                    Err(e) => warn!(error = %e, "reap_expired failed"),
                },
                Wake::Periodic(index) => {
                    let source = periodic[index].0.clone();
                    self.tick(source.as_ref()).await;
                }
            }
        }

        info!(running = running.len(), "job worker stopping");
        let grace = tokio::time::sleep(self.timings.shutdown_grace);
        tokio::pin!(grace);
        loop {
            tokio::select! {
                () = &mut grace => {
                    if !running.is_empty() {
                        warn!(abandoned = running.len(), "shutdown grace over: abandoning running jobs (leases expire and are reaped)");
                    }
                    running.abort_all();
                    break;
                }
                done = running.join_next() => match done {
                    Some(done) => log_join(done),
                    None => break,
                },
            }
        }
    }

    async fn execute(self: Arc<Self>, job: LeasedJob) -> JobOutcome {
        let span = tracing::info_span!(
            "job",
            kind = job.kind.as_str(),
            job_id = %job.lease.job_id,
            attempt = job.attempt,
            org_id = job.org_id.map(|o| o.to_string()).unwrap_or_default(),
        );
        self.execute_inner(job).instrument(span).await
    }

    async fn execute_inner(&self, job: LeasedJob) -> JobOutcome {
        let lease = job.lease;
        let Some(handler) = self.handlers.get(&job.kind).cloned() else {
            // Leased only for registered kinds; reaching this means the registry changed under us.
            let failure = JobFailure::Retryable { error: format!("no handler for {}", job.kind), retry_at: None };
            return self.report(self.queue.fail(&lease, failure.clone()).await.map(|_| ()), JobOutcome::Failed(failure));
        };
        let heartbeat_every = self.timings.heartbeat.unwrap_or_else(|| job.kind.retry_policy().lease / 3).max(Duration::from_millis(1));

        enum Stop {
            Lost,
            Cancel,
        }
        let heartbeat = async {
            let mut every = tokio::time::interval_at(tokio::time::Instant::now() + heartbeat_every, heartbeat_every);
            loop {
                every.tick().await;
                match self.queue.heartbeat(&lease).await {
                    Ok(Heartbeat::Extended { .. }) => {}
                    Ok(Heartbeat::CancelRequested { .. }) => return Stop::Cancel,
                    Err(JobError::LeaseLost) => return Stop::Lost,
                    Err(e) => warn!(error = %e, "job heartbeat failed; retrying"),
                }
            }
        };

        let result = tokio::select! {
            result = handler.handle(&job) => result,
            stop = heartbeat => {
                return match stop {
                    Stop::Lost => {
                        warn!("job lease lost while running; result dropped");
                        JobOutcome::LeaseLost
                    }
                    Stop::Cancel => {
                        info!("job cancel requested; handler stopped");
                        self.report(self.queue.fail(&lease, JobFailure::Cancelled).await.map(|_| ()), JobOutcome::Cancelled)
                    }
                };
            }
        };

        match result {
            Ok(JobDone::Complete { follow_ups }) => {
                debug!(follow_ups = follow_ups.len(), "job completed");
                self.report(self.queue.complete(&lease, &follow_ups).await, JobOutcome::Completed)
            }
            Ok(JobDone::Defer { until }) => {
                debug!(%until, "job deferred");
                self.report(self.queue.defer(&lease, until).await, JobOutcome::Deferred)
            }
            Ok(JobDone::Retry { at, error }) => {
                let failure = JobFailure::Retryable { error, retry_at: Some(at) };
                self.report(self.queue.fail(&lease, failure.clone()).await.map(|_| ()), JobOutcome::Failed(failure))
            }
            Err(e) => {
                let failure = failure_for(&e, self.clock.now());
                match &failure {
                    JobFailure::Permanent { .. } => error!(error = %e, "job failed permanently"),
                    _ => warn!(error = %e, "job failed; will retry"),
                }
                self.report(self.queue.fail(&lease, failure.clone()).await.map(|_| ()), JobOutcome::Failed(failure))
            }
        }
    }

    fn report(&self, result: Result<(), JobError>, outcome: JobOutcome) -> JobOutcome {
        match result {
            Ok(()) => outcome,
            Err(JobError::LeaseLost) => {
                warn!("job lease lost before the outcome was recorded; result dropped");
                JobOutcome::LeaseLost
            }
            Err(e) => {
                warn!(error = %e, "recording the job outcome failed; the lease will expire");
                JobOutcome::QueueError(e.to_string())
            }
        }
    }
}

enum Wake {
    Stop,
    Lease,
    Joined(Result<JobOutcome, tokio::task::JoinError>),
    Reap,
    Periodic(usize),
}

/// Index of the next periodic source due; pending forever without sources.
async fn next_tick(periodic: &mut [(Arc<dyn Periodic>, tokio::time::Interval)]) -> usize {
    if periodic.is_empty() {
        return std::future::pending().await;
    }
    let (_, index, _) = futures::future::select_all(periodic.iter_mut().map(|(_, every)| Box::pin(every.tick()))).await;
    index
}

fn log_join(done: Result<JobOutcome, tokio::task::JoinError>) {
    if let Err(e) = done {
        if e.is_panic() {
            error!(error = %e, "job handler panicked; its lease will expire and be reaped");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use mage_core::ports::jobs::JobState;
    use mage_core::testing::{FakeClock, MemJobQueue, SeqIdGen};
    use parking_lot::Mutex;

    use super::*;

    /// Scripted handler for `auth.sweep_sessions`: pops one result per call.
    struct Scripted {
        script: Mutex<Vec<Result<JobDone, AppError>>>,
        calls: AtomicU32,
        block: bool,
    }

    impl Scripted {
        fn new(script: Vec<Result<JobDone, AppError>>) -> Arc<Scripted> {
            let mut script = script;
            script.reverse();
            Arc::new(Scripted { script: Mutex::new(script), calls: AtomicU32::new(0), block: false })
        }

        fn blocking() -> Arc<Scripted> {
            Arc::new(Scripted { script: Mutex::new(Vec::new()), calls: AtomicU32::new(0), block: true })
        }
    }

    #[async_trait]
    impl JobHandler for Scripted {
        fn kinds(&self) -> &'static [&'static str] {
            &["auth.sweep_sessions"]
        }

        async fn handle(&self, _job: &LeasedJob) -> Result<JobDone, AppError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.block {
                std::future::pending::<()>().await;
            }
            self.script.lock().pop().unwrap_or_else(|| Ok(JobDone::complete()))
        }
    }

    struct Fixture {
        clock: Arc<FakeClock>,
        queue: Arc<MemJobQueue>,
    }

    fn fixture() -> Fixture {
        let clock = Arc::new(FakeClock::new());
        let queue = Arc::new(MemJobQueue::new(clock.clone(), Arc::new(SeqIdGen::new())));
        Fixture { clock, queue }
    }

    fn worker(f: &Fixture, handler: Arc<dyn JobHandler>) -> Arc<Worker> {
        let mut w = Worker::new(f.queue.clone(), f.clock.clone(), JobsConfig::default(), "test-worker")
            .with_timings(WorkerTimings { heartbeat: Some(Duration::from_millis(5)), ..WorkerTimings::default() });
        w.register(handler).expect("register");
        Arc::new(w)
    }

    async fn enqueue_sweep(f: &Fixture) {
        f.queue.enqueue(NewJob::new(JobPayload::AuthSweepSessions)).await.expect("enqueue");
    }

    fn only_job(f: &Fixture) -> mage_core::ports::jobs::JobRecord {
        let jobs = f.queue.jobs();
        assert_eq!(jobs.len(), 1, "{jobs:?}");
        jobs.into_iter().next().expect("one job")
    }

    #[tokio::test]
    async fn transient_errors_retry_with_backoff_then_succeed() {
        let f = fixture();
        let handler = Scripted::new(vec![Err(AppError::Unavailable("db".into())), Ok(JobDone::complete())]);
        let w = worker(&f, handler.clone());
        enqueue_sweep(&f).await;

        let outcomes = w.run_once().await.expect("lease");
        assert!(matches!(outcomes.as_slice(), [JobOutcome::Failed(JobFailure::Retryable { retry_at: None, .. })]), "{outcomes:?}");
        let job = only_job(&f);
        assert_eq!((job.state, job.attempts), (JobState::Queued, 1));
        assert!(job.run_after > f.clock.now(), "backoff puts the retry in the future");

        // Not ready before the backoff elapses.
        assert!(w.run_once().await.expect("lease").is_empty());
        f.clock.set(job.run_after);
        let outcomes = w.run_once().await.expect("lease");
        assert_eq!(outcomes, vec![JobOutcome::Completed]);
        assert_eq!(only_job(&f).state, JobState::Succeeded);
        assert_eq!(handler.calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn rate_limit_retries_at_the_advertised_time() {
        let f = fixture();
        let w = worker(&f, Scripted::new(vec![Err(AppError::RateLimited { retry_after_secs: Some(90) })]));
        enqueue_sweep(&f).await;
        w.run_once().await.expect("lease");
        let job = only_job(&f);
        assert_eq!(job.state, JobState::Queued);
        assert_eq!(job.run_after, f.clock.now() + time::Duration::seconds(90));
    }

    #[tokio::test]
    async fn explicit_retry_uses_the_given_time() {
        let f = fixture();
        let at = f.clock.now() + time::Duration::minutes(7);
        let w = worker(&f, Scripted::new(vec![Ok(JobDone::Retry { at, error: "provider reset".into() })]));
        enqueue_sweep(&f).await;
        w.run_once().await.expect("lease");
        let job = only_job(&f);
        assert_eq!((job.state, job.run_after, job.last_error.as_deref()), (JobState::Queued, at, Some("provider reset")));
    }

    #[tokio::test]
    async fn defer_requeues_without_consuming_the_attempt() {
        let f = fixture();
        let until = f.clock.now() + time::Duration::minutes(3);
        let w = worker(&f, Scripted::new(vec![Ok(JobDone::Defer { until }), Ok(JobDone::complete())]));
        enqueue_sweep(&f).await;
        assert_eq!(w.run_once().await.expect("lease"), vec![JobOutcome::Deferred]);
        let job = only_job(&f);
        assert_eq!((job.state, job.attempts, job.defers, job.run_after), (JobState::Queued, 0, 1, until));
        f.clock.set(until);
        assert_eq!(w.run_once().await.expect("lease"), vec![JobOutcome::Completed]);
        assert_eq!(only_job(&f).attempts, 1);
    }

    #[tokio::test]
    async fn permanent_errors_kill_the_job() {
        let f = fixture();
        let w = worker(&f, Scripted::new(vec![Err(AppError::Invalid("bad payload".into()))]));
        enqueue_sweep(&f).await;
        w.run_once().await.expect("lease");
        let job = only_job(&f);
        assert_eq!(job.state, JobState::Dead);
        assert!(job.last_error.as_deref().is_some_and(|e| e.contains("bad payload")));
    }

    #[tokio::test]
    async fn completion_enqueues_follow_ups() {
        let f = fixture();
        let next = f.clock.now() + time::Duration::hours(1);
        let follow = NewJob::new(JobPayload::AuthSweepSessions).dedupe("auth.sweep_sessions:next").after(next);
        let w = worker(&f, Scripted::new(vec![Ok(JobDone::follow_up(vec![follow]))]));
        enqueue_sweep(&f).await;
        w.run_once().await.expect("lease");
        let jobs = f.queue.jobs();
        assert_eq!(jobs.len(), 2);
        assert!(jobs.iter().any(|j| j.state == JobState::Queued && j.run_after == next));
    }

    #[tokio::test]
    async fn cancel_request_stops_the_handler_and_marks_the_job_cancelled() {
        let f = fixture();
        let handler = Scripted::blocking();
        let w = worker(&f, handler.clone());
        enqueue_sweep(&f).await;
        let job_id = only_job(&f).id;
        let run = tokio::spawn({
            let w = w.clone();
            async move { w.run_once().await }
        });
        while handler.calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        f.queue.cancel(job_id).await.expect("cancel");
        let outcomes = run.await.expect("join").expect("lease");
        assert_eq!(outcomes, vec![JobOutcome::Cancelled]);
        assert_eq!(only_job(&f).state, JobState::Cancelled);
    }

    #[tokio::test]
    async fn lost_lease_drops_the_result() {
        let f = fixture();
        let handler = Scripted::blocking();
        let w = worker(&f, handler.clone());
        enqueue_sweep(&f).await;
        let run = tokio::spawn({
            let w = w.clone();
            async move { w.run_once().await }
        });
        while handler.calls.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        // The lease expires and is reaped: the next heartbeat finds the lease gone.
        f.clock.advance(Duration::from_secs(3600));
        f.queue.reap_expired(10).await.expect("reap");
        let outcomes = run.await.expect("join").expect("lease");
        assert_eq!(outcomes, vec![JobOutcome::LeaseLost]);
        assert_eq!(only_job(&f).state, JobState::Queued);
    }

    #[tokio::test]
    async fn registration_rejects_unknown_wrong_lane_and_duplicate_kinds() {
        struct Kinds(&'static [&'static str]);
        #[async_trait]
        impl JobHandler for Kinds {
            fn kinds(&self) -> &'static [&'static str] {
                self.0
            }
            async fn handle(&self, _job: &LeasedJob) -> Result<JobDone, AppError> {
                Ok(JobDone::complete())
            }
        }
        let f = fixture();
        let mut w = Worker::new(f.queue.clone(), f.clock.clone(), JobsConfig::default(), "w");
        assert_eq!(w.register(Arc::new(Kinds(&["nope.kind"]))), Err(WorkerError::UnknownKind("nope.kind".into())));
        assert_eq!(w.register(Arc::new(Kinds(&["index.version"]))), Err(WorkerError::WrongLane("index.version".into())));
        w.register(Arc::new(Kinds(&["vault.rewrap"]))).expect("first");
        assert_eq!(w.register(Arc::new(Kinds(&["vault.rewrap"]))), Err(WorkerError::DuplicateKind("vault.rewrap".into())));
    }

    #[tokio::test]
    async fn configured_prefixes_limit_the_leased_kinds() {
        let f = fixture();
        let mut w = Worker::new(f.queue.clone(), f.clock.clone(), JobsConfig { slots: 2, kinds: vec!["vault.".into()] }, "w");
        w.register(Scripted::new(vec![])).expect("register");
        assert!(w.leased_kinds().is_empty());
        enqueue_sweep(&f).await;
        let w = Arc::new(w);
        assert!(w.run_once().await.expect("lease").is_empty(), "auth.* is not leased by a vault.* worker");
    }

    #[tokio::test]
    async fn tickers_enqueue_one_job_per_bucket() {
        let f = fixture();
        let mut w = Worker::new(f.queue.clone(), f.clock.clone(), JobsConfig::default(), "w");
        w.add_periodic(Arc::new(Ticker::new(JobPayload::VaultRewrap, Duration::from_secs(86_400))));
        w.tick_periodic().await;
        w.tick_periodic().await;
        assert_eq!(f.queue.jobs().len(), 1, "same bucket is a duplicate");
        f.clock.advance(Duration::from_secs(86_400));
        w.tick_periodic().await;
        assert_eq!(f.queue.jobs().len(), 2, "next bucket enqueues again");
    }

    #[tokio::test]
    async fn run_loop_processes_jobs_and_stops_on_shutdown() {
        let f = fixture();
        let handler = Scripted::new(vec![Ok(JobDone::complete())]);
        let w = Arc::new(
            {
                let mut w = Worker::new(f.queue.clone(), f.clock.clone(), JobsConfig::default(), "w").with_timings(WorkerTimings {
                    poll: Duration::from_millis(5),
                    shutdown_grace: Duration::from_millis(50),
                    heartbeat: Some(Duration::from_millis(5)),
                    ..WorkerTimings::default()
                });
                w.register(handler.clone()).expect("register");
                w
            },
        );
        let shutdown = CancellationToken::new();
        let task = tokio::spawn(w.clone().run(shutdown.clone()));
        enqueue_sweep(&f).await;
        w.wake_handle().notify_one();
        for _ in 0..1000 {
            if only_job(&f).state == JobState::Succeeded {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        assert_eq!(only_job(&f).state, JobState::Succeeded);
        shutdown.cancel();
        tokio::time::timeout(Duration::from_secs(2), task).await.expect("worker stops").expect("join");
    }

    #[test]
    fn error_mapping_matches_the_contract() {
        let now = OffsetDateTime::UNIX_EPOCH;
        assert!(matches!(failure_for(&AppError::Conflict("x".into()), now), JobFailure::Retryable { retry_at: None, .. }));
        assert!(matches!(failure_for(&AppError::Internal("x".into()), now), JobFailure::Retryable { retry_at: None, .. }));
        assert!(matches!(failure_for(&AppError::RateLimited { retry_after_secs: None }, now), JobFailure::Retryable { retry_at: None, .. }));
        assert!(matches!(failure_for(&AppError::NotFound("x".into()), now), JobFailure::Permanent { .. }));
        assert!(matches!(failure_for(&AppError::Forbidden("x".into()), now), JobFailure::Permanent { .. }));
        assert!(matches!(failure_for(&AppError::Unauthorized, now), JobFailure::Permanent { .. }));
    }
}
