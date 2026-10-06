//! Runner daemon use cases (runs.md "Startup and recovery", "Lease loop"): recovery + register,
//! lease capacity (free slots per class, prepared bases), the run task registry, draining.
//! The loop itself is the driving adapter `adapters::driving::lease_loop`.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use mage_core::domain::{FailStage, RunId};
use mage_core::wire::runner::{
    Capabilities, FinishOutcome, FinishReport, HarnessAvailability, HeldRun, MAX_PREPARED_BASES, Os, PreparedBaseKey, RegistrationAck,
    RunAssignment, SlotClass, SlotCounts,
};
use parking_lot::Mutex;
use serde::Deserialize;
use tokio::sync::Notify;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use super::task::{RunnerDeps, run_assignment};
use crate::error::AppError;
use crate::ports::runner_state::{ActiveRun, RunPhase};

/// Daemon settings (`[runner.daemon]`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonConfig {
    /// Concurrent runs per slot class (`Capabilities.slots`); further capped by the executor's
    /// free env slots.
    #[serde(default = "defaults::slots")]
    pub slots: SlotCounts,
    /// Local guard: Work-class runs per `(org, repo)` at once (the server applies the same rule).
    #[serde(default = "defaults::max_sessions_per_repo")]
    pub max_sessions_per_repo: u16,
    /// On drain, running tasks get this long before they are abandoned.
    #[serde(default = "defaults::drain_grace_secs")]
    pub drain_grace_secs: u64,
    /// Playwright and its browsers are installed (`Capabilities.browser`).
    #[serde(default = "defaults::browser")]
    pub browser: bool,
}

mod defaults {
    use mage_core::wire::runner::SlotCounts;

    pub fn slots() -> SlotCounts {
        SlotCounts { light: 2, work: 1, verify: 1 }
    }
    pub fn max_sessions_per_repo() -> u16 {
        1
    }
    pub fn drain_grace_secs() -> u64 {
        300
    }
    pub fn browser() -> bool {
        true
    }
}

impl Default for DaemonConfig {
    fn default() -> Self {
        DaemonConfig {
            slots: defaults::slots(),
            max_sessions_per_repo: defaults::max_sessions_per_repo(),
            drain_grace_secs: defaults::drain_grace_secs(),
            browser: defaults::browser(),
        }
    }
}

struct Running {
    class: SlotClass,
    repo: Option<(mage_core::domain::OrgId, mage_core::domain::RepoId)>,
    abandon: CancellationToken,
    /// Cancelled when the task has ended (a newer attempt of the same run waits for it).
    done: CancellationToken,
}

#[derive(Default)]
struct Registry {
    in_use: SlotCounts,
    running: HashMap<(RunId, u16), Running>,
}

fn slot_mut(counts: &mut SlotCounts, class: SlotClass) -> &mut u16 {
    match class {
        SlotClass::Light => &mut counts.light,
        SlotClass::Work => &mut counts.work,
        SlotClass::Verify => &mut counts.verify,
    }
}

/// The runner daemon: one per process.
pub struct Daemon {
    deps: Arc<RunnerDeps>,
    config: DaemonConfig,
    registry: Mutex<Registry>,
    slot_freed: Notify,
    tasks: TaskTracker,
}

impl Daemon {
    pub fn new(deps: Arc<RunnerDeps>, config: DaemonConfig) -> Arc<Self> {
        Arc::new(Daemon { deps, config, registry: Mutex::new(Registry::default()), slot_freed: Notify::new(), tasks: TaskTracker::new() })
    }

    pub fn config(&self) -> &DaemonConfig {
        &self.config
    }

