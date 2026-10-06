//! ReapLeases (job `runs.reap_leases`, runs.md): one commit per due run; conflicts are skipped
//! and picked up on the next tick. The decision is re-derived from the current record, so the
//! store's `DueReason` is only a hint.

use std::time::Duration;

use mage_core::domain::{BudgetLimit, GateEvent, GateStatus, JobPayload, RunError, RunErrorCode, RunStatus, RunStatusEvent};
use mage_core::ports::jobs::NewJob;

use crate::app::runs::lease::{notice, release};
use crate::app::runs::machine::{Initiator, step};
use crate::app::runs::{RunManager, is_stale, job_err, to_time};
use crate::error::AppError;
use crate::ports::runs::{DueItem, DueReason, GateWrite, RunRecord, RunUpdate, RunWrite};

/// Items handled per `runs.reap_leases` job.
pub const REAP_BATCH: u32 = 200;

/// What one reaper pass did (for logs and tests).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReapReport {
    pub handled: u32,
    pub skipped: u32,
}

impl RunManager {
    /// One reaper pass over at most `limit` due runs.
    pub async fn reap_leases(&self, limit: u32) -> Result<ReapReport, AppError> {
        let now = self.now();
        let mut report = ReapReport::default();
        for item in self.d.runs.due(now, limit).await? {
            match self.reap_one(item).await {
                Ok(true) => report.handled += 1,
                Ok(false) => report.skipped += 1,
                Err(AppError::Unavailable(m)) => return Err(AppError::Unavailable(m)),
                Err(err) => {
                    tracing::warn!(error = %err, run = %item.run_id, "reaper skipped a run");
                    report.skipped += 1;
                }
            }
        }
        Ok(report)
    }

    /// The `runs.reap_leases` job that follows the current one (every `reap_every`).
    pub fn next_reaper_job(&self) -> NewJob {
        NewJob::new(JobPayload::RunsReapLeases).after(self.now() + to_time(Duration::from_secs(self.d.policy.reap_every_secs)))
    }

    /// Startup: make sure the reaper job exists (a duplicate is silent).
    pub async fn ensure_reaper(&self) -> Result<(), AppError> {
        self.d.jobs.enqueue(NewJob::new(JobPayload::RunsReapLeases)).await.map_err(job_err)?;
        Ok(())
    }

    async fn reap_one(&self, item: DueItem) -> Result<bool, AppError> {
        let rec = self.d.runs.get(item.org_id, item.run_id).await?;
        let now = self.now();
        let mut w = RunWrite::default();
        if !self.reap_decide(rec, item.reason, now, &mut w).await? {
            return Ok(false);
        }
        match self.d.runs.commit(item.org_id, w).await {
            Ok(_) => Ok(true),
            Err(e) if is_stale(&e) => Ok(false),
            Err(e) => Err(e.into()),
        }
    }

    /// Queues the reaper's action for `rec`; `false` if nothing is due any more.
    async fn reap_decide(&self, mut rec: RunRecord, hint: DueReason, now: time::OffsetDateTime, w: &mut RunWrite) -> Result<bool, AppError> {
        let status = rec.run.status;
        let max = self.d.policy.max_lease_attempts;
        if matches!(status, RunStatus::Preparing | RunStatus::Running) && rec.wall_at(now) > rec.run.budget.max_wall {
            if !self.try_salvage(&mut rec, now, w).await? {
                let stepped = step(&mut rec, RunStatusEvent::BudgetExceeded { limit: BudgetLimit::Wall }, Initiator::Server, now, w)?;
                if stepped.cascade {
                    self.cascade(&rec, now, w).await?;
                }
            }
            w.updates.push(RunUpdate::of(rec));
            return Ok(true);
        }
        if rec.lease.is_some_and(|l| l.until < now) {
            match status {
                RunStatus::Leased => {
                    let event = if rec.run.attempt < max { RunStatusEvent::LeaseExpiredBeforeStart } else { RunStatusEvent::LeaseLost { max_attempts: max } };
                    self.step_and_queue(rec, event, Initiator::Server, now, w).await?;
                }
                RunStatus::Preparing | RunStatus::Running => {
                    self.step_and_queue(rec, RunStatusEvent::LeaseLost { max_attempts: max }, Initiator::Server, now, w).await?;
                }
                RunStatus::AwaitingGate => {
                    release(&mut rec);
                    rec.session_lost = true;
                    w.events.push(notice(&rec, now, "runner lost; gate continues"));
                    w.updates.push(RunUpdate::of(rec));
                }
                _ => {
                    release(&mut rec);
                    w.updates.push(RunUpdate::of(rec));
                }
            }
            return Ok(true);
        }
        let threshold = self.d.policy.due_thresholds();
        match (status, hint) {
            (RunStatus::Queued, DueReason::QueueWait) => {
                rec.run.set_error(RunError { code: RunErrorCode::NoRunner, message: format!("no capable runner within {}s", threshold.queue_max_wait.as_secs()) });
                self.step_and_queue(rec, RunStatusEvent::InfraError, Initiator::Server, now, w).await?;
                Ok(true)
            }
            (RunStatus::AwaitingGate, DueReason::GateWait) => {
                let gates = self.d.runs.gates(rec.org(), rec.id()).await?;
                let Some(mut gate) = gates.into_iter().find(|g| g.gate.status == GateStatus::Pending && g.created_at + to_time(threshold.gate_wait_max) < now)
                else {
                    return Ok(false);
                };
                gate.gate.status = GateStatus::Pending.apply(GateEvent::Fail { reason: "gate timed out".into(), at: now })?;
                w.gates.push(GateWrite::Update(gate));
                rec.run.set_error(RunError { code: RunErrorCode::VerifyInfra, message: "gate timed out".into() });
                // The cascade cancels the Verify child; the gate is already failed.
                self.step_and_queue(rec, RunStatusEvent::InfraError, Initiator::Server, now, w).await?;
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}
