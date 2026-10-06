//! ApplyTriageOutcome: success, idempotent replay, fencing, failures and the single requeue.

use std::time::Duration;

use mage_core::domain::{
    CauseDraft, CitationDraft, Confidence, JobKind, JobPayload, LineRange, Notice, ReproMethod, RunError, RunErrorCode, RunId, RunStatus,
    RunStatusEvent, TicketStatus, TriageResult, TriageTrigger, UserId,
};

use super::*;
use crate::app::triage::testkit::*;
use crate::ports::ticket::TicketStore;
use crate::ports::triage_eval::{EvalCase, GroundTruth, LabelSource, LabelStrength, NewTriageEval, TriageEvalStore};

const VAT_QUOTE: &str = "const vat = customer.vatId.toUpperCase();";

fn draft(title: &str, p: &str, line: u32, quote: &str, confidence: f64) -> CauseDraft {
    CauseDraft {
        title: title.into(),
        explanation: "e".into(),
        evidence: vec![CitationDraft { path: path(p), lines: LineRange::new(line, line).unwrap(), quote: Some(quote.into()), note: None }],
        repro_steps: vec![],
        confirm_if: "c".into(),
        rule_out_if: "r".into(),
        confidence: Confidence::new(confidence).unwrap(),
        method: ReproMethod::Api,
    }
}

fn result() -> TriageResult {
    TriageResult {
        summary: "Export crashes.".into(),
        causes: vec![
            draft("Null VAT id", "src/billing/export.ts", 30, VAT_QUOTE, 0.8),
            draft("Tax rounding", "src/billing/totals.ts", 10, "const tax = Math.round(net * taxRate * 100) / 100;", 0.3),
        ],
        missing_info: vec![],
    }
}

/// A Triaging ticket fenced on a fresh run for `H1`.
async fn triaging() -> (World, RunId) {
    let w = World::new().await;
    let run = w.seed_run(hash(1), TriageTrigger::Auto).await.run.id;
    w.put_ticket(&ticket(TicketStatus::Triaging, hash(1), Some(hash(1)), Some(run))).await;
    (w, run)
}

async fn succeed(w: &World, run: RunId, r: TriageResult) {
    w.start_running(run).await;
    w.drive(run, &[RunStatusEvent::ResultSubmitted { needs_gate: false }], Some(r), None).await;
}

fn notices(w: &World) -> Vec<JobPayload> {
    w.queued(JobKind::NotifyTicket).into_iter().map(|j| j.payload).collect()
}

#[tokio::test]
async fn non_terminal_runs_are_a_no_op() {
    let (w, run) = triaging().await;
    w.apply().run(org(), run).await.unwrap();
    w.start_running(run).await;
    w.apply().run(org(), run).await.unwrap();
    assert_eq!(w.ticket().await.status, TicketStatus::Triaging);
    assert!(notices(&w).is_empty());
}

#[tokio::test]
async fn success_stores_checked_causes_and_notifies_once_even_when_replayed() {
    let (w, run) = triaging().await;
    succeed(&w, run, result()).await;
    w.apply().run(org(), run).await.unwrap();

    let t = w.ticket().await;
    assert_eq!(t.status, TicketStatus::Triaged);
    let causes = w.h.tickets.causes(org(), ticket_id(), false).await.unwrap();
    assert_eq!(causes.iter().map(|c| (c.rank, c.title.as_str())).collect::<Vec<_>>(), [(1, "Null VAT id"), (2, "Tax rounding")]);
    assert!(causes.iter().all(|c| c.triage_run == run && c.evidence.iter().all(|e| e.snippet_hash.is_some() && e.sha == sha())));
    assert_eq!(notices(&w), [JobPayload::NotifyTicket { org_id: org(), ticket_id: ticket_id(), notice: Notice::TriageReady { run_id: run } }]);

    // Replay of runs.apply_outcome.
    w.apply().run(org(), run).await.unwrap();
    assert_eq!(w.h.tickets.causes(org(), ticket_id(), true).await.unwrap().len(), 2);
    assert_eq!(notices(&w).len(), 1);
}

