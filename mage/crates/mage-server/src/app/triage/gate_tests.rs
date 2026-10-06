//! Evidence gate: validation, the decision table, feedback, salvage.

use std::sync::Arc;

use mage_core::domain::evidence::{CauseEvidence, CitationCheck, CitationDefect, QuoteCheck};
use mage_core::domain::{
    Gate, GateId, GateKind, GateStatus, HarnessKind, LineRange, Run, RunInit, RunKind, RunResult, RunStatusEvent, RunSubject, RunnerId,
    RunnerLease,
};
use mage_core::ports::Clock;
use mage_core::testing::{FakeClock, MemJobQueue, MemRepoStore, SeqIdGen};
use mage_core::wire::runner::{GateTicket, TriageSubmission};

use super::*;
use crate::app::triage::testkit::*;
use crate::ports::runs::{GateRecord, RunRecord};

struct Fx {
    gate: EvidenceGate,
    _source: SourceFixture,
}

async fn fx() -> Fx {
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::at(t0()));
    let repos = Arc::new(MemRepoStore::new(clock.clone(), Arc::new(MemJobQueue::new(clock.clone(), Arc::new(SeqIdGen::default())))));
    let source = SourceFixture::billing(clock, repos).await;
    Fx { gate: EvidenceGate::new(source.knowledge.clone(), source.reader.clone()), _source: source }
}

fn run(gate_attempts: u8) -> Run {
    Run::new(RunInit {
        id: mage_core::domain::RunId::from_uuid(uuid(0xE5)),
        org_id: org(),
        kind: RunKind::Triage,
        subject: RunSubject::Ticket(ticket_id()),
        harness: Some(HarnessKind::Omp),
        budget: budget(gate_attempts),
        created_at: t0(),
    })
    .unwrap()
}

fn spec(max_causes: u8) -> crate::domain::triage::TriageSpec {
    let mut s = spec_for(rich_input());
    s.max_causes = max_causes;
    s
}

async fn evaluate(fx: &Fx, attempt: u8, max_attempts: u8, sub: TriageSubmission) -> TriageGateDecision {
    fx.gate.evaluate_submission(org(), &run(max_attempts), &spec(5), attempt, sub).await.unwrap()
}

const VAT_QUOTE: &str = "const vat = customer.vatId.toUpperCase();";
const TAX_QUOTE: &str = "const tax = Math.round(net * taxRate * 100) / 100;";

fn good_vat() -> mage_core::wire::runner::CauseDraftDto {
    cause("Null customer on invoice export", 0.7, vec![cit("src/billing/export.ts", 28, 32, Some(VAT_QUOTE))])
}

fn good_tax() -> mage_core::wire::runner::CauseDraftDto {
    cause("Rounding in tax calc", 0.4, vec![cit("src/billing/totals.ts", 8, 12, Some(TAX_QUOTE))])
}

fn bad_vat() -> mage_core::wire::runner::CauseDraftDto {
    cause(
        "Null customer on invoice export",
        0.6,
        vec![cit("app/billing/export.ts", 120, 140, Some(VAT_QUOTE)), cit("src/billing/totals.ts", 20, 25, Some("this text is nowhere in the file"))],
    )
}

// ---- decision table -----------------------------------------------------------------------------

#[tokio::test]
async fn every_cause_valid_is_accepted_on_any_attempt() {
    let fx = fx().await;
    for (attempt, max) in [(1, 2), (2, 2)] {
        let d = evaluate(&fx, attempt, max, submission(vec![good_tax(), good_vat()])).await;
        let TriageGateDecision::Accept { result, details, message } = d else { panic!("expected Accept, got {d:?}") };
        let titles: Vec<&str> = result.causes.iter().map(|c| c.title.as_str()).collect();
        assert_eq!(titles, ["Null customer on invoice export", "Rounding in tax calc"], "ranked by confidence");
        assert!(details.causes.iter().all(|c| c.verdict == CauseEvidence::Valid));
        assert_eq!(details.salvage, None);
        assert_eq!(message, "Accepted 2 causes. End your turn now.");
    }
}

#[tokio::test]
async fn zero_causes_with_missing_info_is_accepted() {
    let fx = fx().await;
    let sub = TriageSubmission { summary: "Not enough detail.".into(), causes: vec![], missing_info: vec!["Which customer?".into()] };
    let d = evaluate(&fx, 1, 2, sub).await;
    let TriageGateDecision::Accept { result, message, .. } = d else { panic!("expected Accept, got {d:?}") };
    assert!(result.causes.is_empty());
    assert_eq!(result.missing_info, ["Which customer?"]);
    assert_eq!(message, "Accepted: no causes, 1 question for the reporter. End your turn now.");
}