    /// What `register` advertises: executor arch/kind, harnesses whose probe succeeds, browser,
    /// configured slots.
    pub async fn capabilities(&self) -> Result<Capabilities, AppError> {
        let executor = self.deps.executor.capabilities().await?;
        let mut harnesses = Vec::with_capacity(self.deps.harnesses.len());
        for harness in &self.deps.harnesses {
            match harness.probe().await {
                Ok(info) => {
                    harnesses.push(HarnessAvailability { kind: info.kind, version: info.version, credential_modes: info.credential_modes })
                }
                Err(e) => tracing::warn!(kind = ?harness.kind(), error = %e, "harness not advertised: probe failed"),
            }
        }
        let os = if cfg!(target_os = "macos") { Os::MacOs } else { Os::Linux };
        Ok(Capabilities {
            arch: executor.arch,
            os,
            executor: executor.kind,
            harnesses,
            browser: self.config.browser,
            slots: self.config.slots,
        })
    }

    /// Startup (runs.md flowchart): `recover` (reap leftovers, collect held runs), `register`
    /// with the held runs, then `end_run` and delete the run dir of every held or released run.
    /// No run survives a restart in v1.
    pub async fn start(&self) -> Result<RegistrationAck, AppError> {
        let held = super::recover::recover(self.deps.executor.as_ref(), self.deps.state.as_ref()).await?;
        let capabilities = self.capabilities().await?;
        let held_wire: Vec<HeldRun> = held.iter().map(|r| HeldRun { run_id: r.run_id, attempt: r.attempt }).collect();
        let ack = self.deps.link.register(&capabilities, &held_wire).await?;
        let mut ended = BTreeSet::new();
        for run in held.iter().map(|r| r.run_id).chain(ack.released.iter().map(|r| r.run_id)) {
            if !ended.insert(run) {
                continue;
            }
            if let Err(e) = self.deps.state.end_run(run).await {
                tracing::debug!(run = %run, error = ?e, "end_run of a held/released run");
            }
            let dir = self.deps.config.data_dir.join("runs").join(run.to_string());
            match tokio::fs::remove_dir_all(&dir).await {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => tracing::warn!(dir = %dir.display(), error = %e, "cannot remove a leftover run dir"),
            }
        }
        tracing::info!(
            runner = %ack.runner_id,
            held = held.len(),
            released = ack.released.len(),
            min_version = %ack.min_version,
            "runner registered"
        );
        Ok(ack)
    }

    /// Free slots per class (config minus running, capped by the executor's free env slots; no
    /// Work/Verify slots under disk pressure) and the warm bases, newest first, at most 64.
    pub async fn lease_capacity(&self) -> (SlotCounts, Vec<PreparedBaseKey>) {
        let executor = self.deps.executor.capabilities().await;
        let mut free = {
            let registry = self.registry.lock();
            SlotCounts {
                light: self.config.slots.light.saturating_sub(registry.in_use.light),
                work: self.config.slots.work.saturating_sub(registry.in_use.work),
                verify: self.config.slots.verify.saturating_sub(registry.in_use.verify),
            }
        };
        match executor {
            Ok(caps) => {
                for class in [SlotClass::Light, SlotClass::Work, SlotClass::Verify] {
                    let slot = slot_mut(&mut free, class);
                    *slot = (*slot).min(caps.free_env_slots);
                }
                if caps.disk_pressure {
                    free.work = 0;
                    free.verify = 0;
                }
                let prepared = caps
                    .bases
                    .iter()
                    .take(MAX_PREPARED_BASES)
                    .map(|k| PreparedBaseKey { repo_id: k.repo_id, sha: k.sha, recipe_hash: k.recipe_hash })
                    .collect();
                (free, prepared)
            }
            Err(e) => {
                tracing::warn!(error = %e, "executor capabilities unavailable; not leasing");
                (SlotCounts::default(), Vec::new())
            }
        }
    }

    /// Resolves after a run task released its slot (or immediately if one did since the last
    /// call).
    pub async fn slot_freed(&self) {
        self.slot_freed.notified().await;
    }

    /// Number of run tasks still running.
    pub fn running(&self) -> usize {
        self.registry.lock().running.len()
    }