#[tokio::test]
async fn retriage_supersedes_untested_causes() {
    let (w, first) = triaging().await;
    succeed(&w, first, result()).await;
    w.apply().run(org(), first).await.unwrap();

    let second = w.seed_run(hash(1), TriageTrigger::Manual { by: UserId::from_uuid(uuid(5)) }).await.run.id;
    w.h.tickets.begin_triage(org(), ticket_id(), &[TicketStatus::Triaged], hash(1), second).await.unwrap();
    let mut r = result();
    r.causes.truncate(1);
    succeed(&w, second, r).await;
    w.apply().run(org(), second).await.unwrap();

    let current = w.h.tickets.causes(org(), ticket_id(), false).await.unwrap();
    assert_eq!(current.len(), 1);
    assert_eq!(current[0].triage_run, second);
    let all = w.h.tickets.causes(org(), ticket_id(), true).await.unwrap();
    assert_eq!(all.iter().filter(|c| c.superseded_at.is_some()).count(), 2);
}

#[tokio::test]
async fn an_old_run_can_never_overwrite_a_newer_triage() {
    let (w, old) = triaging().await;
    let newer = w.seed_run(hash(1), TriageTrigger::Manual { by: UserId::from_uuid(uuid(5)) }).await.run.id;
    w.h.tickets.begin_triage(org(), ticket_id(), &[TicketStatus::Triaging], hash(1), newer).await.unwrap();

    // Old run succeeds late.
    succeed(&w, old, result()).await;
    w.apply().run(org(), old).await.unwrap();
    let t = w.ticket().await;
    assert_eq!((t.status, t.triage_run), (TicketStatus::Triaging, Some(newer)));
    assert!(w.h.tickets.causes(org(), ticket_id(), true).await.unwrap().is_empty());
    assert!(notices(&w).is_empty());

    // Old run fails late.
    let (w, old) = triaging().await;
    let newer = w.seed_run(hash(1), TriageTrigger::Manual { by: UserId::from_uuid(uuid(5)) }).await.run.id;
    w.h.tickets.begin_triage(org(), ticket_id(), &[TicketStatus::Triaging], hash(1), newer).await.unwrap();
    w.drive(old, &[RunStatusEvent::Cancel], None, None).await;
    w.apply().run(org(), old).await.unwrap();
    assert_eq!(w.ticket().await.status, TicketStatus::Triaging);
}

#[tokio::test]
async fn content_changed_during_the_run_keeps_the_result_on_the_run_only() {
    let (w, run) = triaging().await;
    let mut t = w.ticket().await.into_data();
    t.content_hash = hash(2);
    w.put_ticket(&mage_core::domain::Ticket::rehydrate(t)).await;
    succeed(&w, run, result()).await;
    w.apply().run(org(), run).await.unwrap();
    assert_eq!(w.ticket().await.status, TicketStatus::Triaging);
    assert!(w.h.tickets.causes(org(), ticket_id(), true).await.unwrap().is_empty());
}

#[tokio::test]
async fn errored_runs_are_requeued_once_except_no_result_and_model_auth() {
    // Infrastructure error: one Requeue in 2 minutes; the ticket stays Triaging.
    let (w, run) = triaging().await;
    let error = RunError { code: RunErrorCode::Runner { stage: mage_core::domain::FailStage::Harness }, message: "crash".into() };
    w.drive(run, &[RunStatusEvent::InfraError], None, Some(error.clone())).await;
    w.apply().run(org(), run).await.unwrap();
    assert_eq!(w.ticket().await.status, TicketStatus::Triaging);
    let jobs = w.queued(JobKind::TriageTicket);
    assert_eq!(jobs.len(), 1);
    assert_eq!(
        jobs[0].payload,
        JobPayload::TriageTicket { org_id: org(), ticket_id: ticket_id(), content_hash: hash(1), trigger: TriageTrigger::Requeue }
    );
    assert_eq!(jobs[0].run_after, w.now() + Duration::from_secs(120));

    // A requeued run is never requeued again.
    let w = World::new().await;
    let run = w.seed_run(hash(1), TriageTrigger::Requeue).await.run.id;
    w.put_ticket(&ticket(TicketStatus::Triaging, hash(1), Some(hash(1)), Some(run))).await;
    w.drive(run, &[RunStatusEvent::InfraError], None, Some(error)).await;
    w.apply().run(org(), run).await.unwrap();
    assert_eq!(w.ticket().await.status, TicketStatus::TriageFailed);
    assert!(w.queued(JobKind::TriageTicket).is_empty());

    for code in [RunErrorCode::NoResult, RunErrorCode::ModelAuth] {
        let (w, run) = triaging().await;
        w.drive(run, &[RunStatusEvent::InfraError], None, Some(RunError { code, message: "m".into() })).await;
        w.apply().run(org(), run).await.unwrap();
        assert_eq!(w.ticket().await.status, TicketStatus::TriageFailed, "{code:?}");
        assert!(w.queued(JobKind::TriageTicket).is_empty(), "{code:?}");
    }
}