#[tokio::test]
async fn partially_valid_rejects_with_salvage_before_the_final_attempt() {
    let fx = fx().await;
    let d = evaluate(&fx, 1, 2, submission(vec![bad_vat(), good_tax()])).await;
    let TriageGateDecision::Reject { details, feedback, attempts_left } = &d else { panic!("expected Reject, got {d:?}") };
    assert_eq!(*attempts_left, 1);
    let salvage = details.salvage.as_ref().expect("valid causes are salvaged");
    assert_eq!(salvage.causes.len(), 1);
    assert_eq!(salvage.causes[0].title, "Rounding in tax calc");
    assert_eq!(
        feedback,
        "REJECTED (attempt 1 of 2). Fix the problems and call submit_triage again with the COMPLETE list of causes, including the ones marked ok.\n\
         cause 1 \"Null customer on invoice export\": INVALID (no valid citation)\n  \
         evidence 1 app/billing/export.ts:120-140: path not found at 3f2a1c9e; similar: src/billing/export.ts\n  \
         evidence 2 src/billing/totals.ts:20-25: quote not found in or near these lines; re-read the file\n\
         cause 2 \"Rounding in tax calc\": ok"
    );
    assert_eq!(d.gate_failure_reason().as_deref(), Some("1 of 2 causes without verified evidence"));
    let gate = GateTicket { gate_id: GateId::from_uuid(uuid(9)), kind: GateKind::Evidence, attempt: 1 };
    let rejection = d.rejection(Some(gate)).unwrap();
    assert_eq!(rejection.gate, Some(gate));
    assert_eq!(rejection.message, *feedback);
    let item_paths: Vec<&str> = rejection.items.iter().map(|i| i.path.as_str()).collect();
    assert_eq!(item_paths, ["causes[0]", "causes[0].evidence[0]", "causes[0].evidence[1]"]);
}

#[tokio::test]
async fn partially_valid_on_the_final_attempt_accepts_the_valid_subset() {
    let fx = fx().await;
    let d = evaluate(&fx, 2, 2, submission(vec![bad_vat(), good_tax()])).await;
    let TriageGateDecision::Accept { result, details, message } = d else { panic!("expected Accept, got {d:?}") };
    assert_eq!(result.causes.len(), 1);
    assert_eq!(result.causes[0].title, "Rounding in tax calc");
    assert_eq!(details.invalid_causes(), 1);
    assert_eq!(message, "Accepted 1 cause; 1 cause without verified evidence dropped. End your turn now.");
}

#[tokio::test]
async fn all_invalid_rejects_without_salvage_then_fails_on_the_final_attempt() {
    let fx = fx().await;
    let d = evaluate(&fx, 1, 2, submission(vec![bad_vat()])).await;
    let TriageGateDecision::Reject { details, .. } = &d else { panic!("expected Reject, got {d:?}") };
    assert_eq!(details.salvage, None);

    let d = evaluate(&fx, 2, 2, submission(vec![bad_vat()])).await;
    let TriageGateDecision::Fail { message, details } = &d else { panic!("expected Fail, got {d:?}") };
    assert!(message.starts_with("REJECTED (attempt 2 of 2, final)."), "{message}");
    assert_eq!(details.invalid_causes(), 1);
    assert_eq!(d.gate_failure_reason().as_deref(), Some("no cause with verified evidence"));
    assert!(d.rejection(None).is_some());
}

#[tokio::test]
async fn reject_then_corrected_resubmission_is_accepted() {
    let fx = fx().await;
    assert!(matches!(evaluate(&fx, 1, 2, submission(vec![bad_vat(), good_tax()])).await, TriageGateDecision::Reject { .. }));
    let d = evaluate(&fx, 2, 2, submission(vec![good_vat(), good_tax()])).await;
    let TriageGateDecision::Accept { result, .. } = d else { panic!("expected Accept, got {d:?}") };
    assert_eq!(result.causes.len(), 2);
}

#[tokio::test]
async fn dropped_citations_alone_never_reject() {
    let fx = fx().await;
    let sub = submission(vec![cause(
        "Null customer",
        0.7,
        vec![
            cit("src/billing/export.ts", 28, 32, Some(VAT_QUOTE)),
            cit("src/billing/export.ts", 300, 320, None),
            cit("../etc/passwd", 1, 2, None),
            cit("src/billing/link.ts", 1, 1, None),
            cit("assets/logo.png", 1, 1, None),
            cit("src/billing/totals.ts", 9, 5, None),
        ],
    )]);
    let d = evaluate(&fx, 1, 2, sub).await;
    let TriageGateDecision::Accept { result, details, .. } = d else { panic!("expected Accept, got {d:?}") };
    assert_eq!(result.causes[0].evidence.len(), 1, "only the verified citation is kept");
    let checks = &details.causes[0].citations;
    assert!(matches!(checks[0], CitationCheck::Ok { quote: QuoteCheck::Exact, .. }));
    assert_eq!(checks[1], CitationCheck::Rejected(CitationDefect::LineOutOfRange { line_count: 60 }));
    assert_eq!(checks[2], CitationCheck::Rejected(CitationDefect::BadPath));
    assert_eq!(checks[3], CitationCheck::Rejected(CitationDefect::NotAFile));
    assert_eq!(checks[4], CitationCheck::Rejected(CitationDefect::NotText));
    assert_eq!(checks[5], CitationCheck::Rejected(CitationDefect::BadRange));
}

