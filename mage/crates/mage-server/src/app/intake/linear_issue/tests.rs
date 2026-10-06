//! IngestLinearIssue table tests (intake.md "Testing"): filters, upstream close, own content,
//! hash/triage decision per status, target resolution order and stickiness, customer
//! resolution, Slack thread folding.

use std::collections::BTreeMap;

use mage_core::domain::{
    Customer, CustomerId, Deployment, DeploymentSource, ExternalRef, JobKind, JobPayload, RunId, SlackThreadRef, TargetOrigin, TicketStatus,
    VersionReason,
};
use mage_core::ports::RepoStore;
use mage_core::ports::common::Versioned;
use mage_core::domain::Ticket;
use time::macros::datetime;
use url::Url;
use uuid::Uuid;

use super::*;
use crate::app::intake::testkit::{HEAD, Kit, MAGE_USER, OTHER, T0, TEAM, sha};
use crate::ports::chat::{ChatAuthor, ChatError, ChatMessageView};
use crate::ports::deployments::DeploymentError;
use crate::ports::outbound::OutboundKey;
use crate::ports::tracker::{Author, TrackerAttachment, TrackerComment};
use crate::testing::{ChatMethod, TrackerMethod};

async fn ticket(kit: &Kit, issue: &str) -> Versioned<Ticket> {
    kit.tickets.get_by_linear_issue(kit.org, issue).await.unwrap().expect("ticket saved")
}

fn deployment(kit: &Kit, env: &str, sha_byte: u8, at: time::OffsetDateTime) -> Deployment {
    Deployment { org_id: kit.org, repo_id: kit.repo.id, environment: env.into(), sha: sha(sha_byte), observed_at: at, source: DeploymentSource::GitHubDeployments }
}

fn human(id: &str, author: &str, body: &str) -> TrackerComment {
    TrackerComment { id: id.into(), author: Author::Human { id: author.into() }, body: body.into(), created_at: T0 - time::Duration::minutes(30) }
}

#[tokio::test]
async fn new_issue_gets_assumed_head_target_pin_index_and_triage() {
    let kit = Kit::new().await;
    kit.tracker.put_issue(kit.org, kit.issue("i1"));
    let out = kit.ingest().run(kit.org, "i1").await.unwrap();
    let t = ticket(&kit, "i1").await.value;
    assert_eq!(out.result, IngestResult::Saved { ticket: t.id, created: true, index: true, triage: true });
    let target = t.target.clone().unwrap();
    assert_eq!((target.repo_id, target.sha, target.origin.clone()), (kit.repo.id, sha(HEAD), TargetOrigin::DefaultHead));
    assert!(target.assumed());
    assert_eq!(t.status, TicketStatus::New);
    assert!(t.body.starts_with("## Description\nExport button"));
    // Version pinned while the ticket is open.
    let v = kit.repos.version(kit.org, mage_core::domain::RepoVersionKey::new(kit.repo.id, sha(HEAD))).await.unwrap();
    assert!(v.reasons.contains(&VersionReason::OnDemand(t.id)));
    let index = kit.queued(JobKind::IndexVersion);
    assert_eq!(index.len(), 1);
    assert_eq!(index[0].payload, JobPayload::IndexVersion { org_id: kit.org, repo_id: kit.repo.id, sha: sha(HEAD), reason: VersionReason::OnDemand(t.id) });
    let triage = kit.queued(JobKind::TriageTicket);
    assert_eq!(triage.len(), 1);
    assert!(matches!(&triage[0].payload, JobPayload::TriageTicket { ticket_id, content_hash, .. } if *ticket_id == t.id && *content_hash == t.content_hash));
}

