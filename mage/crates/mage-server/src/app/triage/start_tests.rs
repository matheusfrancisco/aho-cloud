//! StartTriage: every precondition row, layer policy, harness choice, fencing and convergence.

use std::time::Duration;

use mage_core::domain::{
    HarnessKind, JobKind, JobPayload, Layer, LayerEvent, LayerState, MemberRole, Membership, RepoVersion, RunError, RunErrorCode, RunId,
    RunStatus, RunStatusEvent, TicketStatus, TriageTrigger, UserId, VersionReason,
};
use mage_core::ports::RepoStore;

use super::*;
use crate::app::triage::testkit::*;
use crate::domain::triage::TriageMode;
use crate::ports::org::OrgStore;

const H1: u8 = 1;
const H2: u8 = 2;

async fn world_with(status: TicketStatus) -> World {
    let w = World::new().await;
    w.put_ticket(&ticket(status, hash(H1), None, None)).await;
    w
}

async fn start(w: &World, trigger: TriageTrigger) -> StartOutcome {
    let job = w.job(trigger, hash(H1), 1, 10);
    w.start().run(&job, &World::payload(&job)).await.unwrap()
}

fn started(outcome: &StartOutcome) -> RunId {
    match outcome {
        StartOutcome::Started { run, .. } => *run,
        other => panic!("expected Started, got {other:?}"),
    }
}

// ---- preconditions ------------------------------------------------------------------------------

#[tokio::test]
async fn missing_ticket_is_skipped() {
    let w = World::new().await;
    assert_eq!(start(&w, TriageTrigger::Auto).await, StartOutcome::Skipped(SkipReason::TicketMissing));
}

#[tokio::test]
async fn a_job_for_an_older_content_hash_is_stale() {
    let w = World::new().await;
    w.put_ticket(&ticket(TicketStatus::New, hash(H2), None, None)).await;
    assert_eq!(start(&w, TriageTrigger::Auto).await, StartOutcome::Skipped(SkipReason::Stale));
    assert!(w.live_runs().await.is_empty());
}

#[tokio::test]
async fn statuses_past_triage_refuse_and_triage_statuses_start() {
    for status in [
        TicketStatus::Diagnosing,
        TicketStatus::Diagnosed,
        TicketStatus::Fixing,
        TicketStatus::FixReady,
        TicketStatus::PrOpen,
        TicketStatus::Resolved,
        TicketStatus::Closed,
    ] {
        let w = world_with(status).await;
        assert_eq!(start(&w, TriageTrigger::Auto).await, StartOutcome::Skipped(SkipReason::Refused(status)), "{status:?}");
        assert_eq!(w.ticket().await.status, status, "skips never change the ticket");
    }
    for status in [TicketStatus::New, TicketStatus::Triaged, TicketStatus::TriageFailed] {
        let w = world_with(status).await;
        let run = started(&start(&w, TriageTrigger::Auto).await);
        let t = w.ticket().await;
        assert_eq!((t.status, t.triage_run, t.triaged_hash), (TicketStatus::Triaging, Some(run), Some(hash(H1))), "{status:?}");
    }
}

#[tokio::test]
async fn auto_job_for_an_already_triaged_hash_skips_and_cancels_other_in_flight_runs() {
    let w = world_with(TicketStatus::Triaged).await;
    let old = w.seed_run(hash(H2), TriageTrigger::Auto).await;
    w.h.clock.advance(Duration::from_secs(1));
    let newest = w.seed_run(hash(H1), TriageTrigger::Auto).await;
    assert_eq!(start(&w, TriageTrigger::Auto).await, StartOutcome::Skipped(SkipReason::AlreadyTriaged));
    assert_eq!(w.record(old.run.id).await.run.status, RunStatus::Cancelled);
    assert_eq!(w.record(newest.run.id).await.run.status, RunStatus::Queued);
}