#[tokio::test]
async fn misplaced_quote_is_adjusted_and_the_result_carries_the_matched_lines() {
    let fx = fx().await;
    let sub = submission(vec![cause("Null customer", 0.7, vec![cit("./src/billing/export.ts", 40, 45, Some(VAT_QUOTE))])]);
    let d = evaluate(&fx, 1, 2, sub).await;
    let TriageGateDecision::Accept { result, details, .. } = d else { panic!("expected Accept, got {d:?}") };
    assert_eq!(result.causes[0].evidence[0].lines, LineRange::new(30, 30).unwrap());
    assert_eq!(result.causes[0].evidence[0].path.as_str(), "src/billing/export.ts");
    assert_eq!(details.adjusted_citations(), 1);
}

#[tokio::test]
async fn unquoted_citations_make_a_cause_invalid_with_a_quote_hint() {
    let fx = fx().await;
    let sub = submission(vec![cause("Null customer", 0.7, vec![cit("src/billing/export.ts", 28, 32, None)]), good_tax()]);
    let d = evaluate(&fx, 1, 2, sub).await;
    let TriageGateDecision::Reject { details, feedback, .. } = d else { panic!("expected Reject, got {d:?}") };
    assert_eq!(details.causes[0].verdict, CauseEvidence::NoVerifiedQuote);
    assert!(feedback.contains("cause 1 \"Null customer\": INVALID (no citation with a verified quote)"), "{feedback}");
    assert!(feedback.contains("evidence 1 src/billing/export.ts:28-32: no quote;"), "{feedback}");
}

#[tokio::test]
async fn duplicate_causes_are_merged_with_a_notice() {
    let fx = fx().await;
    let a = cause("Null customer A", 0.8, vec![cit("src/billing/export.ts", 28, 32, Some(VAT_QUOTE))]);
    let b = cause("Null customer B", 0.5, vec![cit("src/billing/export.ts", 29, 32, Some(VAT_QUOTE))]);
    let d = evaluate(&fx, 1, 2, submission(vec![a, b])).await;
    let TriageGateDecision::Accept { result, details, message } = d else { panic!("expected Accept, got {d:?}") };
    assert_eq!(result.causes.len(), 1);
    assert_eq!(result.causes[0].title, "Null customer A");
    assert_eq!(details.notices.len(), 1);
    assert_eq!(message, "Accepted 1 cause; 1 duplicate cause merged. End your turn now.");
}

// ---- validation ---------------------------------------------------------------------------------

#[test]
fn validation_reports_every_problem_with_its_path() {
    let mut bad = cause("", 1.5, vec![cit("", 0, 1, Some("   ")), cit("a.ts", 1, 1, None)]);
    bad.explanation = "line\u{7}bell".into();
    bad.repro_steps = vec!["x".repeat(501)];
    let sub = TriageSubmission { summary: "  ".into(), causes: vec![bad], missing_info: vec![] };
    let problems: Vec<String> = validate_submission(&sub, 5).into_iter().map(|p| format!("{}: {}", p.path, p.reason)).collect();
    assert_eq!(
        problems,
        [
            "summary: must not be empty",
            "causes[0].title: must not be empty",
            "causes[0].explanation: contains control characters",
            "causes[0].repro_steps[0]: longer than 500 characters",
            "causes[0].confidence: must be a number from 0 to 1",
            "causes[0].evidence[0].path: must not be empty",
            "causes[0].evidence[0].start_line: lines are 1-based",
            "causes[0].evidence[0].quote: must not be empty",
        ]
    );
}

#[test]
fn zero_causes_need_missing_info_and_cause_count_respects_max_causes() {
    let empty = TriageSubmission { summary: "s".into(), causes: vec![], missing_info: vec![] };
    let p = validate_submission(&empty, 5);
    assert_eq!(p.len(), 1);
    assert_eq!(p[0].path, "missing_info");

    let three = submission(vec![good_vat(), good_tax(), good_vat()]);
    assert!(validate_submission(&three, 3).is_empty());
    let p = validate_submission(&three, 2);
    assert_eq!(p[0].reason, "at most 2 causes for this triage");
}