#[tokio::test]
async fn filters_decide_creation_and_freeze_existing_tickets() {
    let kit = Kit::new().await;
    kit.config(|c| {
        c.linear.require_labels = vec!["bug".into()];
    })
    .await;
    // Wrong team.
    let mut other = kit.issue("t1");
    other.team_key = "OPS".into();
    other.labels = vec!["bug".into()];
    kit.tracker.put_issue(kit.org, other);
    assert_eq!(kit.ingest().run(kit.org, "t1").await.unwrap().result, IngestResult::Ignored("filtered".into()));
    // Missing required label.
    kit.tracker.put_issue(kit.org, kit.issue("t2"));
    assert_eq!(kit.ingest().run(kit.org, "t2").await.unwrap().result, IngestResult::Ignored("filtered".into()));
    // Excluded label wins over a required one.
    let mut excluded = kit.issue("t3");
    excluded.labels = vec!["Bug".into(), "mage:ignore".into()];
    kit.tracker.put_issue(kit.org, excluded);
    assert_eq!(kit.ingest().run(kit.org, "t3").await.unwrap().result, IngestResult::Ignored("filtered".into()));
    assert!(kit.tickets.get_by_linear_issue(kit.org, "t3").await.unwrap().is_none());

    // An accepted issue becomes a ticket; failing the filter later keeps it untouched.
    let mut ok = kit.issue("t4");
    ok.labels = vec!["BUG".into()];
    kit.tracker.put_issue(kit.org, ok);
    kit.ingest().run(kit.org, "t4").await.unwrap();
    let before = ticket(&kit, "t4").await;
    kit.tracker.update_issue(kit.org, "t4", |i| {
        i.labels = vec!["bug".into(), "mage:ignore".into()];
        i.title = "changed".into();
    });
    let out = kit.ingest().run(kit.org, "t4").await.unwrap();
    assert_eq!(out.result, IngestResult::Unchanged(before.value.id));
    assert_eq!(ticket(&kit, "t4").await, before);
}

#[tokio::test]
async fn closed_archived_and_deleted_issues_close_the_ticket() {
    let kit = Kit::new().await;
    // A closed issue never creates a ticket.
    let mut done = kit.issue("c0");
    done.state_kind = TrackerStateKind::Completed;
    kit.tracker.put_issue(kit.org, done);
    assert!(matches!(kit.ingest().run(kit.org, "c0").await.unwrap().result, IngestResult::Ignored(_)));
    assert!(kit.tickets.get_by_linear_issue(kit.org, "c0").await.unwrap().is_none());

    for (id, change) in [("c1", 0u8), ("c2", 1), ("c3", 2)] {
        kit.tracker.put_issue(kit.org, kit.issue(id));
        kit.ingest().run(kit.org, id).await.unwrap();
        match change {
            0 => kit.tracker.update_issue(kit.org, id, |i| i.state_kind = TrackerStateKind::Canceled),
            1 => kit.tracker.update_issue(kit.org, id, |i| i.archived = true),
            _ => kit.tracker.remove_issue(kit.org, id),
        }
        let t = ticket(&kit, id).await.value;
        assert_eq!(kit.ingest().run(kit.org, id).await.unwrap().result, IngestResult::Closed(t.id));
        assert_eq!(ticket(&kit, id).await.value.status, TicketStatus::Closed);
        // Terminal tickets stay as they are.
        assert_eq!(kit.ingest().run(kit.org, id).await.unwrap().result, IngestResult::Unchanged(t.id));
    }
}

#[tokio::test]
async fn own_comments_never_change_the_hash() {
    let kit = Kit::new().await;
    kit.tracker.put_issue(kit.org, kit.issue("o1"));
    kit.ingest().run(kit.org, "o1").await.unwrap();
    // Triage consumed this content (triaged_hash = content_hash), so only a hash change may
    // trigger another triage.
    let first = ticket(&kit, "o1").await.value;
    kit.tickets.begin_triage(kit.org, first.id, &[TicketStatus::New], first.content_hash, RunId::from_uuid(Uuid::from_u128(77))).await.unwrap();
    kit.drain().await;
    let before = ticket(&kit, "o1").await.value;

    // Mage's own comment (identity) and one known to the OutboundLog are dropped.
    let key = OutboundKey { ticket_id: before.id, target: OutboundTarget::LinearComment, notice_key: "triage:x".into() };
    kit.outbound.begin(kit.org, &key, Uuid::from_u128(9), [0; 32]).await.unwrap();
    kit.outbound.mark_sent(kit.org, &key, "c-known", T0).await.unwrap();
    kit.tracker.update_issue(kit.org, "o1", |i| {
        i.comments = vec![human("c-own", MAGE_USER, "**Mage triage** · target ..."), human("c-known", "u-someone", "relayed triage text")];
    });
    assert_eq!(kit.ingest().run(kit.org, "o1").await.unwrap().result, IngestResult::Unchanged(before.id));
    assert_eq!(ticket(&kit, "o1").await.value.content_hash, before.content_hash);
    assert!(kit.queued(JobKind::TriageTicket).is_empty());

    // A human comment changes the body and the hash.
    kit.tracker.update_issue(kit.org, "o1", |i| i.comments.push(human("c-h", "u-1", "Also fails with TypeError: x is undefined")));
    kit.ingest().run(kit.org, "o1").await.unwrap();
    let after = ticket(&kit, "o1").await.value;
    assert_ne!(after.content_hash, before.content_hash);
    assert!(after.body.contains("**u-1 · ") && !after.body.contains("relayed triage text"));
    assert!(after.error_strings.iter().any(|e| e.contains("TypeError")));
}

