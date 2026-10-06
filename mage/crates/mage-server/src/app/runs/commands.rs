//! PollCommands (`GET /runs/{id}/commands`), SteerRun and CancelRun (runs.md).

use mage_core::domain::{OrgId, RunCommand, RunEventKind, RunId, RunKind, RunStatus, RunStatusEvent, UserId};
use mage_core::wire::api::SteerRequest;
use mage_core::wire::runner::{CommandBatch, CommandEnvelope, CommandsQuery};

use crate::app::runs::lease::RECHECK;
use crate::app::runs::machine::Initiator;
use crate::app::runs::{MAX_COMMIT_RETRIES, ProtocolError, RunManager, RunnerCaller, contention, is_stale};
use crate::error::AppError;
use crate::ports::audit::{Actor, AuditAction, AuditEvent, AuditOutcome, AuditTarget};
use crate::ports::runs::{CommandDraft, CommandRecord, RunEventDraft, RunWrite, SignalTopic};

/// Commands returned per poll.
const POLL_BATCH: u32 = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SteerRun {
    pub org: OrgId,
    pub run: RunId,
    pub text: String,
    pub by: UserId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelRun {
    pub org: OrgId,
    pub run: RunId,
    /// `Some` for a user cancel (Verify runs refused); `None` for system callers.
    pub by: Option<UserId>,
    /// Written as a `Notice` event with the cancel.
    pub notice: Option<String>,
}

fn envelope(c: CommandRecord) -> CommandEnvelope {
    CommandEnvelope { seq: c.seq, attempt: c.attempt, issued_at: c.issued_at, command: c.command }
}

impl RunManager {
    /// Long-poll for commands of `attempt` after `after` (wait capped at `long_poll_max`). The
    /// runner holding a terminal run's lease may still poll.
    pub async fn poll_commands(&self, caller: &RunnerCaller, run: RunId, q: CommandsQuery) -> Result<CommandBatch, ProtocolError> {
        let rec = self.fenced(caller, run, q.attempt).await?;
        let org = rec.org();
        let mut sub = self.d.signals.subscribe(SignalTopic::RunCommands(run)).await.map_err(|e| AppError::Unavailable(e.to_string()))?;
        let deadline = tokio::time::Instant::now() + q.wait.min(self.d.policy.long_poll_max());
        let mut status = rec.run.status;
        loop {
            let commands = self.d.runs.commands_after(org, run, q.attempt, q.after, POLL_BATCH).await?;
            if !commands.is_empty() {
                return Ok(CommandBatch { commands: commands.into_iter().map(envelope).collect(), status });
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() || self.d.shutdown.is_cancelled() {
                return Ok(CommandBatch { commands: Vec::new(), status });
            }
            tokio::select! {
                () = self.d.shutdown.cancelled() => return Ok(CommandBatch { commands: Vec::new(), status }),
                _ = sub.wait(remaining.min(RECHECK)) => {}
            }
            let fresh = self.d.runs.get(org, run).await?;
            if !fresh.holds(&caller.fence(q.attempt)) {
                return Err(ProtocolError::LeaseLost);
            }
            status = fresh.run.status;
        }
    }

    /// SteerRun: `Running` only (409 "gate running" in `AwaitingGate`); text ≤ 8 KiB; also logged
    /// as a `Notice` event. Returns the command's `seq`.
    pub async fn steer_run(&self, req: SteerRun) -> Result<u64, AppError> {
        if req.text.trim().is_empty() {
            return Err(AppError::Invalid("steer text is empty".into()));
        }
        if req.text.len() > SteerRequest::MAX_TEXT_BYTES {
            return Err(AppError::Invalid(format!("steer text is longer than {} bytes", SteerRequest::MAX_TEXT_BYTES)));
        }
        let rec = self.d.runs.get(req.org, req.run).await?;
        match rec.run.status {
            RunStatus::Running => {}
            RunStatus::AwaitingGate => return Err(AppError::Conflict("gate running".into())),
            s => return Err(AppError::Conflict(format!("run is {}", s.name()))),
        }
        let now = self.now();
        let w = RunWrite {
            commands: vec![CommandDraft { run_id: req.run, attempt: rec.run.attempt, command: RunCommand::Steer { text: req.text.clone(), by: req.by } }],
            events: vec![RunEventDraft { run_id: req.run, attempt: rec.run.attempt, at: now, kind: RunEventKind::Notice(format!("steer by {}: {}", req.by, req.text)) }],
            ..RunWrite::default()
        };
        let receipt = self.d.runs.commit(req.org, w).await?;
        let seq = receipt.command_seqs.first().map_or(0, |(_, s)| *s);
        self.audit_user(req.org, req.by, AuditAction::RunSteered, req.run).await;
        Ok(seq)
    }

    /// CancelRun: terminal → no-op; otherwise `Cancel` through `machine::step` (Abort if leased,
    /// cascade to Verify children, `apply_outcome`). Users cancel the parent, never a Verify run.
    pub async fn cancel_run(&self, req: CancelRun) -> Result<RunStatus, AppError> {
        for _ in 0..MAX_COMMIT_RETRIES {
            let rec = self.d.runs.get(req.org, req.run).await?;
            if rec.run.status.is_terminal() {
                return Ok(rec.run.status);
            }
            if req.by.is_some() && rec.run.kind == RunKind::Verify {
                return Err(AppError::Invalid("verify runs are cancelled through their parent".into()));
            }
            let now = self.now();
            let mut w = RunWrite::default();
            if let Some(text) = &req.notice {
                w.events.push(RunEventDraft { run_id: req.run, attempt: rec.run.attempt, at: now, kind: RunEventKind::Notice(text.clone()) });
            }
            let by = req.by.map_or(Initiator::Server, Initiator::User);
            let rec = self.step_and_queue(rec, RunStatusEvent::Cancel, by, now, &mut w).await?;
            match self.d.runs.commit(req.org, w).await {
                Ok(_) => {
                    if let Some(user) = req.by {
                        self.audit_user(req.org, user, AuditAction::RunCancelled, req.run).await;
                    }
                    return Ok(rec.run.status);
                }
                Err(e) if is_stale(&e) => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Err(contention())
    }

    pub(crate) async fn audit_user(&self, org: OrgId, user: UserId, action: AuditAction, run: RunId) {
        let event = AuditEvent {
            org: Some(org),
            actor: Actor::User(user),
            action,
            target: AuditTarget::new("run", run.to_string()),
            outcome: AuditOutcome::Success,
            ip: None,
            request_id: None,
            details: Vec::new(),
        };
        if let Err(err) = self.d.audit.record(event).await {
            tracing::warn!(error = %err, %run, "audit failed");
        }
    }
}
