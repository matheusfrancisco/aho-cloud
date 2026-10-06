//! Eval harness: metrics, report, labels, variant parsing, eval case replay.

use std::collections::BTreeMap;
use std::time::Duration;

use mage_core::domain::{
    CauseDraft, CitationDraft, Confidence, JobKind, LineRange, ReproMethod, RunStatus, TicketStatus, TriageEvalId, TriageTrigger, Usage,
};
use mage_core::ports::KnowledgeStore;
use mage_core::ports::knowledge::{CommitRecord, LinkSource, TicketLink};
use mage_core::wire::indexer::{ChangeKind, CommitInfo, FileChange};
use url::Url;

use super::*;
use crate::app::triage::testkit::*;
use crate::ports::ticket::TicketStore;

fn result_row(ticket: u128, hit: Option<u16>, primary: Option<u16>) -> EvalCaseResult {
    EvalCaseResult {
        eval: TriageEvalId::from_uuid(uuid(1)),
        ticket: TicketId::from_uuid(uuid(ticket)),
        repeat: 0,
        run: RunId::from_uuid(uuid(1000 + ticket)),
        status: RunStatus::Succeeded,
        hit_rank: hit,
        primary_hit_rank: primary,
        causes: 3,
        usage: Usage::default(),
        queue_wait: Duration::ZERO,
        agent_time: Duration::ZERO,
        gate_attempts: 1,
        dropped_causes: 0,
        adjusted_citations: 0,
    }
}

#[test]
fn recall_at_k_and_mrr() {
    let rows = [result_row(1, Some(1), Some(1)), result_row(2, Some(2), Some(4)), result_row(3, Some(4), None), result_row(4, None, None)];
    let m = recall_metrics(rows.iter());
    assert_eq!(m.cases, 4);
    assert_eq!(m.recall_at, [0.25, 0.5, 0.75]);
    assert_eq!(m.primary_recall_at, [0.25, 0.25, 0.5]);
    assert_eq!(m.mrr, (1.0 + 0.5 + 0.25) / 4.0);
    assert_eq!(recall_metrics([].iter()), RecallMetrics::default());
}

fn cause_at(paths_and_quotes: &[(&str, bool)]) -> CauseDraft {
    CauseDraft {
        title: "t".into(),
        explanation: "e".into(),
        evidence: paths_and_quotes
            .iter()
            .map(|(p, quoted)| CitationDraft { path: path(p), lines: LineRange::new(1, 1).unwrap(), quote: quoted.then(|| "q".into()), note: None })
            .collect(),
        repro_steps: vec![],
        confirm_if: "c".into(),
        rule_out_if: "r".into(),
        confidence: Confidence::new(0.5).unwrap(),
        method: ReproMethod::Unknown,
    }
}

#[test]
fn hit_rank_uses_any_citation_and_primary_rank_only_the_first_verified_one() {
    let causes = [cause_at(&[("a.ts", true)]), cause_at(&[("b.ts", true), ("label.ts", false)]), cause_at(&[("label.ts", true)])];
    assert_eq!(hit_ranks(&causes, &paths(&["label.ts"])), (Some(2), Some(3)));
    assert_eq!(hit_ranks(&causes, &paths(&["zzz.ts"])), (None, None));
    assert_eq!(hit_ranks(&[], &paths(&["a.ts"])), (None, None));
}

fn outcome(ticket: u128, hit: Option<u16>, strength: Option<LabelStrength>, cited: &[&str], label: &[&str]) -> CaseOutcome {
    CaseOutcome {
        result: result_row(ticket, hit, hit),
        label: strength.map(|s| (s, paths(label))),
        cited: paths(cited),
        salvaged: false,
        dropped_citations: 0,
        first_attempt_rejected: false,
        tool_calls: BTreeMap::new(),
        answering_layers: BTreeMap::new(),
    }
}