/// Unchanged content → no triage; changed content triages only in New/Triaging/Triaged/
/// TriageFailed; moving into a triage state with unchanged content triages (via triaged_hash).
#[tokio::test]
async fn triage_decision_follows_hash_status_and_state() {
    let kit = Kit::new().await;
    kit.tracker.put_issue(kit.org, kit.issue("h1"));
    kit.ingest().run(kit.org, "h1").await.unwrap();
    let t = ticket(&kit, "h1").await.value;
    kit.tickets.begin_triage(kit.org, t.id, &[TicketStatus::New], t.content_hash, RunId::from_uuid(Uuid::from_u128(77))).await.unwrap();
    kit.drain().await;
    assert_eq!(kit.ingest().run(kit.org, "h1").await.unwrap().result, IngestResult::Unchanged(t.id));
    assert!(kit.queued(JobKind::TriageTicket).is_empty());

    let statuses = [
        (TicketStatus::New, true),
        (TicketStatus::Triaging, true),
        (TicketStatus::Triaged, true),
        (TicketStatus::TriageFailed, true),
        (TicketStatus::Diagnosing, false),
        (TicketStatus::Diagnosed, false),
        (TicketStatus::Fixing, false),
        (TicketStatus::FixReady, false),
        (TicketStatus::PrOpen, false),
    ];
    for (n, (status, triage)) in statuses.into_iter().enumerate() {
        let current = ticket(&kit, "h1").await.value;
        kit.tickets.transition(kit.org, current.id, current.status, status, &[]).await.unwrap();
        kit.drain().await;
        kit.tracker.update_issue(kit.org, "h1", |i| i.description = format!("edit {n}"));
        let out = kit.ingest().run(kit.org, "h1").await.unwrap();
        assert!(matches!(out.result, IngestResult::Saved { triage: t, .. } if t == triage), "{status:?}: {:?}", out.result);
        assert_eq!(kit.queued(JobKind::TriageTicket).len(), usize::from(triage), "{status:?}");
        let stored = ticket(&kit, "h1").await.value;
        assert_eq!(stored.status, status, "intake never changes the status");
        assert!(stored.body.contains(&format!("edit {n}")));
    }

    // Backlog-only triage: an issue outside the triage states is stored but not triaged...
    let kit = Kit::new().await;
    kit.config(|c| c.linear.triage_states = vec!["triage".into()]).await;
    let mut backlog = kit.issue("h2");
    backlog.state_kind = TrackerStateKind::Backlog;
    kit.tracker.put_issue(kit.org, backlog);
    let out = kit.ingest().run(kit.org, "h2").await.unwrap();
    assert!(matches!(out.result, IngestResult::Saved { triage: false, .. }));
    // ...and moving it into a triage state with unchanged content triggers triage.
    kit.tracker.update_issue(kit.org, "h2", |i| i.state_kind = TrackerStateKind::Triage);
    kit.drain().await;
    let out = kit.ingest().run(kit.org, "h2").await.unwrap();
    assert!(matches!(out.result, IngestResult::Saved { triage: true, index: false, .. }), "{:?}", out.result);
}

