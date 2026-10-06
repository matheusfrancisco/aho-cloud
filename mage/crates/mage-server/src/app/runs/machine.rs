//! `machine::step` (runs.md "Terminal side effects"): pure `RunRecord x RunStatusEvent` → new
//! record + side effects appended to the commit being built.
//!
//! Every status change appends a `Status(new)` event. The wall clock counts only `Preparing` and
//! `Running`. Requeue rows (`LeaseExpiredBeforeStart`, `LeaseLost` → `Queued`) drop the lease,
//! freeze the harness-reported usage of the lost attempt and reset `session_lost`. Entering a
//! terminal state adds: an `Abort` command when the lease is held and the server caused the
//! transition (`Cancel`, `BudgetExceeded`, `InfraError`), the `runs.apply_outcome` job, and the
//! cascade flag (the caller cancels non-terminal Verify children, which needs the store).

use time::OffsetDateTime;

use mage_core::domain::{AbortReason, DomainError, JobPayload, OrgId, RunCommand, RunEventKind, RunId, RunStatus, RunStatusEvent, UserId};
use mage_core::ports::jobs::NewJob;

use crate::ports::runs::{CommandDraft, RunEventDraft, RunRecord, RunWrite};

/// Who raised an event. Only server-caused terminal transitions send `Abort`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Initiator {
    /// The runner's own report (submit, finish, heartbeat phase).
    Runner,
    /// The server (reaper, budget, gateway, gates, cascades).
    Server,
    /// A user action (cancel).
    User(UserId),
}

/// What a step did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stepped {
    pub from: RunStatus,
    pub to: RunStatus,
    /// Entered a terminal state: cancel the run's non-terminal Verify children.
    pub cascade: bool,
}

/// Applies `event` to `rec` and appends its side effects to `w` (events, commands, jobs). The
/// caller adds `RunUpdate::of(rec)` once all steps on `rec` are done.
pub fn step(rec: &mut RunRecord, event: RunStatusEvent, by: Initiator, now: OffsetDateTime, w: &mut RunWrite) -> Result<Stepped, DomainError> {
    let from = rec.run.status;
    rec.run.apply(event, now)?;
    let to = rec.run.status;

    if matches!(event, RunStatusEvent::LeaseExpiredBeforeStart | RunStatusEvent::LeaseLost { .. }) {
        rec.lease = None;
        if to == RunStatus::Queued {
            rec.reported_frozen = rec.usage_reported;
            rec.session_lost = false;
            rec.affinity = None;
        }
    }

    let counting = |s: RunStatus| matches!(s, RunStatus::Preparing | RunStatus::Running);
    if counting(from) && !counting(to) {
        rec.wall.close(now);
    } else if !counting(from) && counting(to) {
        rec.wall.open(now);
    }
    let effective = rec.effective_usage(now);
    let _ = rec.run.record_usage(effective);

    if from != to {
        w.events.push(RunEventDraft { run_id: rec.id(), attempt: rec.run.attempt, at: now, kind: RunEventKind::Status(to) });
    }
    let cascade = to.is_terminal() && !from.is_terminal();
    if cascade {
        let reason = match (event, by) {
            (_, Initiator::Runner) => None,
            (RunStatusEvent::Cancel, Initiator::User(user)) => Some(AbortReason::Cancelled { by: Some(user) }),
            (RunStatusEvent::Cancel, Initiator::Server) => Some(AbortReason::Cancelled { by: None }),
            (RunStatusEvent::BudgetExceeded { limit }, _) => Some(AbortReason::Budget { limit }),
            (RunStatusEvent::InfraError, _) => Some(AbortReason::Infra),
            _ => None,
        };
        if let Some(reason) = reason
            && rec.lease.is_some()
        {
            w.commands.push(CommandDraft { run_id: rec.id(), attempt: rec.run.attempt, command: RunCommand::Abort { reason } });
        }
        w.jobs.push(apply_outcome_job(rec.org(), rec.id(), to));
    }
    Ok(Stepped { from, to, cascade })
}