#[test]
fn report_splits_strong_labels_and_computes_precision_rates_and_distributions() {
    let mut cases = vec![
        outcome(1, Some(1), Some(LabelStrength::Strong), &["a.ts", "b.ts"], &["a.ts"]),
        outcome(2, None, Some(LabelStrength::Strong), &["c.ts"], &["a.ts"]),
        outcome(3, Some(3), Some(LabelStrength::Medium), &["x.ts"], &["x.ts"]),
        outcome(4, Some(1), None, &["a.ts"], &[]),
    ];
    cases[1].result.causes = 0;
    cases[1].first_attempt_rejected = true;
    cases[2].result.status = RunStatus::TimedOut;
    cases[2].result.gate_attempts = 0;
    cases[3].salvaged = true;
    for (i, c) in cases.iter_mut().enumerate() {
        c.result.usage.cost_micros = 100 * (i as u64 + 1);
        c.tool_calls.insert("read_file".into(), 2);
    }
    let eval = TriageEval { id: TriageEvalId::from_uuid(uuid(1)), name: "v".into(), variant_toml: String::new(), created_at: t0(), cases: 5, done: 4 };
    let r = compute_report(&eval, &cases);
    assert_eq!(r.unlabeled, 1);
    assert_eq!(r.all.cases, 3);
    assert_eq!(r.strong.cases, 2);
    assert_eq!(r.strong.recall_at, [0.5, 0.5, 0.5]);
    assert_eq!(r.all.recall_at[1], 2.0 / 3.0);
    // cited paths of labeled cases: a,b (1 in label), c (0), x (1) → 2 of 4.
    assert_eq!(r.citation_precision, 0.5);
    // accepted (Succeeded) cases: 1, 2, 4; one has zero causes.
    assert_eq!(r.zero_cause_rate, 1.0 / 3.0);
    assert_eq!(r.gate.attempt1_rejection_rate, 1.0 / 3.0);
    assert_eq!(r.gate.salvaged, 1);
    assert_eq!(r.outcomes.get("succeeded"), Some(&3));
    assert_eq!(r.outcomes.get("timed_out"), Some(&1));
    assert_eq!(r.cost_micros.total, 1000.0);
    assert_eq!(r.cost_micros.p50, 200.0);
    assert_eq!(r.cost_micros.p95, 400.0);
    assert_eq!(r.tool_calls.get("read_file"), Some(&8));
    let (lo, hi) = r.strong_recall3_ci95.unwrap();
    assert!(0.0 <= lo && lo <= 0.5 && 0.5 <= hi && hi <= 1.0, "{lo} {hi}");
}

#[test]
fn bootstrap_is_deterministic_and_degenerate_for_identical_tickets() {
    let rows: Vec<EvalCaseResult> = (1..=4).map(|t| result_row(t, Some(1), Some(1))).collect();
    let clusters: Vec<Vec<&EvalCaseResult>> = rows.iter().map(|r| vec![r]).collect();
    assert_eq!(bootstrap_recall3(&clusters), Some((1.0, 1.0)));
    assert_eq!(bootstrap_recall3(&[]), None);
    let mixed: Vec<EvalCaseResult> = (1..=10).map(|t| result_row(t, (t % 2 == 0).then_some(1), None)).collect();
    let clusters: Vec<Vec<&EvalCaseResult>> = mixed.iter().map(|r| vec![r]).collect();
    assert_eq!(bootstrap_recall3(&clusters), bootstrap_recall3(&clusters));
}

#[test]
fn label_exclusions() {
    for p in [
        "src/billing/export.test.ts",
        "pkg/api/handler_test.go",
        "tests/test_export.py",
        "src/__tests__/a.ts",
        "docs/runbook.md",
        "README.md",
        "package-lock.json",
        "go.sum",
        "gen/api.pb.go",
        "web/dist/app.min.js",
        "vendor/lib/x.go",
    ] {
        assert!(excluded_from_label(&path(p)), "{p}");
    }
    for p in ["src/billing/export.ts", "pkg/api/handler.go", "app/models/contest.rb", "src/latest/doc_builder.ts"] {
        assert!(!excluded_from_label(&path(p)), "{p}");
    }
}

#[test]
fn pr_numbers_from_github_urls() {
    assert_eq!(pr_number("/acme/billing/pull/412"), Some(412));
    assert_eq!(pr_number("/acme/billing/pull/412/"), Some(412));
    assert_eq!(pr_number("/acme/billing/issues/412"), None);
    assert_eq!(pr_number("/acme/billing/pull/abc"), None);
}

