//! Notify (intake.md "Testing"): second run after `Sent` sends nothing; an uncertain Slack attempt
//! finds the earlier reply and does not post; a Linear retry reuses `client_id`; `NoAccess` →
//! `Skipped`; `RateLimited` → retry at `retry_at`; the mp4 is uploaded once and embedded.

use std::time::Duration;

use mage_core::domain::{
    Budget, Cause, CauseData, CauseId, CauseStatus, Citation, Confidence, DiagnoseResult, Gate, GateId, LineRange, ReproMethod, RepoPath, RunId,
    RunKind, RunResult, RunStatus, RunSubject, TicketStatus,
};
use mage_core::ports::blob::{BlobKey, PutOptions};
use mage_core::ports::ArtifactStore;
use uuid::Uuid;

use super::*;
use crate::app::intake::testkit::{Kit, T0, TEAM, sha};
use crate::ports::chat::{ChatAuthor, ChatMessageView};
use crate::ports::runs::{GateWrite, NewArtifact, NewRun, RunWrite};
use crate::ports::ticket::TriageWrite;
use crate::ports::tracker::TrackerAttachment;
use crate::testing::runs::{sample_run, sample_spec};
use crate::testing::{ChatMethod, TrackerMethod};

/// A ticket on issue `n1` linked to a Slack thread.
async fn ticket_with_thread(kit: &Kit) -> Ticket {
    let thread = mage_core::domain::SlackThreadRef { team: TEAM.into(), channel: "C1".into(), ts: "1700000000.000100".into() };
    kit.chat.push_message(&thread, ChatMessageView {
        ts: "1700000000.000100".into(),
        author: ChatAuthor::Human { user_id: "U1".into() },
        text: "export broken".into(),
        at: T0,
        file_names: vec![],
        mage_key: None,
    });
    let mut issue = kit.issue("n1");
    issue.attachments = vec![TrackerAttachment {
        url: Url::parse("https://acme.slack.com/archives/C1/p1700000000000100").unwrap(),
        title: "Slack".into(),
        source_kind: Some("slack".into()),
        metadata: Default::default(),
    }];
    kit.tracker.put_issue(kit.org, issue);
    kit.ingest().run(kit.org, "n1").await.unwrap();
    let t = kit.tickets.get_by_linear_issue(kit.org, "n1").await.unwrap().unwrap().value;
    assert!(t.slack_thread.is_some());
    t
}

fn triage_notice() -> Notice {
    Notice::TriageReady { run_id: RunId::from_uuid(Uuid::from_u128(0x77)) }
}

async fn record(kit: &Kit, ticket: TicketId, target: OutboundTarget) -> Option<crate::ports::outbound::OutboundRecord> {
    kit.outbound.list_for_ticket(kit.org, ticket).await.unwrap().into_iter().find(|r| r.key.target == target)
}

#[tokio::test]
async fn second_run_after_sent_sends_nothing() {
    let kit = Kit::new().await;
    let t = ticket_with_thread(&kit).await;
    let first = kit.notify().run(kit.org, t.id, &triage_notice()).await.unwrap();
    assert!(matches!(&first.targets[..], [(OutboundTarget::LinearComment, Delivered::Sent { .. }), (OutboundTarget::SlackReply, Delivered::Sent { .. })]));
    let again = kit.notify().run(kit.org, t.id, &triage_notice()).await.unwrap();
    assert_eq!(again.targets, vec![(OutboundTarget::LinearComment, Delivered::AlreadyFinal), (OutboundTarget::SlackReply, Delivered::AlreadyFinal)]);
    assert_eq!(kit.tracker.comments().len(), 1);
    assert_eq!(kit.chat.posts().len(), 1);
    // The sidebar attachment is one per (issue, url) and carries the current subtitle.
    let attachments = kit.tracker.attachments();
    assert_eq!(attachments.len(), 1);
    assert_eq!(attachments[0].url.as_str(), format!("https://mage.example/tickets/{}", t.id));
    assert_eq!(attachments[0].subtitle, "Triaged · 0 causes");
}