#[tokio::test]
async fn manual_and_requeue_start_a_new_run_only_when_the_newest_run_predates_the_request() {
    for trigger in [TriageTrigger::Manual { by: UserId::from_uuid(uuid(5)) }, TriageTrigger::Requeue] {
        let w = world_with(TicketStatus::Triaged).await;
        let old = w.seed_run(hash(H1), TriageTrigger::Auto).await;
        w.h.clock.advance(Duration::from_secs(5));
        let job = w.job(trigger, hash(H1), 1, 10);
        let run = started(&w.start().run(&job, &World::payload(&job)).await.unwrap());
        assert_ne!(run, old.run.id);
        assert_eq!(w.record(run).await.spec.triage.unwrap().trigger, trigger);

        // The same request again finds a run created after it was enqueued.
        let again = w.start().run(&job, &World::payload(&job)).await.unwrap();
        assert_eq!(again, StartOutcome::Skipped(SkipReason::AlreadyTriaged), "{trigger:?}");
    }
}

#[tokio::test]
async fn auto_triage_disabled_skips_auto_but_not_manual() {
    let w = world_with(TicketStatus::New).await;
    w.set_settings(|s| s.auto_triage = false).await;
    assert_eq!(start(&w, TriageTrigger::Auto).await, StartOutcome::Skipped(SkipReason::AutoDisabled));
    started(&start(&w, TriageTrigger::Manual { by: UserId::from_uuid(uuid(5)) }).await);
}

#[tokio::test]
async fn auto_retriage_cap_counts_auto_runs_of_the_last_day_except_the_first_triage() {
    let w = world_with(TicketStatus::Triaged).await;
    w.set_settings(|s| s.auto_retriage_per_day = 2).await;
    // first triage (not counted) + 2 auto retriages within 24 h
    for n in 10..13u8 {
        w.seed_run(hash(n), TriageTrigger::Auto).await;
        w.h.clock.advance(Duration::from_secs(60));
    }
    assert_eq!(start(&w, TriageTrigger::Auto).await, StartOutcome::Skipped(SkipReason::RetriageCapReached));
    // Manual retriages bypass the auto cap.
    started(&start(&w, TriageTrigger::Manual { by: UserId::from_uuid(uuid(5)) }).await);

    // A day later the window is empty again.
    let w = world_with(TicketStatus::Triaged).await;
    w.set_settings(|s| s.auto_retriage_per_day = 2).await;
    for n in 10..13u8 {
        w.seed_run(hash(n), TriageTrigger::Auto).await;
    }
    w.h.clock.advance(Duration::from_secs(25 * 3600));
    started(&start(&w, TriageTrigger::Auto).await);
}

#[tokio::test]
async fn ticket_cost_cap_blocks_unless_an_admin_asks() {
    let w = world_with(TicketStatus::Triaged).await;
    w.set_settings(|s| s.ticket_cost_cap_micros = 1_000).await;
    let old = w.seed_run(hash(H2), TriageTrigger::Auto).await;
    w.set_cost(old.run.id, 1_000).await;

    assert_eq!(start(&w, TriageTrigger::Auto).await, StartOutcome::Skipped(SkipReason::TicketCostCapReached));
    let member = UserId::from_uuid(uuid(6));
    w.h.orgs.put_membership(&Membership { org_id: org(), user_id: member, role: MemberRole::Member }).await.unwrap();
    assert_eq!(start(&w, TriageTrigger::Manual { by: member }).await, StartOutcome::Skipped(SkipReason::TicketCostCapReached));
    let admin = UserId::from_uuid(uuid(7));
    w.h.orgs.put_membership(&Membership { org_id: org(), user_id: admin, role: MemberRole::Admin }).await.unwrap();
    started(&start(&w, TriageTrigger::Manual { by: admin }).await);
}

#[tokio::test]
async fn a_ticket_without_target_is_skipped() {
    let w = World::new().await;
    let mut t = ticket(TicketStatus::New, hash(H1), None, None).into_data();
    t.target = None;
    w.put_ticket(&mage_core::domain::Ticket::rehydrate(t)).await;
    assert_eq!(start(&w, TriageTrigger::Auto).await, StartOutcome::Skipped(SkipReason::NoTarget));
}

// ---- layers -------------------------------------------------------------------------------------