#[test]
fn variant_files_parse_with_defaults_and_reject_unknown_keys() {
    let v = EvalVariant::parse(
        "name = \"omp-v2-prompt\"\nharness = \"omp\"\nmodel = \"m\"\nprompt_version = 2\ndisabled_layers = [\"semantic\"]\nseeds = true\n[budget]\nmax_cost_micros = 2000000\n",
    )
    .unwrap();
    assert_eq!(v.harness, Some(mage_core::domain::HarnessKind::Omp));
    assert_eq!(v.disabled_layers, [mage_core::domain::Layer::Semantic]);
    assert_eq!(v.budget.max_cost_micros, Some(2_000_000));
    let minimal = EvalVariant::parse("name = \"x\"").unwrap();
    assert_eq!((minimal.prompt_version, minimal.seeds, minimal.concurrency), (crate::app::triage::prompt::TRIAGE_PROMPT_VERSION, true, None));
    assert!(EvalVariant::parse("name = \"x\"\nbogus = 1").is_err());
    assert!(EvalVariant::parse("name = \"\"").is_err());
    let with = with_concurrency("name = \"x\"\n[budget]\nmax_turns = 5\n", Some(2)).unwrap();
    let v = EvalVariant::parse(&with).unwrap();
    assert_eq!((v.concurrency, v.budget.max_turns), (Some(2), Some(5)));
}

// ---- use cases over the world -------------------------------------------------------------------

/// A resolved, labeled ticket whose first triage run (input) exists.
async fn labeled_world() -> (World, RunId) {
    let w = World::new().await;
    let first = w.seed_run(hash(1), TriageTrigger::Auto).await.run.id;
    w.put_ticket(&ticket(TicketStatus::Resolved, hash(1), Some(hash(1)), Some(first))).await;
    w.evals
        .put_label(
            org(),
            &GroundTruth { ticket: ticket_id(), paths: paths(&["src/billing/export.ts"]), strength: LabelStrength::Strong, source: LabelSource::Manual },
        )
        .await
        .unwrap();
    (w, first)
}

fn start_eval(w: &World) -> StartTriageEval {
    StartTriageEval::new(w.h.tickets.clone(), w.h.runs.clone(), w.evals.clone(), w.h.jobs.clone())
}

fn request(variant: &str, repeat: u8, concurrency: Option<u16>) -> StartTriageEvalRequest {
    StartTriageEvalRequest {
        corpus: "resolved".into(),
        variant_toml: variant.into(),
        repeat,
        limit: None,
        concurrency,
        created_by: mage_core::domain::UserId::from_uuid(uuid(5)),
    }
}

fn eval_job(w: &World, eval: TriageEvalId, repeat: u8) -> mage_core::ports::jobs::LeasedJob {
    let mut job = w.job(TriageTrigger::Auto, hash(1), 1, 10);
    job.kind = JobKind::TriageEvalCase;
    job.payload = mage_core::domain::JobPayload::TriageEvalCase { org_id: org(), eval_id: eval, ticket_id: ticket_id(), repeat };
    job
}

#[tokio::test]
async fn an_eval_enqueues_one_case_job_per_ticket_and_repeat() {
    let (w, first) = labeled_world().await;
    let eval = start_eval(&w).run(org(), request("name = \"v\"", 2, None)).await.unwrap();
    assert_eq!(eval.cases, 2);
    let cases = w.evals.cases(org(), eval.id).await.unwrap();
    assert_eq!(cases, [EvalCase { ticket: ticket_id(), input_run: Some(first), repeats: 2 }]);
    assert_eq!(w.queued(JobKind::TriageEvalCase).len(), 2);

    // Unlabeled tickets are excluded; nothing labeled → refused.
    let w = World::new().await;
    w.put_ticket(&ticket(TicketStatus::Resolved, hash(1), None, None)).await;
    assert!(matches!(start_eval(&w).run(org(), request("name = \"v\"", 1, None)).await, Err(AppError::Invalid(_))));
}