#[tokio::test]
async fn linear_retry_after_unknown_outcome_reuses_the_client_id() {
    let kit = Kit::new().await;
    let t = ticket_with_thread(&kit).await;
    kit.config(|c| c.notify.slack.clear()).await;
    kit.tracker.lose_next_response(TrackerMethod::Comment);
    let err = kit.notify().run(kit.org, t.id, &triage_notice()).await.unwrap_err();
    assert!(matches!(err, IntakeError::Retry { at: None, .. }), "{err:?}");
    let pending = record(&kit, t.id, OutboundTarget::LinearComment).await.unwrap();
    assert_eq!(pending.state, OutboundState::Pending { uncertain: true });
    assert_eq!(kit.tracker.comments().len(), 1, "the provider committed the comment");

    let out = kit.notify().run(kit.org, t.id, &triage_notice()).await.unwrap();
    assert_eq!(out.targets[0].1, Delivered::Sent { external_id: pending.client_id.to_string(), reconciled: true });
    let comments = kit.tracker.comments();
    assert_eq!(comments.len(), 1);
    assert_eq!(comments[0].id, pending.client_id);
    let sent = record(&kit, t.id, OutboundTarget::LinearComment).await.unwrap();
    assert_eq!((sent.state, sent.client_id, sent.external_id), (OutboundState::Sent, pending.client_id, Some(pending.client_id.to_string())));
}

#[tokio::test]
async fn uncertain_slack_attempt_finds_the_reply_and_does_not_post_again() {
    let kit = Kit::new().await;
    let t = ticket_with_thread(&kit).await;
    kit.chat.lose_next_post_response();
    assert!(kit.notify().run(kit.org, t.id, &triage_notice()).await.is_err());
    assert_eq!(kit.chat.posts().len(), 1);
    assert_eq!(record(&kit, t.id, OutboundTarget::SlackReply).await.unwrap().state, OutboundState::Pending { uncertain: true });

    kit.clock.advance(Duration::from_secs(120));
    let out = kit.notify().run(kit.org, t.id, &triage_notice()).await.unwrap();
    let (_, _, posted, ts) = kit.chat.posts().pop().unwrap();
    assert_eq!(kit.chat.posts().len(), 1, "no second reply");
    assert!(out.targets.contains(&(OutboundTarget::SlackReply, Delivered::Sent { external_id: ts, reconciled: true })));
    assert!(kit.chat.calls().contains(&ChatMethod::FindReply));
    assert_eq!(posted.key, format!("{}:{}", t.id, triage_notice().key()));
}

#[tokio::test]
async fn no_access_skips_and_rate_limit_retries_at_the_deadline() {
    let kit = Kit::new().await;
    let t = ticket_with_thread(&kit).await;
    let retry_at = T0 + time::Duration::minutes(20);
    kit.chat.fail_next(ChatMethod::PostReply, ChatError::NoAccess("not_in_channel".into()));
    kit.tracker.fail_next(TrackerMethod::Comment, TrackerError::RateLimited { retry_at });
    let err = kit.notify().run(kit.org, t.id, &triage_notice()).await.unwrap_err();
    assert_eq!(err, IntakeError::Retry { reason: "Linear rate limited".into(), at: Some(retry_at) });
    let slack = record(&kit, t.id, OutboundTarget::SlackReply).await.unwrap();
    assert_eq!(slack.state, OutboundState::Skipped { reason: "not_in_channel".into() });
    // A rate-limited attempt is not uncertain: no lookup is needed, the retry just sends.
    assert_eq!(record(&kit, t.id, OutboundTarget::LinearComment).await.unwrap().state, OutboundState::Pending { uncertain: false });
    let out = kit.notify().run(kit.org, t.id, &triage_notice()).await.unwrap();
    assert!(matches!(out.targets[0], (OutboundTarget::LinearComment, Delivered::Sent { reconciled: false, .. })));
    assert_eq!(out.targets[1], (OutboundTarget::SlackReply, Delivered::AlreadyFinal));
    assert!(kit.chat.posts().is_empty());
}

