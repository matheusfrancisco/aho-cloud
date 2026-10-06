//! IngestEvents (`POST /runs/{id}/events`) and RecordUsage (model gateway), runs.md.

use time::OffsetDateTime;

use mage_core::domain::{BudgetLimit, OrgId, RunEventKind, RunId, RunStatus, RunStatusEvent, Usage};
use mage_core::wire::runner::{EventsAck, EventsRequest, MAX_EVENTS_PER_BATCH, OutboundEventKind};

use crate::app::runs::machine::Initiator;
use crate::app::runs::{MAX_COMMIT_RETRIES, ProtocolError, RunManager, RunnerCaller, contention, is_stale};
use crate::error::AppError;
use crate::ports::runs::{Ingest, RunEventDraft, RunRecord, RunUpdate, RunWrite};

/// Gateway metering of one model call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordUsage {
    pub org: OrgId,
    pub run: RunId,
    pub delta: Usage,
}

/// Whether the gateway may keep serving the run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsageVerdict {
    Allow,
    Deny(String),
}

/// Per-field max of two cumulative reports (wall is server-side, never taken from a runner).
fn max_usage(a: Usage, b: Usage) -> Usage {
    Usage {
        input_tokens: a.input_tokens.max(b.input_tokens),
        output_tokens: a.output_tokens.max(b.output_tokens),
        cost_micros: a.cost_micros.max(b.cost_micros),
        turns: a.turns.max(b.turns),
        wall: std::time::Duration::ZERO,
    }
}

fn sub_usage(a: Usage, b: Usage) -> Usage {
    Usage {
        input_tokens: a.input_tokens.saturating_sub(b.input_tokens),
        output_tokens: a.output_tokens.saturating_sub(b.output_tokens),
        cost_micros: a.cost_micros.saturating_sub(b.cost_micros),
        turns: a.turns.saturating_sub(b.turns),
        wall: std::time::Duration::ZERO,
    }
}

impl RunManager {
    /// `BudgetExceeded` (Triage: salvage first) when the effective usage is over budget while
    /// `Preparing | Running`. Returns `true` if `rec` changed.
    pub(crate) async fn enforce_budget(&self, rec: &mut RunRecord, now: OffsetDateTime, w: &mut RunWrite) -> Result<bool, AppError> {
        if !matches!(rec.run.status, RunStatus::Preparing | RunStatus::Running) {
            return Ok(false);
        }
        let Some(limit) = rec.effective_usage(now).exceeds(&rec.run.budget) else { return Ok(false) };
        if self.try_salvage(rec, now, w).await? {
            return Ok(true);
        }
        let stepped = crate::app::runs::machine::step(rec, RunStatusEvent::BudgetExceeded { limit }, Initiator::Server, now, w)?;
        if stepped.cascade {
            self.cascade(rec, now, w).await?;
        }
        Ok(true)
    }

    /// `POST /runs/{id}/events`.
    pub async fn ingest_events(&self, caller: &RunnerCaller, run: RunId, req: EventsRequest) -> Result<EventsAck, ProtocolError> {
        if req.events.len() > MAX_EVENTS_PER_BATCH {
            return Err(ProtocolError::TooLarge(format!("at most {MAX_EVENTS_PER_BATCH} events per batch")));
        }
        let last_usage = req.events.iter().rposition(|e| matches!(e.kind, OutboundEventKind::Usage(_)));
        let reported = last_usage.and_then(|i| match &req.events[i].kind {
            OutboundEventKind::Usage(u) => Some(*u),
            _ => None,
        });
        let drafts: Vec<(u64, RunEventDraft)> = req
            .events
            .into_iter()
            .enumerate()
            .filter(|(i, e)| !matches!(e.kind, OutboundEventKind::Usage(_)) || Some(*i) == last_usage)
            .map(|(_, e)| {
                let mut kind = RunEventKind::from(e.kind);
                kind.truncate_previews();
                (e.local_seq, RunEventDraft { run_id: run, attempt: req.attempt, at: e.at, kind })
            })
            .collect();
        for _ in 0..MAX_COMMIT_RETRIES {
            let mut rec = self.fenced(caller, run, req.attempt).await?;
            let org = rec.org();
            let now = self.now();
            let mut w = RunWrite { ingest: Some(Ingest { run_id: run, attempt: req.attempt, events: drafts.clone() }), ..RunWrite::default() };
            let mut changed = false;
            if let Some(u) = reported {
                let current = sub_usage(rec.usage_reported, rec.reported_frozen);
                let next = rec.reported_frozen.add(&max_usage(current, u));
                if next != rec.usage_reported {
                    rec.usage_reported = next;
                    let effective = rec.effective_usage(now);
                    let _ = rec.run.record_usage(effective);
                    changed = true;
                }
            }
            changed |= self.enforce_budget(&mut rec, now, &mut w).await?;
            if changed {
                w.updates.push(RunUpdate::of(rec.clone()));
            }
            match self.d.runs.commit(org, w).await {
                Ok(receipt) => {
                    let (acked_local_seq, last_seq) = receipt.ingested.unwrap_or((0, rec.last_seq));
                    return Ok(EventsAck { acked_local_seq, last_seq, status: rec.run.status });
                }
                Err(e) if is_stale(&e) => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Err(contention().into())
    }

    /// RecordUsage: adds gateway-metered usage; `Deny` when the run is terminal or over budget.
    pub async fn record_usage(&self, req: RecordUsage) -> Result<UsageVerdict, AppError> {
        for _ in 0..MAX_COMMIT_RETRIES {
            let mut rec = self.d.runs.get(req.org, req.run).await?;
            let now = self.now();
            rec.usage_metered = rec.usage_metered.add(&Usage { wall: std::time::Duration::ZERO, ..req.delta });
            let effective = rec.effective_usage(now);
            let _ = rec.run.record_usage(effective);
            let mut w = RunWrite::default();
            self.enforce_budget(&mut rec, now, &mut w).await?;
            let over: Option<BudgetLimit> = rec.effective_usage(now).exceeds(&rec.run.budget);
            let verdict = match (rec.run.status.is_terminal(), over) {
                (true, _) => UsageVerdict::Deny(format!("run is {}", rec.run.status.name())),
                (false, Some(limit)) => UsageVerdict::Deny(format!("budget exceeded: {limit}")),
                (false, None) => UsageVerdict::Allow,
            };
            w.updates.push(RunUpdate::of(rec));
            match self.d.runs.commit(req.org, w).await {
                Ok(_) => return Ok(verdict),
                Err(e) if is_stale(&e) => continue,
                Err(e) => return Err(e.into()),
            }
        }
        Err(contention())
    }
}