#[test]
fn quotes_may_carry_carriage_returns_but_prose_may_not() {
    let mut c = cause("t", 0.5, vec![cit("a.ts", 1, 2, Some("line one\r\nline two"))]);
    assert!(validate_submission(&submission(vec![c.clone()]), 5).is_empty());
    c.title = "t\r".into();
    assert_eq!(validate_submission(&submission(vec![c]), 5)[0].path, "causes[0].title");
}

#[tokio::test]
async fn invalid_submission_consumes_no_gate_attempt() {
    let fx = fx().await;
    let sub = TriageSubmission { summary: String::new(), causes: vec![], missing_info: vec![] };
    let d = evaluate(&fx, 2, 2, sub).await;
    let TriageGateDecision::Invalid { problems } = &d else { panic!("expected Invalid, got {d:?}") };
    assert_eq!(problems.len(), 2);
    let r = d.rejection(None).unwrap();
    assert!(r.message.starts_with("INVALID submission (no gate attempt used)."));
    assert_eq!(r.items, *problems);
    assert_eq!(d.gate_failure_reason(), None);
}

// ---- salvage ------------------------------------------------------------------------------------

fn record(run: Run) -> RunRecord {
    let mut spec = crate::testing::runs::sample_spec(RunKind::Triage);
    spec.triage = Some(spec_for(rich_input()));
    RunRecord::new(run, spec, true)
}

fn running() -> Run {
    let mut r = run(2);
    let runner = RunnerLease { runner_id: RunnerId::from_uuid(uuid(77)), until: t0() };
    r.apply(RunStatusEvent::Lease { runner }, t0()).unwrap();
    r.apply(RunStatusEvent::EnvPreparing, t0()).unwrap();
    r.apply(RunStatusEvent::Started, t0()).unwrap();
    r
}

fn gate_row(attempt: u8, status: GateStatus, details: &EvidenceGateDetails) -> GateRecord {
    GateRecord {
        gate: Gate {
            id: GateId::from_uuid(uuid(u128::from(attempt))),
            org_id: org(),
            run_id: run(2).id,
            kind: GateKind::Evidence,
            attempt,
            status,
            verify_run: None,
        },
        feedback: None,
        created_at: t0(),
        details: Some(details.to_value().unwrap()),
    }
}

#[tokio::test]
async fn salvage_returns_the_valid_causes_of_a_newest_reject_only() {
    let fx = fx().await;
    let TriageGateDecision::Reject { details: partial, .. } = evaluate(&fx, 1, 3, submission(vec![bad_vat(), good_tax()])).await else {
        panic!()
    };
    let TriageGateDecision::Reject { details: none_valid, .. } = evaluate(&fx, 2, 3, submission(vec![bad_vat()])).await else { panic!() };
    let failed = GateStatus::Failed { reason: "r".into(), at: t0() };
    let rec = record(running());

    let gates = [gate_row(1, failed.clone(), &partial)];
    let salvaged = fx.gate_salvage(&rec, &gates).expect("reject with valid causes salvages");
    assert_eq!(salvaged.causes.len(), 1);

    // The newest attempt decides: a later reject with no valid cause salvages nothing.
    let gates = [gate_row(1, failed.clone(), &partial), gate_row(2, failed.clone(), &none_valid)];
    assert_eq!(fx.gate_salvage(&rec, &gates), None);
    assert_eq!(fx.gate_salvage(&rec, &[]), None);

    // Only a Running triage run without a result is salvaged.
    let queued = record(run(2));
    assert_eq!(fx.gate_salvage(&queued, &[gate_row(1, failed.clone(), &partial)]), None);
    let mut done = running();
    done.set_result(RunResult::Triage(partial.salvage.clone().unwrap())).unwrap();
    assert_eq!(fx.gate_salvage(&record(done), &[gate_row(1, failed, &partial)]), None);
}

impl Fx {
    fn gate_salvage(&self, rec: &RunRecord, gates: &[GateRecord]) -> Option<mage_core::domain::TriageResult> {
        crate::app::runs::TriageGate::salvage(&self.gate, rec, gates)
    }
}

#[tokio::test]
async fn runs_decision_carries_details_as_json_and_notices() {
    let fx = fx().await;
    let a = cause("Null customer A", 0.8, vec![cit("src/billing/export.ts", 28, 32, Some(VAT_QUOTE))]);
    let b = cause("Null customer B", 0.5, vec![cit("src/billing/export.ts", 29, 32, Some(VAT_QUOTE))]);
    let rec = record(running());
    let decision = crate::app::runs::TriageGate::evaluate(&fx.gate, org(), &rec, 1, submission(vec![a, b])).await.unwrap();
    let crate::app::runs::EvidenceDecision::Accept { details, notices, .. } = decision else { panic!("expected Accept") };
    assert_eq!(notices.len(), 1);
    let back = EvidenceGateDetails::from_value(&details).unwrap();
    assert_eq!(back.causes.len(), 2);
}