#[tokio::test]
async fn missing_repo_version_enqueues_index_version_once_and_defers() {
    let w = World::new().await;
    let mut t = ticket(TicketStatus::New, hash(H1), None, None).into_data();
    let other_sha = mage_core::domain::CommitSha::from_bytes([0x99; 20]);
    t.target.as_mut().unwrap().sha = other_sha;
    w.put_ticket(&mage_core::domain::Ticket::rehydrate(t)).await;

    for _ in 0..2 {
        let outcome = start(&w, TriageTrigger::Auto).await;
        let StartOutcome::Deferred { until, why } = outcome else { panic!("expected Deferred") };
        assert_eq!(until, w.now() + Duration::from_secs(30));
        assert_eq!(why, DeferReason::IndexPending { waiting_on: vec![Layer::Files] });
    }
    let index = w.queued(JobKind::IndexVersion);
    assert_eq!(index.len(), 1, "dedupe keeps one index.version job");
    assert_eq!(
        index[0].payload,
        JobPayload::IndexVersion { org_id: org(), repo_id: repo_id(), sha: other_sha, reason: VersionReason::OnDemand(ticket_id()) }
    );
    assert_eq!(w.ticket().await.status, TicketStatus::New);
}

#[tokio::test]
async fn files_pending_defers_then_times_out() {
    let w = World::with_layers(&[]).await;
    w.put_ticket(&ticket(TicketStatus::New, hash(H1), None, None)).await;
    let StartOutcome::Deferred { why, .. } = start(&w, TriageTrigger::Auto).await else { panic!("expected Deferred") };
    assert_eq!(why, DeferReason::IndexPending { waiting_on: vec![Layer::Files, Layer::Symbols, Layer::ErrorSites] });
    w.h.clock.advance(Duration::from_secs(31 * 60));
    assert_eq!(start(&w, TriageTrigger::Auto).await, StartOutcome::Skipped(SkipReason::IndexTimeout));
}

#[tokio::test]
async fn files_failed_is_skipped() {
    let w = World::with_layers(&[]).await;
    w.put_ticket(&ticket(TicketStatus::New, hash(H1), None, None)).await;
    w.h.repos.apply_layer_event(org(), key(), Layer::Files, LayerEvent::Start, &[]).await.unwrap();
    w.h.repos.apply_layer_event(org(), key(), Layer::Files, LayerEvent::Fail { reason: "dead".into() }, &[]).await.unwrap();
    assert_eq!(start(&w, TriageTrigger::Auto).await, StartOutcome::Skipped(SkipReason::IndexFailed(Layer::Files)));
}

#[tokio::test]
async fn preferred_layers_are_waited_on_then_the_run_proceeds_degraded() {
    let w = World::with_layers(&[Layer::Files, Layer::Refs, Layer::History]).await;
    w.put_ticket(&ticket(TicketStatus::New, hash(H1), None, None)).await;
    let StartOutcome::Deferred { why, .. } = start(&w, TriageTrigger::Auto).await else { panic!("expected Deferred") };
    assert_eq!(why, DeferReason::IndexPending { waiting_on: vec![Layer::Symbols, Layer::ErrorSites] });

    w.h.clock.advance(Duration::from_secs(11 * 60));
    let run = started(&start(&w, TriageTrigger::Auto).await);
    let spec = w.record(run).await.spec.triage.unwrap();
    assert_eq!(spec.degraded_layers, [Layer::Symbols, Layer::ErrorSites, Layer::AppMap, Layer::Semantic]);
}

#[test]
fn layer_policy_table() {
    let at = t0();
    let wait = Duration::from_secs(600);
    let files_wait = Duration::from_secs(1800);
    let mut v = RepoVersion::new(org(), key(), VersionReason::DefaultHead, at);
    let ready = LayerState::Ready { at };
    for l in Layer::ALL {
        v.layers.insert(l, ready.clone());
    }
    assert_eq!(layer_policy(&v, at, wait, files_wait), LayerVerdict::Proceed { degraded: vec![] });

    // Preferred failed: degraded immediately; optional never waited on.
    v.layers.insert(Layer::Symbols, LayerState::Failed { reason: "x".into() });
    v.layers.insert(Layer::Semantic, LayerState::Running);
    assert_eq!(layer_policy(&v, at, wait, files_wait), LayerVerdict::Proceed { degraded: vec![Layer::Symbols, Layer::Semantic] });

    // Preferred running: wait until layer_wait after the version was created.
    v.layers.insert(Layer::ErrorSites, LayerState::Running);
    assert_eq!(layer_policy(&v, at, wait, files_wait), LayerVerdict::Wait { waiting_on: vec![Layer::ErrorSites] });
    assert!(matches!(layer_policy(&v, at + wait, wait, files_wait), LayerVerdict::Proceed { .. }));

    // Files skipped is a failure; files missing from the map counts as pending.
    v.layers.insert(Layer::Files, LayerState::Skipped { reason: "x".into() });
    assert_eq!(layer_policy(&v, at, wait, files_wait), LayerVerdict::Failed(Layer::Files));
    v.layers.remove(&Layer::Files);
    assert!(matches!(layer_policy(&v, at, wait, files_wait), LayerVerdict::Wait { waiting_on } if waiting_on[0] == Layer::Files));
    assert_eq!(layer_policy(&v, at + files_wait, wait, files_wait), LayerVerdict::TimedOut);
}