#[tokio::test]
async fn an_eval_case_replays_the_stored_input_as_an_eval_run_once() {
    let (w, first) = labeled_world().await;
    w.h.clock.advance(Duration::from_secs(3600));
    let eval = start_eval(&w).run(org(), request("name = \"v\"\ndisabled_layers = [\"history\"]\nseeds = false\n", 1, None)).await.unwrap();
    let job = eval_job(&w, eval.id, 0);
    let StartOutcome::Started { run, .. } = w.eval_case().run(&job).await.unwrap() else { panic!("expected Started") };
    let rec = w.record(run).await;
    let spec = rec.spec.triage.clone().unwrap();
    let stored = w.record(first).await.spec.triage.unwrap().input;
    assert_eq!(spec.mode, TriageMode::Eval { eval: eval.id, repeat: 0, disabled_layers: vec![mage_core::domain::Layer::History] });
    assert_eq!(spec.input.snapshot_at, stored.snapshot_at, "the first triage's input is replayed");
    assert_eq!(spec.input.seeds, None, "variant disabled seeds");
    assert!(spec.degraded_layers.contains(&mage_core::domain::Layer::History));
    assert_eq!(spec.memory_cutoff(ticket_id()).as_of, Some(stored.snapshot_at));
    assert_eq!(spec.disabled_layers(), [mage_core::domain::Layer::History]);
    assert_eq!(w.evals.case_run(org(), eval.id, ticket_id(), 0).await.unwrap(), Some(run));
    // The ticket is untouched.
    assert_eq!(w.ticket().await.status, TicketStatus::Resolved);
    // Replaying the job finds the recorded run.
    assert_eq!(w.eval_case().run(&job).await.unwrap(), StartOutcome::Started { run, cancelled: vec![] });
    assert_eq!(w.h.runs.all().iter().filter(|r| r.spec.triage.as_ref().is_some_and(|t| t.is_eval())).count(), 1);
}

#[tokio::test]
async fn eval_concurrency_defers_extra_cases() {
    let (w, _) = labeled_world().await;
    let eval = start_eval(&w).run(org(), request("name = \"v\"\nseeds = false\n", 2, Some(1))).await.unwrap();
    assert!(matches!(w.eval_case().run(&eval_job(&w, eval.id, 0)).await.unwrap(), StartOutcome::Started { .. }));
    let deferred = w.eval_case().run(&eval_job(&w, eval.id, 1)).await.unwrap();
    assert!(matches!(deferred, StartOutcome::Deferred { why: DeferReason::EvalConcurrency { in_flight: 1 }, .. }), "{deferred:?}");
}

#[tokio::test]
async fn auto_labels_come_from_linked_fix_commits_else_a_reproduced_confirmed_cause() {
    let w = World::new().await;
    w.put_ticket(&ticket(TicketStatus::Triaged, hash(1), Some(hash(1)), None)).await;
    let t = w.ticket().await;
    let resolution = mage_core::domain::Resolution::new(
        &t,
        None,
        "null VAT id".into(),
        Some(Url::parse("https://github.com/acme/billing/pull/412").unwrap()),
        None,
        mage_core::domain::UserId::from_uuid(uuid(5)),
        t0(),
    )
    .unwrap();
    w.h.tickets.resolve(org(), resolution, TicketStatus::Triaged, &[]).await.unwrap();
    let fix_commit = mage_core::domain::CommitSha::from_bytes([0x44; 20]);
    w.source
        .knowledge
        .put_ticket_links(org(), repo_id(), &[TicketLink { identifier: "sup-123".into(), source: LinkSource::PrTitle, commit: Some(fix_commit), pr: Some(412) }])
        .await
        .unwrap();
    let change = |p: &str| FileChange { path: path(p), old_path: None, change: ChangeKind::Modified, adds: Some(1), dels: Some(1) };
    let info = CommitInfo {
        sha: fix_commit,
        tree: mage_core::domain::TreeOid::from_bytes([0x45; 20]),
        parents: vec![sha()],
        author_name: "dev".into(),
        author_time: t0(),
        committer_time: t0(),
        subject: "SUP-123: guard null VAT id".into(),
        body: String::new(),
        files: Some(vec![change("src/billing/export.ts"), change("src/billing/export.test.ts"), change("package-lock.json")]),
    };
    w.source.knowledge.put_commits(org(), repo_id(), &[CommitRecord { info }]).await.unwrap();

    let labeler = LabelTriageEval::new(w.h.tickets.clone(), w.source.knowledge.clone(), w.evals.clone());
    let written = labeler.auto(org()).await.unwrap();
    assert_eq!(
        written,
        [GroundTruth { ticket: ticket_id(), paths: paths(&["src/billing/export.ts"]), strength: LabelStrength::Strong, source: LabelSource::FixPr }]
    );

    // A manual label wins over auto labeling.
    let manual = labeler.put_manual(org(), ticket_id(), paths(&["src/billing/totals.ts"])).await.unwrap();
    assert!(labeler.auto(org()).await.unwrap().is_empty());
    assert_eq!(w.evals.labels(org(), &[ticket_id()]).await.unwrap(), [manual]);
    assert!(matches!(labeler.put_manual(org(), ticket_id(), BTreeSet::new()).await, Err(AppError::Invalid(_))));
}