#[tokio::test]
async fn targets_follow_config_and_integrations() {
    let kit = Kit::new().await;
    let t = ticket_with_thread(&kit).await;
    // FixBriefReady is off for Slack by default.
    let cause = seed_cause(&kit, &t).await;
    let notice = Notice::FixBriefReady { cause_id: cause.id, version: 1 };
    let out = kit.notify().run(kit.org, t.id, &notice).await.unwrap();
    assert_eq!(out.targets.len(), 1);
    assert_eq!(out.targets[0].0, OutboundTarget::LinearComment);
    // Deleted ticket: nothing to do.
    let gone = kit.notify().run(kit.org, TicketId::from_uuid(Uuid::from_u128(0xdead)), &triage_notice()).await.unwrap();
    assert!(gone.ticket_gone);
    // PR opened + linear_state_on_pr moves the issue.
    kit.config(|c| c.notify.linear_state_on_pr = "In Review".into()).await;
    let pr = Notice::PrOpened { cause_id: cause.id, url: Url::parse("https://github.com/acme/shop/pull/12").unwrap() };
    kit.notify().run(kit.org, t.id, &pr).await.unwrap();
    assert_eq!(kit.tracker.states_set(), vec![(kit.org, "n1".to_owned(), "In Review".to_owned())]);
    assert!(kit.chat.posts().iter().any(|(_, _, p, _)| p.link.as_ref().is_some_and(|(_, label)| label == "PR #12")));
}

async fn seed_cause(kit: &Kit, t: &Ticket) -> Cause {
    let run = RunId::from_uuid(Uuid::from_u128(0x77));
    kit.tickets.begin_triage(kit.org, t.id, &[TicketStatus::New], t.content_hash, run).await.unwrap();
    let target = t.target.clone().unwrap();
    let cause = Cause::rehydrate(CauseData {
        id: CauseId::from_uuid(Uuid::from_u128(0xc1)),
        ticket_id: t.id,
        org_id: kit.org,
        triage_run: run,
        rank: 1,
        title: "Session token *not* refreshed [after](https://evil/x) SSO".into(),
        explanation: "e".into(),
        evidence: vec![Citation {
            repo_id: target.repo_id,
            sha: target.sha,
            path: RepoPath::new("gateway/session.go").unwrap(),
            lines: LineRange::new(118, 141).unwrap(),
            snippet_hash: None,
            note: None,
        }],
        repro_steps: vec![],
        confirm_if: String::new(),
        rule_out_if: String::new(),
        confidence: Confidence::new(0.72).unwrap(),
        method: ReproMethod::Ui,
        status: CauseStatus::Untested,
        superseded_at: None,
    });
    let w = TriageWrite { ticket: t.id, expect: TicketStatus::Triaging, content_hash: t.content_hash, triage_run: run, causes: vec![cause.clone()], next: TicketStatus::Triaged };
    kit.tickets.apply_triage(kit.org, w, &[]).await.unwrap();
    cause
}

#[tokio::test]
async fn triage_comment_lists_causes_with_permalinks_and_escapes_model_text() {
    let kit = Kit::new().await;
    let t = ticket_with_thread(&kit).await;
    seed_cause(&kit, &t).await;
    kit.notify().run(kit.org, t.id, &triage_notice()).await.unwrap();
    let body = kit.tracker.comments()[0].body.clone();
    assert!(body.contains("Session token \\*not\\* refreshed \\[after\\]\\(https://evil/x\\) SSO"), "{body}");
    let head = sha(crate::app::intake::testkit::HEAD).to_hex();
    assert!(body.contains(&format!("](https://github.com/acme/shop/blob/{head}/gateway/session.go#L118-L141)")), "{body}");
    assert!(body.contains("(assumed: default branch head"));
    let (_, _, post, _) = kit.chat.posts().pop().unwrap();
    assert_eq!(post.text, "Mage found 1 likely causes for ENG-2.");
    assert!(!post.text.contains("Session"), "minimal Slack detail carries no model text");
}