// ---- run creation -------------------------------------------------------------------------------

#[tokio::test]
async fn started_run_carries_the_triage_spec_prompts_and_budget() {
    let w = world_with(TicketStatus::New).await;
    w.set_settings(|s| s.max_causes = 3).await;
    let job = w.job(TriageTrigger::Auto, hash(H1), 1, 10);
    let run = started(&w.start().run(&job, &World::payload(&job)).await.unwrap());
    let rec = w.record(run).await;
    let spec = rec.spec.triage.clone().unwrap();
    assert_eq!(spec.mode, TriageMode::Live);
    assert_eq!(spec.trigger, TriageTrigger::Auto);
    assert_eq!(spec.prompt_version, crate::app::triage::prompt::TRIAGE_PROMPT_VERSION);
    assert_eq!(spec.max_causes, 3);
    assert_eq!(spec.input.content_hash, hash(H1));
    assert_eq!(spec.input.identifier, "SUP-123");
    assert_eq!((spec.input.repo.owner.as_str(), spec.input.repo.name.as_str()), ("acme", "billing"));
    assert_eq!(rec.run.harness, Some(HarnessKind::Omp));
    assert_eq!(rec.run.budget, mage_core::wire::api::TriageSettings::default().budget);
    assert_eq!(rec.spec.model, "model-default");
    assert!(rec.spec.prompt.starts_with("Triage ticket SUP-123."));
    assert!(rec.spec.system_append.contains("At most 3 causes"));
    assert!(rec.spec.prompt.contains("-----BEGIN UNTRUSTED body "));
}

#[tokio::test]
async fn claude_code_falls_back_to_omp_in_production_until_verified() {
    let mut w = world_with(TicketStatus::New).await;
    w.cfg.production = true;
    w.set_settings(|s| s.harness = HarnessKind::ClaudeCode).await;
    let run = started(&start(&w, TriageTrigger::Auto).await);
    assert_eq!(w.record(run).await.run.harness, Some(HarnessKind::Omp));

    let mut w = world_with(TicketStatus::New).await;
    w.cfg.production = true;
    w.cfg.claude_code_gateway_verified = true;
    w.set_settings(|s| s.harness = HarnessKind::ClaudeCode).await;
    let run = started(&start(&w, TriageTrigger::Auto).await);
    assert_eq!(w.record(run).await.run.harness, Some(HarnessKind::ClaudeCode));
}

#[tokio::test]
async fn new_content_starts_a_new_run_and_cancels_the_old_one() {
    let w = World::new().await;
    w.put_ticket(&ticket(TicketStatus::New, hash(H2), None, None)).await;
    let job = w.job(TriageTrigger::Auto, hash(H2), 1, 10);
    let old = started(&w.start().run(&job, &World::payload(&job)).await.unwrap());

    let mut t = w.ticket().await.into_data();
    t.content_hash = hash(H1);
    w.put_ticket(&mage_core::domain::Ticket::rehydrate(t)).await;
    let outcome = start(&w, TriageTrigger::Auto).await;
    let StartOutcome::Started { run, cancelled } = outcome else { panic!("expected Started") };
    assert_eq!(cancelled, [old]);
    assert_eq!(w.record(old).await.run.status, RunStatus::Cancelled);
    let t = w.ticket().await;
    assert_eq!((t.status, t.triage_run, t.triaged_hash), (TicketStatus::Triaging, Some(run), Some(hash(H1))));
}

