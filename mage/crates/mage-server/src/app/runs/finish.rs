//! FinishRun (`POST /runs/{id}/finish`, runs.md): replay per `(run, attempt)`, fence (an expired
//! lease is accepted while nobody else holds the run and the attempt is unchanged), the outcome on
//! a still non-terminal run, then the lease release (frees the repo slot, signals `Queue`).

use mage_core::domain::{RunError, RunErrorCode, RunId, RunStatus, RunStatusEvent};
use mage_core::wire::runner::{FinishAck, FinishOutcome, FinishRequest};

use crate::app::runs::lease::{notice, release};
use crate::app::runs::machine::Initiator;
use crate::app::runs::{MAX_COMMIT_RETRIES, ProtocolError, RunManager, RunnerCaller, contention, is_stale, request_hash};
use crate::error::AppError;
use crate::ports::runs::{IdempotentReply, RunConflict, RunStoreError, RunUpdate, RunWrite};

impl RunManager {
    pub async fn finish_run(&self, caller: &RunnerCaller, run: RunId, req: FinishRequest) -> Result<FinishAck, ProtocolError> {
        let scope = format!("runner:{}:finish:{run}", caller.id());
        let key = req.attempt.to_string();
        let replayed = |body: &[u8]| -> Result<FinishAck, ProtocolError> {
            serde_json::from_slice(body).map_err(|e| ProtocolError::App(AppError::Internal(format!("stored reply: {e}"))))
        };
        if let Some(stored) = self.d.runs.reply(&scope, &key).await? {
            return replayed(&stored.body);
        }
        for _ in 0..MAX_COMMIT_RETRIES {
            let mut rec = self.runner_run(caller, run).await?;
            let org = rec.org();
            let fence = caller.fence(req.attempt);
            let orphaned = rec.lease.is_none() && rec.run.attempt == req.attempt;
            if !rec.holds(&fence) && !orphaned {
                return Err(ProtocolError::LeaseLost);
            }
            let now = self.now();
            let mut w = RunWrite::default();
            let status = rec.run.status;
            let max = self.d.policy.max_lease_attempts;
            if status == RunStatus::AwaitingGate {
                if req.report.outcome == FinishOutcome::Done {
                    return Err(ProtocolError::InvalidState(status));
                }
                rec.session_lost = true;
                w.events.push(notice(&rec, now, "runner session ended; gate continues"));
            } else if !status.is_terminal() {
                match &req.report.outcome {
                    FinishOutcome::Done => return Err(ProtocolError::InvalidState(status)),
                    FinishOutcome::NoResult => {
                        if !self.try_salvage(&mut rec, now, &mut w).await? {
                            rec.run.set_error(RunError {
                                code: RunErrorCode::NoResult,
                                message: req.report.message.clone().unwrap_or_else(|| "harness ended without a result".into()),
                            });
                            rec = self.step_terminal(rec, RunStatusEvent::InfraError, now, &mut w).await?;
                        }
                    }
                    FinishOutcome::Failed { stage, reason, retryable: false } => {
                        rec.run.set_error(RunError { code: RunErrorCode::Runner { stage: *stage }, message: reason.clone() });
                        rec = self.step_terminal(rec, RunStatusEvent::InfraError, now, &mut w).await?;
                    }
                    FinishOutcome::Failed { retryable: true, reason, .. } => {
                        w.events.push(notice(&rec, now, &format!("runner handed the lease back: {reason}")));
                        rec = self.step_terminal(rec, RunStatusEvent::LeaseLost { max_attempts: max }, now, &mut w).await?;
                    }
                    FinishOutcome::Abandoned => {
                        rec = self.step_terminal(rec, RunStatusEvent::LeaseLost { max_attempts: max }, now, &mut w).await?;
                    }
                }
            }
            if rec.lease.is_some_and(|l| l.fence() == fence) {
                release(&mut rec);
            }
            let ack = FinishAck { status: rec.run.status };
            w.updates.push(RunUpdate::of(rec));
            let body = serde_json::to_vec(&ack).map_err(|e| AppError::Internal(e.to_string()))?;
            w.reply = Some(IdempotentReply { scope: scope.clone(), key: key.clone(), request_hash: request_hash(&body), body });
            match self.d.runs.commit(org, w).await {
                Ok(_) => return Ok(ack),
                Err(RunStoreError::Conflict(RunConflict::ReplyExists(stored))) => return replayed(&stored.body),
                Err(e) if is_stale(&e) => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Err(contention().into())
    }

    /// One step whose cascade (if any) is queued, without queuing the record itself.
    async fn step_terminal(
        &self,
        mut rec: crate::ports::runs::RunRecord,
        event: RunStatusEvent,
        now: time::OffsetDateTime,
        w: &mut RunWrite,
    ) -> Result<crate::ports::runs::RunRecord, AppError> {
        let stepped = crate::app::runs::machine::step(&mut rec, event, Initiator::Runner, now, w)?;
        if stepped.cascade {
            self.cascade(&rec, now, w).await?;
        }
        Ok(rec)
    }
}