/// DiagnosisVerdict with a recorded verify run: the mp4 is uploaded to Linear once (cached for
/// retries) and embedded; links point at Mage, never at signed blob URLs.
#[tokio::test]
async fn verdict_uploads_the_video_once_and_embeds_it() {
    let kit = Kit::new().await;
    let t = ticket_with_thread(&kit).await;
    let cause = seed_cause(&kit, &t).await;
    let budget = Budget { max_wall: Duration::from_secs(600), max_turns: 10, max_cost_micros: 1, max_gate_attempts: 2 };
    let diag_id = RunId::from_uuid(Uuid::from_u128(0xd1));
    let verify_id = RunId::from_uuid(Uuid::from_u128(0xd2));
    let mut diag = sample_run(kit.org, diag_id, RunKind::Diagnose, RunSubject::Cause(cause.id), budget, T0).into_data();
    diag.status = RunStatus::Succeeded;
    diag.result = Some(RunResult::Diagnose(DiagnoseResult {
        verdict: mage_core::domain::Verdict::Reproduced,
        script: None,
        signature: None,
        summary: "s".into(),
        suggested_fix: None,
        fix_sites: vec![],
    }));
    let verify = sample_run(kit.org, verify_id, RunKind::Verify, RunSubject::Run(diag_id), budget, T0);
    let new_run = |run: mage_core::domain::Run, kind| NewRun { run, spec: sample_spec(kind), create_key: None, source_ready: true, affinity: None };
    let gate = GateRecord {
        gate: Gate {
            id: GateId::from_uuid(Uuid::from_u128(0xe1)),
            org_id: kit.org,
            run_id: diag_id,
            kind: GateKind::Repro,
            attempt: 1,
            status: GateStatus::Passed { at: T0 },
            verify_run: Some(verify_id),
        },
        feedback: None,
        details: None,
        created_at: T0,
    };
    kit.runs
        .commit(kit.org, RunWrite {
            inserts: vec![new_run(mage_core::domain::Run::rehydrate(diag), RunKind::Diagnose), new_run(verify, RunKind::Verify)],
            gates: vec![GateWrite::Insert(gate)],
            ..RunWrite::default()
        })
        .await
        .unwrap();
    let mp4 = b"\x00\x00\x00\x18ftypmp42 fake video".to_vec();
    let rec = kit
        .runs
        .reserve_artifact(kit.org, NewArtifact {
            run_id: verify_id,
            attempt: 1,
            kind: ArtifactKind::Video,
            name: "verify-r1.video.mp4".into(),
            bytes: mp4.len() as u64,
            sha256: Sha256::digest(&mp4).into(),
            content_type: "video/mp4".into(),
        })
        .await
        .unwrap();
    kit.artifacts.put(&BlobKey::parse(&rec.artifact.key).unwrap(), mp4.clone(), &PutOptions { content_type: "video/mp4".into(), if_absent: false }).await.unwrap();
    kit.runs.commit(kit.org, RunWrite { complete_artifacts: vec![(rec.artifact.id, mp4.len() as u64)], ..RunWrite::default() }).await.unwrap();

    let notice = Notice::DiagnosisVerdict { cause_id: cause.id, run_id: diag_id };
    kit.tracker.lose_next_response(TrackerMethod::Comment);
    assert!(kit.notify().run(kit.org, t.id, &notice).await.is_err());
    kit.notify().run(kit.org, t.id, &notice).await.unwrap();
    let uploads = kit.tracker.uploads();
    assert_eq!(uploads.len(), 1, "retries reuse the cached upload");
    assert_eq!((uploads[0].1.bytes.clone(), uploads[0].1.content_type.as_str()), (mp4, "video/mp4"));
    let body = kit.tracker.comments()[0].body.clone();
    assert!(body.contains(&format!("({})", uploads[0].2)), "{body}");
    assert!(body.contains("reproduced") && body.contains("Repro gate passed (attempt 1)"));
    assert!(body.contains(&format!("https://mage.example/artifacts/{}", rec.artifact.id)));
    assert_eq!(kit.tracker.attachments()[0].subtitle, "Reproduced · cause #1");
}