#[tokio::test]
async fn a_retry_after_a_crash_reuses_the_recorded_run_id_and_converges_to_one_run() {
    let w = World::new().await;
    // State left by an attempt that crashed after `begin_triage`: the ticket names a run that was
    // never created.
    let recorded = RunId::from_uuid(uuid(0xDEAD));
    w.put_ticket(&ticket(TicketStatus::Triaging, hash(H1), Some(hash(H1)), Some(recorded))).await;
    let job = w.job(TriageTrigger::Auto, hash(H1), 2, 10);
    let run = started(&w.start().run(&job, &World::payload(&job)).await.unwrap());
    assert_eq!(run, recorded);
    // The next retry finds the run and creates nothing.
    assert_eq!(w.start().run(&job, &World::payload(&job)).await.unwrap(), StartOutcome::Skipped(SkipReason::AlreadyTriaged));
    assert_eq!(w.live_runs().await.len(), 1);
    assert_eq!(w.ticket().await.triage_run, Some(recorded));
}

#[tokio::test]
async fn skip_while_the_fencing_run_ended_unsuccessfully_fails_the_ticket() {
    let w = World::new().await;
    let fence = w.seed_run(hash(H1), TriageTrigger::Requeue).await.run.id;
    w.put_ticket(&ticket(TicketStatus::Triaging, hash(H1), Some(hash(H1)), Some(fence))).await;
    w.drive(fence, &[RunStatusEvent::InfraError], None, Some(RunError { code: RunErrorCode::Internal, message: "boom".into() })).await;
    // A duplicate job for the same hash skips (AlreadyTriaged): the ticket must not hang Triaging.
    assert_eq!(start(&w, TriageTrigger::Auto).await, StartOutcome::Skipped(SkipReason::AlreadyTriaged));
    assert_eq!(w.ticket().await.status, TicketStatus::TriageFailed);

    // A stale job never touches the ticket.
    let w = World::new().await;
    let fence = w.seed_run(hash(H1), TriageTrigger::Requeue).await.run.id;
    w.put_ticket(&ticket(TicketStatus::Triaging, hash(H1), Some(hash(H1)), Some(fence))).await;
    w.drive(fence, &[RunStatusEvent::InfraError], None, None).await;
    let job = w.job(TriageTrigger::Requeue, hash(H2), 1, 10);
    assert_eq!(w.start().run(&job, &World::payload(&job)).await.unwrap(), StartOutcome::Skipped(SkipReason::Stale));
    assert_eq!(w.ticket().await.status, TicketStatus::Triaging);
}

#[tokio::test]
async fn skip_while_the_fencing_run_succeeded_leaves_the_ticket_to_apply_outcome() {
    let w = World::new().await;
    let fence = w.seed_run(hash(H1), TriageTrigger::Auto).await.run.id;
    w.put_ticket(&ticket(TicketStatus::Triaging, hash(H1), Some(hash(H1)), Some(fence))).await;
    w.start_running(fence).await;
    let result = mage_core::domain::TriageResult { summary: "s".into(), causes: vec![], missing_info: vec!["q".into()] };
    w.drive(fence, &[RunStatusEvent::ResultSubmitted { needs_gate: false }], Some(result), None).await;
    assert_eq!(start(&w, TriageTrigger::Auto).await, StartOutcome::Skipped(SkipReason::AlreadyTriaged));
    assert_eq!(w.ticket().await.status, TicketStatus::Triaging);
}