#[tokio::test]
async fn target_order_override_directive_deployment_head() {
    let kit = Kit::new().await;
    // Head (assumed) first.
    kit.tracker.put_issue(kit.org, kit.issue("g1"));
    kit.ingest().run(kit.org, "g1").await.unwrap();
    assert_eq!(ticket(&kit, "g1").await.value.target.clone().unwrap().origin, TargetOrigin::DefaultHead);

    // Head moves: the assumed target is sticky.
    kit.repos.set_default_head(kit.org, kit.repo.id, sha(0x33)).await.unwrap();
    kit.tracker.update_issue(kit.org, "g1", |i| i.title = "edited".into());
    kit.ingest().run(kit.org, "g1").await.unwrap();
    assert_eq!(ticket(&kit, "g1").await.value.target.clone().unwrap().sha, sha(HEAD));

    // A deployment appears: upgrade from assumed; as_of = created_at picks the older one.
    let created = kit.issue("g1").created_at;
    kit.deployments.add(deployment(&kit, "production", 0x55, created - time::Duration::days(2)));
    kit.deployments.add(deployment(&kit, "production", 0x66, created + time::Duration::minutes(10)));
    kit.ingest().run(kit.org, "g1").await.unwrap();
    let t = ticket(&kit, "g1").await.value.target.clone().unwrap();
    assert_eq!((t.sha, t.origin), (sha(0x55), TargetOrigin::Deployment { environment: "production".into() }));
    let calls = kit.deployments.calls();
    assert_eq!(calls.last().map(|c| (c.1.as_str(), c.2)), Some(("production", created)));

    // The deployment source has no answer now: no downgrade.
    let kit2 = Kit::new().await;
    kit2.deployments.add(deployment(&kit2, "production", 0x55, T0 - time::Duration::days(3)));
    kit2.tracker.put_issue(kit2.org, kit2.issue("g2"));
    kit2.ingest().run(kit2.org, "g2").await.unwrap();
    let ingest_without = IngestLinearIssue::new(
        kit2.orgs.clone(),
        kit2.tickets.clone(),
        kit2.repos.clone(),
        kit2.tracker.clone(),
        kit2.chat.clone(),
        kit2.outbound.clone(),
        Arc::new(TargetResolver::new(kit2.source.clone(), kit2.repos.clone(), Arc::new(crate::app::intake::target::DeploymentChain::new(vec![])))),
        kit2.identities.clone(),
        kit2.clock.clone(),
        kit2.ids.clone(),
    );
    kit2.tracker.update_issue(kit2.org, "g2", |i| i.title = "edited".into());
    ingest_without.run(kit2.org, "g2").await.unwrap();
    assert_eq!(ticket(&kit2, "g2").await.value.target.clone().unwrap().sha, sha(0x55));

    // A directive beats the deployment; an unresolvable one is ignored.
    kit.tracker.update_issue(kit.org, "g1", |i| i.description = format!("steps\nmage-sha: {}\n", &sha(OTHER).to_hex()[..10]));
    kit.ingest().run(kit.org, "g1").await.unwrap();
    let t = ticket(&kit, "g1").await.value.target.clone().unwrap();
    assert_eq!((t.sha, t.origin), (sha(OTHER), TargetOrigin::Directive));
    kit.tracker.update_issue(kit.org, "g1", |i| i.description = "mage-ref: no-such-branch".into());
    kit.ingest().run(kit.org, "g1").await.unwrap();
    // Directive gone: the deployment (lower rank) does not replace it.
    assert_eq!(ticket(&kit, "g1").await.value.target.clone().unwrap().origin, TargetOrigin::Directive);

    // Override beats everything and is sticky.
    let current = ticket(&kit, "g1").await;
    let mut over = current.value.target.clone().unwrap();
    over.sha = sha(0x33);
    over.origin = TargetOrigin::Override;
    kit.tickets.set_target(kit.org, current.value.id, Some(over.clone()), current.rev, &[]).await.unwrap();
    kit.tracker.update_issue(kit.org, "g1", |i| i.description = format!("mage-sha: {}", sha(OTHER).to_hex()));
    kit.ingest().run(kit.org, "g1").await.unwrap();
    assert_eq!(ticket(&kit, "g1").await.value.target.clone().unwrap(), over);
}