#[tokio::test]
async fn failed_timed_out_and_cancelled_runs_fail_the_ticket_without_notice() {
    use mage_core::domain::BudgetLimit;
    let cases: [&[RunStatusEvent]; 3] = [
        &[RunStatusEvent::ResultSubmitted { needs_gate: true }, RunStatusEvent::GateFailed { attempts_left: 0 }],
        &[RunStatusEvent::BudgetExceeded { limit: BudgetLimit::Wall }],
        &[RunStatusEvent::Cancel],
    ];
    for events in cases {
        let (w, run) = triaging().await;
        w.start_running(run).await;
        w.drive(run, events, None, None).await;
        assert!(w.record(run).await.run.status.is_terminal());
        w.apply().run(org(), run).await.unwrap();
        assert_eq!(w.ticket().await.status, TicketStatus::TriageFailed);
        assert!(notices(&w).is_empty());
    }
}

#[tokio::test]
async fn zero_accepted_causes_with_missing_info_is_a_success() {
    let (w, run) = triaging().await;
    succeed(&w, run, TriageResult { summary: "s".into(), causes: vec![], missing_info: vec!["Which customer?".into()] }).await;
    w.apply().run(org(), run).await.unwrap();
    assert_eq!(w.ticket().await.status, TicketStatus::Triaged);
    assert_eq!(notices(&w).len(), 1);
}

#[tokio::test]
async fn a_draft_failing_the_recheck_is_dropped_and_ranks_stay_contiguous() {
    let w = World::new().await;
    let view = w.source.view(&["src/billing/export.ts", "src/billing/gone.ts"]).await;
    let t = ticket(TicketStatus::Triaging, hash(1), Some(hash(1)), None);
    let r = TriageResult {
        summary: "s".into(),
        causes: vec![
            draft("Gone", "src/billing/gone.ts", 1, "this file does not exist", 0.9),
            draft("Null VAT id", "src/billing/export.ts", 30, VAT_QUOTE, 0.8),
        ],
        missing_info: vec![],
    };
    let mut n = 0;
    let causes = rebuild_causes(org(), &t, RunId::from_uuid(uuid(1)), &r, &view, || {
        n += 1;
        mage_core::domain::CauseId::from_uuid(uuid(n))
    });
    assert_eq!(causes.iter().map(|c| (c.rank, c.title.as_str())).collect::<Vec<_>>(), [(1, "Null VAT id")]);
}

#[tokio::test]
async fn eval_runs_record_the_case_result_and_touch_no_ticket() {
    let w = World::new().await;
    w.put_ticket(&ticket(TicketStatus::Resolved, hash(1), Some(hash(1)), None)).await;
    let eval = w
        .evals
        .create_eval(
            org(),
            NewTriageEval { name: "v".into(), corpus: "c".into(), variant_toml: "name = \"v\"".into(), repeat: 1, created_by: UserId::from_uuid(uuid(5)) },
        )
        .await
        .unwrap();
    w.evals.add_cases(org(), eval, &[EvalCase { ticket: ticket_id(), input_run: None, repeats: 1 }]).await.unwrap();
    w.evals
        .put_label(
            org(),
            &GroundTruth { ticket: ticket_id(), paths: paths(&["src/billing/totals.ts"]), strength: LabelStrength::Strong, source: LabelSource::Manual },
        )
        .await
        .unwrap();

    let mut input = rich_input();
    input.content_hash = hash(1);
    let mut spec = spec_for(input);
    spec.mode = crate::domain::triage::TriageMode::Eval { eval, repeat: 0, disabled_layers: vec![] };
    let mut create = crate::app::runs::CreateRun::new(org(), mage_core::domain::RunKind::Triage, mage_core::domain::RunSubject::Ticket(ticket_id()));
    create.prompt = Some(crate::app::runs::RunPrompt { prompt: "p".into(), system_append: "s".into() });
    create.triage = Some(spec);
    let run = w.h.manager.create_run(create).await.unwrap().run.id;
    w.evals.record_case_run(org(), eval, ticket_id(), 0, run).await.unwrap();
    w.h.clock.advance(Duration::from_secs(30));
    succeed(&w, run, result()).await;
    w.apply().run(org(), run).await.unwrap();

    assert_eq!(w.ticket().await.status, TicketStatus::Resolved);
    assert!(notices(&w).is_empty());
    let results = w.evals.case_results(org(), eval).await.unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!((results[0].status, results[0].causes, results[0].hit_rank), (RunStatus::Succeeded, 2, Some(2)));
    assert_eq!(results[0].queue_wait, Duration::from_secs(30));
}