/// `runs.apply_outcome {run_id}`, dedupe `runs.apply_outcome:{run_id}:{status}`.
pub fn apply_outcome_job(org: OrgId, run: RunId, status: RunStatus) -> NewJob {
    NewJob::new(JobPayload::RunsApplyOutcome { org_id: org, run_id: run }).dedupe(format!("runs.apply_outcome:{run}:{}", status.name()))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use time::macros::datetime;
    use uuid::Uuid;

    use mage_core::domain::{BudgetLimit, CommitSha, RepoId, RepoVersionKey, RunKind, RunSubject, RunnerId, RunnerLease, Usage};

    use super::*;
    use crate::ports::runs::LeaseMeta;
    use crate::testing::runs::{sample_run, sample_spec};

    const T0: OffsetDateTime = datetime!(2026-03-01 10:00 UTC);
    const MAX: u16 = 3;

    fn budget() -> mage_core::domain::Budget {
        mage_core::domain::Budget { max_wall: Duration::from_secs(600), max_turns: 10, max_cost_micros: 1000, max_gate_attempts: 2 }
    }

    fn runner() -> RunnerId {
        RunnerId::from_uuid(Uuid::from_u128(9))
    }

    /// A record in `status` at `attempt`, with a held lease where the status implies one.
    fn record(status: RunStatus, attempt: u16) -> RunRecord {
        let org = OrgId::from_uuid(Uuid::from_u128(1));
        let key = RepoVersionKey::new(RepoId::from_uuid(Uuid::from_u128(2)), CommitSha([1; 20]));
        let run = sample_run(org, mage_core::domain::RunId::from_uuid(Uuid::from_u128(3)), RunKind::Learn, RunSubject::RepoVersion(key), budget(), T0);
        let mut data = run.into_data();
        data.status = status;
        data.attempt = attempt;
        let leased = !matches!(status, RunStatus::Queued);
        if leased {
            data.runner = Some(RunnerLease { runner_id: runner(), until: T0 + Duration::from_secs(60) });
        }
        let mut rec = RunRecord::new(mage_core::domain::Run::rehydrate(data), sample_spec(RunKind::Learn), true);
        if leased {
            rec.lease = Some(LeaseMeta { runner_id: runner(), instance_id: Uuid::from_u128(5), attempt, until: T0 + Duration::from_secs(60), request_id: Uuid::nil() });
        }
        if matches!(status, RunStatus::Preparing | RunStatus::Running) {
            rec.wall.segment_start = Some(T0);
        }
        rec
    }

    const ALL_STATUSES: [RunStatus; 10] = [
        RunStatus::Queued,
        RunStatus::Leased,
        RunStatus::Preparing,
        RunStatus::Running,
        RunStatus::AwaitingGate,
        RunStatus::Succeeded,
        RunStatus::Failed,
        RunStatus::TimedOut,
        RunStatus::Cancelled,
        RunStatus::Errored,
    ];

    fn events() -> Vec<RunStatusEvent> {
        vec![
            RunStatusEvent::Lease { runner: RunnerLease { runner_id: runner(), until: T0 } },
            RunStatusEvent::LeaseExpiredBeforeStart,
            RunStatusEvent::EnvPreparing,
            RunStatusEvent::Started,
            RunStatusEvent::ResultSubmitted { needs_gate: true },
            RunStatusEvent::ResultSubmitted { needs_gate: false },
            RunStatusEvent::GatePassed,
            RunStatusEvent::GateFailed { attempts_left: 1 },
            RunStatusEvent::GateFailed { attempts_left: 0 },
            RunStatusEvent::BudgetExceeded { limit: BudgetLimit::Cost },
            RunStatusEvent::Cancel,
            RunStatusEvent::InfraError,
            RunStatusEvent::LeaseLost { max_attempts: MAX },
        ]
    }

    /// runs.md "Run lifecycle" table: expected `(status, attempt)` or `None` for illegal pairs.
    fn expected(from: RunStatus, event: &RunStatusEvent, attempt: u16) -> Option<(RunStatus, u16)> {
        use RunStatus as S;
        use RunStatusEvent as E;
        match (from, event) {
            (S::Queued, E::Lease { .. }) => Some((S::Leased, attempt)),
            (S::Leased, E::LeaseExpiredBeforeStart) => Some((S::Queued, attempt + 1)),
            (S::Leased, E::EnvPreparing) => Some((S::Preparing, attempt)),
            (S::Preparing, E::Started) => Some((S::Running, attempt)),
            (S::Running, E::ResultSubmitted { needs_gate: true }) => Some((S::AwaitingGate, attempt)),
            (S::Running, E::ResultSubmitted { needs_gate: false }) => Some((S::Succeeded, attempt)),
            (S::AwaitingGate, E::GatePassed) => Some((S::Succeeded, attempt)),
            (S::AwaitingGate, E::GateFailed { attempts_left }) => Some((if *attempts_left > 0 { S::Running } else { S::Failed }, attempt)),
            (S::Preparing | S::Running, E::BudgetExceeded { .. }) => Some((S::TimedOut, attempt)),
            (S::Leased | S::Preparing | S::Running, E::LeaseLost { max_attempts }) => {
                Some(if attempt < *max_attempts { (S::Queued, attempt + 1) } else { (S::Errored, attempt) })
            }
            (f, E::Cancel) if !f.is_terminal() => Some((S::Cancelled, attempt)),
            (f, E::InfraError) if !f.is_terminal() => Some((S::Errored, attempt)),
            _ => None,
        }
    }

    #[test]
    fn every_state_event_pair_follows_the_table_with_its_side_effects() {
        for from in ALL_STATUSES {
            for attempt in [1, MAX] {
                for event in events() {
                    for by in [Initiator::Server, Initiator::Runner] {
                        let mut rec = record(from, attempt);
                        let before = rec.clone();
                        let mut w = RunWrite::default();
                        let got = step(&mut rec, event, by, T0 + Duration::from_secs(30), &mut w);
                        let Some((to, next_attempt)) = expected(from, &event, attempt) else {
                            assert!(matches!(got, Err(DomainError::InvalidTransition { .. })), "{from:?} x {event:?}: {got:?}");
                            assert_eq!(rec, before, "an illegal event changes nothing");
                            assert!(w.is_empty());
                            continue;
                        };
                        let stepped = got.unwrap_or_else(|e| panic!("{from:?} x {event:?}: {e}"));
                        assert_eq!((rec.run.status, rec.run.attempt), (to, next_attempt), "{from:?} x {event:?}");
                        // Status event on every change.
                        let status_events: Vec<_> = w.events.iter().filter(|e| matches!(e.kind, RunEventKind::Status(_))).collect();
                        assert_eq!(status_events.len(), usize::from(from != to));
                        // Terminal side effects.
                        let terminal = to.is_terminal();
                        assert_eq!(stepped.cascade, terminal);
                        assert_eq!(w.jobs.len(), usize::from(terminal), "{from:?} x {event:?}");
                        if terminal {
                            assert_eq!(w.jobs[0].dedupe_key.as_deref(), Some(format!("runs.apply_outcome:{}:{}", rec.id(), to.name()).as_str()));
                        }
                        let server_caused =
                            matches!(event, RunStatusEvent::Cancel | RunStatusEvent::BudgetExceeded { .. } | RunStatusEvent::InfraError);
                        let aborts = w.commands.iter().filter(|c| matches!(c.command, RunCommand::Abort { .. })).count();
                        let lease_held = rec.lease.is_some();
                        assert_eq!(
                            aborts,
                            usize::from(terminal && server_caused && by == Initiator::Server && lease_held),
                            "{from:?} x {event:?} by {by:?}"
                        );
                        // Requeue drops the lease; terminal keeps it until finish.
                        if to == RunStatus::Queued {
                            assert!(rec.lease.is_none() && rec.run.runner.is_none());
                        }
                        if matches!(event, RunStatusEvent::LeaseLost { .. }) {
                            assert!(rec.lease.is_none());
                        }
                        if terminal && !matches!(event, RunStatusEvent::LeaseLost { .. }) {
                            assert_eq!(rec.lease.is_some(), before.lease.is_some());
                            assert!(rec.run.finished_at.is_some());
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn wall_counts_only_preparing_and_running() {
        let mut rec = record(RunStatus::Leased, 1);
        let mut w = RunWrite::default();
        let t = |s: u64| T0 + Duration::from_secs(s);
        step(&mut rec, RunStatusEvent::EnvPreparing, Initiator::Runner, t(10), &mut w).unwrap();
        step(&mut rec, RunStatusEvent::Started, Initiator::Runner, t(20), &mut w).unwrap();
        step(&mut rec, RunStatusEvent::ResultSubmitted { needs_gate: true }, Initiator::Runner, t(70), &mut w).unwrap();
        assert_eq!(rec.wall_at(t(1000)), Duration::from_secs(60), "AwaitingGate does not count");
        step(&mut rec, RunStatusEvent::GateFailed { attempts_left: 1 }, Initiator::Server, t(1000), &mut w).unwrap();
        assert_eq!(rec.wall_at(t(1005)), Duration::from_secs(65), "the segment reopens on return to Running");
    }

    #[test]
    fn requeue_freezes_reported_usage_and_resets_session_state() {
        let mut rec = record(RunStatus::Running, 1);
        rec.usage_reported = Usage { cost_micros: 40, turns: 2, ..Usage::default() };
        rec.session_lost = true;
        let mut w = RunWrite::default();
        step(&mut rec, RunStatusEvent::LeaseLost { max_attempts: MAX }, Initiator::Server, T0, &mut w).unwrap();
        assert_eq!(rec.run.status, RunStatus::Queued);
        assert_eq!(rec.reported_frozen, rec.usage_reported);
        assert!(!rec.session_lost);
        assert!(w.commands.is_empty(), "a lost lease gets no Abort");
    }

    #[test]
    fn user_cancel_names_the_user_in_abort() {
        let mut rec = record(RunStatus::Running, 1);
        let user = UserId::from_uuid(Uuid::from_u128(77));
        let mut w = RunWrite::default();
        step(&mut rec, RunStatusEvent::Cancel, Initiator::User(user), T0, &mut w).unwrap();
        assert_eq!(w.commands[0].command, RunCommand::Abort { reason: AbortReason::Cancelled { by: Some(user) } });
    }
}