#[tokio::test]
async fn directives_can_be_disabled() {
    let kit = Kit::new().await;
    kit.config(|c| c.linear.directives = false).await;
    let mut issue = kit.issue("d1");
    issue.description = format!("mage-sha: {}", sha(OTHER).to_hex());
    kit.tracker.put_issue(kit.org, issue);
    kit.ingest().run(kit.org, "d1").await.unwrap();
    assert_eq!(ticket(&kit, "d1").await.value.target.clone().unwrap().origin, TargetOrigin::DefaultHead);
}

/// `Unavailable` from the deployment source fails the job and never yields an assumed target.
#[tokio::test]
async fn unknown_deployment_answer_retries_without_saving() {
    let kit = Kit::new().await;
    kit.tracker.put_issue(kit.org, kit.issue("u1"));
    kit.deployments.fail_next(DeploymentError::Unavailable("github down".into()));
    let err = kit.ingest().run(kit.org, "u1").await.unwrap_err();
    assert!(matches!(err, IntakeError::Retry { at: None, .. }), "{err:?}");
    assert!(kit.tickets.get_by_linear_issue(kit.org, "u1").await.unwrap().is_none());
    let retry_at = T0 + time::Duration::minutes(7);
    kit.deployments.fail_next(DeploymentError::RateLimited { retry_at });
    assert_eq!(kit.ingest().run(kit.org, "u1").await.unwrap_err(), IntakeError::Retry { reason: "deployment source rate limited".into(), at: Some(retry_at) });
}

#[tokio::test]
async fn no_route_saves_without_target_or_jobs() {
    let kit = Kit::new().await;
    kit.config(|c| {
        c.routes = vec![crate::domain::intake::RepoRoute {
            when: crate::domain::intake::RouteWhen { team: Some("ENG".into()), label: Some("frontend".into()), project: None },
            repo: "acme/webapp".into(),
        }];
    })
    .await;
    let mut issue = kit.issue("r1");
    issue.labels = vec!["frontend".into()];
    kit.tracker.put_issue(kit.org, issue);
    let out = kit.ingest().run(kit.org, "r1").await.unwrap();
    assert!(matches!(out.result, IngestResult::Saved { index: false, triage: false, .. }));
    assert_eq!(ticket(&kit, "r1").await.value.target, None);
    // No route match: the org's only granted repo.
    kit.tracker.put_issue(kit.org, kit.issue("r2"));
    kit.ingest().run(kit.org, "r2").await.unwrap();
    assert_eq!(ticket(&kit, "r2").await.value.target.clone().unwrap().repo_id, kit.repo.id);
}