#[tokio::test]
async fn the_last_attempt_fails_the_ticket_when_the_run_cannot_be_created() {
    let w = World::new().await;
    w.put_ticket(&ticket(TicketStatus::New, hash(H1), None, None)).await;
    let job = w.job(TriageTrigger::Auto, hash(H1), 10, 10);
    // Another run already owns this job's create key.
    let mut create = crate::app::runs::CreateRun::new(org(), mage_core::domain::RunKind::Triage, mage_core::domain::RunSubject::Ticket(ticket_id()));
    let mut spec = spec_for(rich_input());
    spec.input.content_hash = hash(H2);
    create.prompt = Some(crate::app::runs::RunPrompt { prompt: "p".into(), system_append: "s".into() });
    create.triage = Some(spec);
    create.create_key = Some(job.lease.job_id.to_string());
    w.h.manager.create_run(create).await.unwrap();

    let err = w.start().run(&job, &World::payload(&job)).await.unwrap_err();
    assert!(matches!(err, AppError::Conflict(_)), "{err:?}");
    assert_eq!(w.ticket().await.status, TicketStatus::TriageFailed);

    // Before the last attempt the ticket stays Triaging for the retry.
    let w2 = World::new().await;
    w2.put_ticket(&ticket(TicketStatus::New, hash(H1), None, None)).await;
    let job2 = w2.job(TriageTrigger::Auto, hash(H1), 1, 10);
    let mut create = crate::app::runs::CreateRun::new(org(), mage_core::domain::RunKind::Triage, mage_core::domain::RunSubject::Ticket(ticket_id()));
    let mut spec = spec_for(rich_input());
    spec.input.content_hash = hash(H2);
    create.prompt = Some(crate::app::runs::RunPrompt { prompt: "p".into(), system_append: "s".into() });
    create.triage = Some(spec);
    create.create_key = Some(job2.lease.job_id.to_string());
    w2.h.manager.create_run(create).await.unwrap();
    assert!(w2.start().run(&job2, &World::payload(&job2)).await.is_err());
    assert_eq!(w2.ticket().await.status, TicketStatus::Triaging);
}

#[tokio::test]
async fn prior_hypotheses_and_customer_reach_the_snapshot() {
    use crate::ports::ticket::TicketStore;
    let w = World::new().await;
    // A ticket triaged before whose cause a diagnostic did not reproduce.
    let first = w.seed_run(hash(H2), TriageTrigger::Auto).await.run.id;
    w.put_ticket(&ticket(TicketStatus::Triaging, hash(H2), Some(hash(H2)), Some(first))).await;
    let view = w.source.view(&["src/billing/export.ts"]).await;
    let draft = mage_core::domain::CauseDraft {
        title: "Null VAT id".into(),
        explanation: "customer may be null".into(),
        evidence: vec![mage_core::domain::CitationDraft {
            path: path("src/billing/export.ts"),
            lines: mage_core::domain::LineRange::new(30, 30).unwrap(),
            quote: Some("const vat = customer.vatId.toUpperCase();".into()),
            note: None,
        }],
        repro_steps: vec![],
        confirm_if: "c".into(),
        rule_out_if: "r".into(),
        confidence: mage_core::domain::Confidence::new(0.5).unwrap(),
        method: mage_core::domain::ReproMethod::Ui,
    };
    let proof = mage_core::domain::evidence::check(&draft.evidence, &view).into_checked().unwrap();
    let t = w.ticket().await;
    let cause = mage_core::domain::Cause::new(mage_core::domain::CauseId::from_uuid(uuid(0xCA)), org(), &t, first, 1, draft, proof).unwrap();
    let write = crate::ports::ticket::TriageWrite {
        ticket: ticket_id(),
        expect: TicketStatus::Triaging,
        content_hash: hash(H2),
        triage_run: first,
        causes: vec![cause.clone()],
        next: TicketStatus::Triaged,
    };
    w.h.tickets.apply_triage(org(), write, &[]).await.unwrap();
    let diag = RunId::from_uuid(uuid(0xD1));
    w.h.tickets
        .set_cause_status(org(), cause.id, &mage_core::domain::CauseStatus::Untested, mage_core::domain::CauseStatus::Running(diag), None, &[])
        .await
        .unwrap();
    w.h.tickets
        .set_cause_status(
            org(),
            cause.id,
            &mage_core::domain::CauseStatus::Running(diag),
            mage_core::domain::CauseStatus::NotReproduced { run: diag },
            None,
            &[],
        )
        .await
        .unwrap();

    let mut t = w.ticket().await.into_data();
    t.content_hash = hash(H1);
    w.put_ticket(&mage_core::domain::Ticket::rehydrate(t)).await;
    let run = started(&start(&w, TriageTrigger::Auto).await);
    let input = w.record(run).await.spec.triage.unwrap().input;
    assert_eq!(input.prior_hypotheses.len(), 1);
    assert_eq!(input.prior_hypotheses[0].title, "Null VAT id");
    assert_eq!(input.prior_hypotheses[0].verdict, "not reproduced");
    assert_eq!(input.snapshot_at, w.now());
}