    /// Takes a leased assignment: `RunnerState::begin_run`, then the run task. `lease_sent_at`
    /// is when the lease request that returned it was sent (local lease deadline).
    pub fn accept(self: &Arc<Self>, a: RunAssignment, lease_sent_at: Instant) {
        let class = SlotClass::of(a.kind);
        let repo = a.repo.as_ref().map(|r| (a.org_id, r.repo_id));
        let abandon = CancellationToken::new();
        let done = CancellationToken::new();
        let (previous, repo_busy) = {
            let mut registry = self.registry.lock();
            let previous: Vec<CancellationToken> =
                registry.running.iter().filter(|((run, _), _)| *run == a.run_id).map(|(_, r)| r.done.clone()).collect();
            let repo_busy = class == SlotClass::Work
                && repo.is_some()
                && registry
                    .running
                    .iter()
                    .filter(|((run, _), r)| *run != a.run_id && r.class == SlotClass::Work && r.repo == repo)
                    .count()
                    >= usize::from(self.config.max_sessions_per_repo);
            *slot_mut(&mut registry.in_use, class) += 1;
            registry.running.insert((a.run_id, a.attempt), Running { class, repo, abandon: abandon.clone(), done: done.clone() });
            (previous, repo_busy)
        };
        let daemon = self.clone();
        self.tasks.spawn(async move {
            let (run, attempt) = (a.run_id, a.attempt);
            for earlier in previous {
                earlier.cancelled().await;
            }
            if repo_busy {
                daemon.hand_back(&a, FailStage::Fork, "runner.max_sessions_per_repo reached on this runner").await;
            } else {
                let active = ActiveRun {
                    run_id: a.run_id,
                    org_id: a.org_id,
                    kind: a.kind,
                    attempt: a.attempt,
                    phase: RunPhase::Leased,
                    env: None,
                    started_at: daemon.deps.clock.now(),
                };
                match daemon.deps.state.begin_run(active).await {
                    Ok(()) => run_assignment(daemon.deps.clone(), a, lease_sent_at, abandon).await,
                    Err(e) => daemon.hand_back(&a, FailStage::Internal, &format!("RunnerState.begin_run failed: {e:?}")).await,
                }
            }
            daemon.release(run, attempt);
        });
    }

    /// Returns a run this runner cannot track: `finish(Failed { retryable: true })` requeues it.
    async fn hand_back(&self, a: &RunAssignment, stage: FailStage, reason: &str) {
        tracing::error!(run = %a.run_id, attempt = a.attempt, reason, "handing the run back");
        let report = FinishReport { outcome: FinishOutcome::Failed { stage, reason: reason.to_owned(), retryable: true }, message: None };
        if let Err(e) = self.deps.link.finish(a.run_id, a.attempt, &report).await {
            tracing::warn!(run = %a.run_id, error = %e, "finish after hand-back failed; the reaper clears the lease");
        }
    }

    fn release(&self, run: RunId, attempt: u16) {
        {
            let mut registry = self.registry.lock();
            if let Some(running) = registry.running.remove(&(run, attempt)) {
                let slot = slot_mut(&mut registry.in_use, running.class);
                *slot = slot.saturating_sub(1);
                running.done.cancel();
            }
        }
        self.slot_freed.notify_one();
    }

    /// Draining (runs.md): no new tasks; running ones get `grace`, then the rest are abandoned
    /// (harness aborted, env destroyed, `finish(Abandoned)` → requeued through `LeaseLost`).
    pub async fn drain(&self, grace: Duration) {
        self.tasks.close();
        if tokio::time::timeout(grace, self.tasks.wait()).await.is_err() {
            let abandon: Vec<CancellationToken> = self.registry.lock().running.values().map(|r| r.abandon.clone()).collect();
            tracing::warn!(tasks = abandon.len(), "drain grace over; abandoning the remaining runs");
            for token in abandon {
                token.cancel();
            }
            self.tasks.wait().await;
        }
    }
}