#[tokio::test]
async fn customer_resolution_steps_and_ambiguity() {
    let kit = Kit::new().await;
    let customer = |n: u128, refs: Vec<ExternalRef>, env: Option<&str>| Customer {
        id: CustomerId::from_uuid(Uuid::from_u128(n)),
        org_id: kit.org,
        name: format!("c{n}"),
        external_refs: refs,
        environment: env.map(String::from),
    };
    kit.orgs.put_customer(&customer(1, vec![ExternalRef::LinearCustomer("lc-1".into())], Some("prod"))).await.unwrap();
    kit.orgs.put_customer(&customer(2, vec![ExternalRef::LinearLabel("customer:beta".into())], None)).await.unwrap();
    kit.orgs.put_customer(&customer(3, vec![ExternalRef::LinearLabel("customer:gamma".into())], None)).await.unwrap();
    kit.orgs.put_customer(&customer(4, vec![ExternalRef::SlackChannel { team: TEAM.into(), channel: "C42".into() }], None)).await.unwrap();
    kit.config(|c| {
        c.environments.aliases = BTreeMap::from([("prod".into(), "production".into())]);
    })
    .await;
    kit.deployments.add(deployment(&kit, "production", 0x55, T0 - time::Duration::days(1)));

    // Step 1: Linear customer; its environment alias resolves the deployment.
    let mut a = kit.issue("cu1");
    a.customer_ids = vec!["lc-1".into()];
    a.labels = vec!["customer:beta".into()];
    kit.tracker.put_issue(kit.org, a);
    kit.ingest().run(kit.org, "cu1").await.unwrap();
    let t = ticket(&kit, "cu1").await.value;
    assert_eq!(t.customer, Some(CustomerId::from_uuid(Uuid::from_u128(1))));
    assert_eq!(t.target.clone().unwrap().origin, TargetOrigin::Deployment { environment: "production".into() });

    // Step 2: customer label.
    let mut b = kit.issue("cu3");
    b.labels = vec!["customer:beta".into()];
    kit.tracker.put_issue(kit.org, b);
    kit.ingest().run(kit.org, "cu3").await.unwrap();
    assert_eq!(ticket(&kit, "cu3").await.value.customer, Some(CustomerId::from_uuid(Uuid::from_u128(2))));

    // Ambiguous labels stop the search: no customer even though the Slack channel would match.
    let mut c = kit.issue("cu4");
    c.labels = vec!["customer:beta".into(), "customer:gamma".into()];
    c.attachments = vec![slack_attachment("https://acme.slack.com/archives/C42/p1700000000000100")];
    kit.tracker.put_issue(kit.org, c);
    kit.chat.push_message(&thread("C42", "1700000000.000100"), message("U1", "hello"));
    kit.ingest().run(kit.org, "cu4").await.unwrap();
    assert_eq!(ticket(&kit, "cu4").await.value.customer, None);

    // Step 3: shared Slack channel.
    let mut d = kit.issue("cu5");
    d.attachments = vec![slack_attachment("https://acme.slack.com/archives/C42/p1700000000000100")];
    kit.tracker.put_issue(kit.org, d);
    kit.ingest().run(kit.org, "cu5").await.unwrap();
    assert_eq!(ticket(&kit, "cu5").await.value.customer, Some(CustomerId::from_uuid(Uuid::from_u128(4))));

    // No match at all.
    kit.tracker.put_issue(kit.org, kit.issue("cu6"));
    kit.ingest().run(kit.org, "cu6").await.unwrap();
    assert_eq!(ticket(&kit, "cu6").await.value.customer, None);
}

fn slack_attachment(url: &str) -> TrackerAttachment {
    TrackerAttachment { url: Url::parse(url).unwrap(), title: "Slack".into(), source_kind: Some("slack".into()), metadata: BTreeMap::new() }
}

fn thread(channel: &str, ts: &str) -> SlackThreadRef {
    SlackThreadRef { team: TEAM.into(), channel: channel.into(), ts: ts.into() }
}

fn message(user: &str, text: &str) -> ChatMessageView {
    ChatMessageView {
        ts: "1700000000.000200".into(),
        author: ChatAuthor::Human { user_id: user.into() },
        text: text.into(),
        at: datetime!(2023-11-14 22:13:20 UTC),
        file_names: vec!["trace.har".into()],
        mage_key: None,
    }
}

#[tokio::test]
async fn slack_thread_is_folded_and_refreshed_by_interval() {
    let kit = Kit::new().await;
    let th = thread("C9", "1700000000.000100");
    kit.chat.push_message(&th, message("U1", "panic: runtime error: index out of range"));
    kit.chat.push_message(&th, ChatMessageView { author: ChatAuthor::Bot { id: "B1".into(), ours: false }, text: "bot noise".into(), ..message("x", "") });
    let mut issue = kit.issue("s1");
    issue.description = "see https://acme.slack.com/archives/C9/p1700000000000100?thread_ts=1700000000.000100".into();
    kit.tracker.put_issue(kit.org, issue);
    kit.ingest().run(kit.org, "s1").await.unwrap();
    let t = ticket(&kit, "s1").await.value;
    assert_eq!(t.slack_thread, Some(th.clone()));
    assert!(t.body.contains("## Slack thread\n**U1 · 2023-11-14T22:13Z**\npanic: runtime error: index out of range [files: trace.har]"));
    assert!(!t.body.contains("bot noise"));
    assert!(t.error_strings.iter().any(|e| e.contains("index out of range")));
    let snapshot = kit.tickets.get_thread_snapshot(kit.org, t.id).await.unwrap().unwrap();
    assert_eq!((snapshot.messages.len(), snapshot.error.clone()), (1, None));

    // Within the refresh interval the stored snapshot is reused (no Slack call).
    let reads = || kit.chat.calls().iter().filter(|m| **m == ChatMethod::ReadThread).count();
    let before = reads();
    kit.clock.advance(std::time::Duration::from_secs(60));
    kit.ingest().run(kit.org, "s1").await.unwrap();
    assert_eq!(reads(), before);

    // Stale + rate limited: keep the stale text, schedule a re-ingest at retry_at.
    kit.clock.advance(std::time::Duration::from_secs(600));
    let retry_at = kit.clock.now() + time::Duration::minutes(1);
    kit.chat.fail_next(ChatMethod::ReadThread, ChatError::RateLimited { retry_at });
    let out = kit.ingest().run(kit.org, "s1").await.unwrap();
    assert_eq!(out.follow_ups.len(), 1);
    assert_eq!(out.follow_ups[0].run_after, Some(retry_at));
    assert_eq!(out.follow_ups[0].payload, JobPayload::IntakeLinearIssue { org_id: kit.org, issue_id: "s1".into() });
    assert!(ticket(&kit, "s1").await.value.body.contains("index out of range"));

    // Bot removed from the channel: snapshot with error, ingest continues without thread text.
    kit.chat.fail_next(ChatMethod::ReadThread, ChatError::NoAccess("not_in_channel".into()));
    kit.ingest().run(kit.org, "s1").await.unwrap();
    let t = ticket(&kit, "s1").await.value;
    assert!(!t.body.contains("## Slack thread"));
    let snapshot = kit.tickets.get_thread_snapshot(kit.org, t.id).await.unwrap().unwrap();
    assert_eq!(snapshot.error.as_deref(), Some("not_in_channel"));
}

#[tokio::test]
async fn rejected_key_twice_revokes_the_integration() {
    let kit = Kit::new().await;
    kit.tracker.put_issue(kit.org, kit.issue("k1"));
    // Rejected once, identity still fine: transient retry.
    kit.tracker.fail_next(TrackerMethod::GetIssue, TrackerError::Unauthorized);
    assert!(matches!(kit.ingest().run(kit.org, "k1").await.unwrap_err(), IntakeError::Retry { .. }));
    // Rejected and confirmed by identity: revoked, permanent.
    kit.tracker.fail_next(TrackerMethod::GetIssue, TrackerError::Unauthorized);
    kit.tracker.fail_next(TrackerMethod::Identity, TrackerError::Unauthorized);
    assert!(matches!(kit.ingest().run(kit.org, "k1").await.unwrap_err(), IntakeError::Permanent(_)));
    let linear = kit.orgs.integrations(kit.org).await.unwrap().into_iter().find(|i| i.kind == mage_core::domain::IntegrationKind::Linear).unwrap();
    assert_eq!(linear.status, mage_core::domain::IntegrationStatus::Revoked);
    assert_eq!(kit.ingest().run(kit.org, "k1").await.unwrap().result, IngestResult::Ignored("linear integration not active".into()));
}

#[test]
fn slack_links_parse_to_thread_roots() {
    let l = parse_slack_link("https://acme.slack.com/archives/C0123ABC/p1700000000000100").unwrap();
    assert_eq!((l.team_domain.as_str(), l.channel.as_str(), l.thread_ts.as_str()), ("acme", "C0123ABC", "1700000000.000100"));
    let l = parse_slack_link("https://acme.slack.com/archives/C1/p1700000000000200?thread_ts=1700000000.000100&cid=C1").unwrap();
    assert_eq!(l.thread_ts, "1700000000.000100");
    for bad in ["http://acme.slack.com/archives/C1/p1700000000000100", "https://evil.com/archives/C1/p1700000000000100", "https://a.b.slack.com/archives/C1/p17", "https://acme.slack.com/messages/C1/p1700000000000100"] {
        assert_eq!(parse_slack_link(bad), None, "{bad}");
    }
}
