//! `TicketStore` contract (storage.md "`TicketStore`" and "Testing"; triage.md begin / apply /
//! fail triage; fix.md briefs and pull requests; knowledge.md `similar_tickets`).
//!
//! Seeding order for the pg foreign keys: org, repos, customers and users first; a ticket is
//! saved before its Triage run is seeded (`causes.triage_run`); Diagnose / Fix runs, gates and
//! artifacts are seeded once their cause exists.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use mage_core::domain::fix::{BranchName, ChangeSetDigest, FixPrState, FixPullRequest};
use mage_core::domain::{
    ArtifactId, Cause, CauseData, CauseId, CauseStatus, Citation, CommitSha, Confidence, ContentHash, CustomerId, FixBrief, FixBriefId,
    FixPullRequestId, GateId, JobKind, JobPayload, LineRange, OrgId, RepoId, RepoPath, ReproMethod, Resolution, RunId, SlackThreadRef,
    TargetOrigin, Ticket, TicketContent, TicketData, TicketId, TicketSource, TicketStatus, TicketTarget, UserId,
};
use mage_core::ports::clock::Clock;
use mage_core::ports::common::{PageRequest, Versioned};
use mage_core::ports::jobs::{JobQueue, LeaseRequest, NewJob};
use mage_core::testing::{FakeClock, join_all};
use time::OffsetDateTime;
use url::Url;
use uuid::Uuid;

use crate::domain::intake::{ThreadMessage, ThreadSnapshot};
use crate::ports::feed::{ChangeFeed, FeedId, FeedKind};
use crate::ports::ticket::{SimilarMatch, SimilarQuery, TicketFilter, TicketStore, TicketStoreError, TicketTransition, TriageWrite};
use crate::testing::seed::{Seed, SeedRun};

/// A fresh, empty `TicketStore` with the job queue and feed it writes to.
pub struct TicketFixture {
    pub tickets: Arc<dyn TicketStore>,
    pub jobs: Arc<dyn JobQueue>,
    pub feed: Arc<dyn ChangeFeed>,
    pub seed: Arc<dyn Seed>,
}

/// Runs every `TicketStore` scenario, each against a fresh fixture from `make`.
pub async fn contract_ticket_store<F, Fut>(make: F)
where
    F: Fn(Arc<FakeClock>) -> Fut,
    Fut: Future<Output = TicketFixture>,
{
    macro_rules! scenario {
        ($name:ident) => {{
            let clock = Arc::new(FakeClock::new());
            let fixture = make(clock.clone()).await;
            $name(&clock, &fixture).await;
        }};
    }
    scenario!(tickets_insert_update_and_look_up);
    scenario!(list_tickets_filters_and_pages);
    scenario!(transition_and_set_target_cas);
    scenario!(thread_snapshots_round_trip);
    scenario!(triage_is_fenced_and_supersedes_only_untested_causes);
    scenario!(cause_status_variants_and_ticket_transition);
    scenario!(fix_brief_versions_under_concurrency);
    scenario!(pull_requests_upsert_progress_and_lookup);
    scenario!(resolve_writes_resolution_and_ticket_once);
    scenario!(similar_resolved_filters_matches_and_ranks);
}

// ---- ids and builders ----

fn org(n: u128) -> OrgId {
    OrgId::from_uuid(Uuid::from_u128(0x0a00 + n))
}
fn repo(n: u128) -> RepoId {
    RepoId::from_uuid(Uuid::from_u128(0x0b00 + n))
}
fn tid(n: u128) -> TicketId {
    TicketId::from_uuid(Uuid::from_u128(0x0c00 + n))
}
fn cid(n: u128) -> CauseId {
    CauseId::from_uuid(Uuid::from_u128(0x0d00 + n))
}
fn run(n: u128) -> RunId {
    RunId::from_uuid(Uuid::from_u128(0x0e00 + n))
}
fn gate(n: u128) -> GateId {
    GateId::from_uuid(Uuid::from_u128(0x0f00 + n))
}
fn artifact(n: u128) -> ArtifactId {
    ArtifactId::from_uuid(Uuid::from_u128(0x1000 + n))
}
fn user(n: u128) -> UserId {
    UserId::from_uuid(Uuid::from_u128(0x1100 + n))
}
fn customer(n: u128) -> CustomerId {
    CustomerId::from_uuid(Uuid::from_u128(0x1200 + n))
}
fn pr_id(n: u128) -> FixPullRequestId {
    FixPullRequestId::from_uuid(Uuid::from_u128(0x1300 + n))
}
fn brief_id(n: u128) -> FixBriefId {
    FixBriefId::from_uuid(Uuid::from_u128(0x1400 + n))
}
fn sha(n: u8) -> CommitSha {
    CommitSha::from_bytes([n; 20])
}
fn hash(n: u8) -> ContentHash {
    ContentHash::from_bytes([n; 32])
}
fn url(s: &str) -> Url {
    Url::parse(s).unwrap()
}
fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

fn ticket(org: OrgId, id: TicketId, issue: &str, title: &str, body: &str, at: OffsetDateTime) -> Ticket {
    let source = TicketSource::Linear {
        issue_id: issue.into(),
        identifier: format!("ENG-{issue}"),
        url: url(&format!("https://linear.app/acme/issue/{issue}")),
    };
    let content = TicketContent {
        title: title.into(),
        body: body.into(),
        content_hash: hash(1),
        slack_thread: None,
        customer: None,
        target: None,
        error_strings: Vec::new(),
    };
    Ticket::new(id, org, source, content, at)
}

fn edit(t: &Ticket, f: impl FnOnce(&mut TicketData)) -> Ticket {
    let mut data = t.clone().into_data();
    f(&mut data);
    Ticket::rehydrate(data)
}

fn target(repo: RepoId, n: u8, origin: TargetOrigin) -> Option<TicketTarget> {
    Some(TicketTarget { repo_id: repo, sha: sha(n), origin })
}

fn citation(repo: RepoId, n: u8, note: Option<&str>, snippet: Option<[u8; 32]>) -> Citation {
    Citation {
        repo_id: repo,
        sha: sha(n),
        path: RepoPath::new(format!("src/mod{n}.rs")).unwrap(),
        lines: LineRange::new(3, 7 + u32::from(n)).unwrap(),
        snippet_hash: snippet,
        note: note.map(str::to_owned),
    }
}

/// An `Untested` cause of `t` from `triage_run` with one citation in `repo`; the confidence is
/// exactly representable in `f32` (the `real` column).
fn cause(t: &Ticket, id: CauseId, triage_run: RunId, rank: u16, repo: RepoId) -> Cause {
    Cause::rehydrate(CauseData {
        id,
        ticket_id: t.id,
        org_id: t.org_id,
        triage_run,
        rank,
        title: format!("cause {rank}"),
        explanation: format!("explanation of cause {rank}"),
        evidence: vec![citation(repo, rank as u8, None, None)],
        repro_steps: vec!["open the checkout page".into(), "press pay".into()],
        confirm_if: "the request returns 500".into(),
        rule_out_if: "the request returns 200".into(),
        confidence: Confidence::new(0.5).unwrap(),
        method: ReproMethod::Ui,
        status: CauseStatus::Untested,
        superseded_at: None,
    })
}

fn with_status(c: &Cause, status: CauseStatus) -> Cause {
    let mut data = c.clone().into_data();
    data.status = status;
    Cause::rehydrate(data)
}

fn superseded(c: &Cause, at: OffsetDateTime) -> Cause {
    let mut data = c.clone().into_data();
    data.superseded_at = Some(at);
    Cause::rehydrate(data)
}

/// Outbox job tagged `tag` (tags are unique per scenario so dedupe never merges two).
fn job(org: OrgId, tag: &str) -> NewJob {
    NewJob::new(JobPayload::IntakeLinearIssue { org_id: org, issue_id: tag.into() })
}

/// A job whose kind does not match its payload: every enqueue path rejects it.
fn bad_job(org: OrgId) -> NewJob {
    NewJob { kind: JobKind::IndexGc, ..job(org, "bad") }
}

/// Tags of the outbox jobs enqueued since the last call (sorted).
async fn drain_jobs(jobs: &dyn JobQueue) -> Vec<String> {
    let request = LeaseRequest {
        lane: JobKind::IntakeLinearIssue.lane(),
        kinds: Some(vec![JobKind::IntakeLinearIssue]),
        worker: "contract".into(),
        max: 100,
    };
    let mut tags: Vec<String> = jobs
        .lease(&request)
        .await
        .expect("lease")
        .into_iter()
        .map(|l| match l.payload {
            JobPayload::IntakeLinearIssue { issue_id, .. } => issue_id,
            other => panic!("unexpected job {other:?}"),
        })
        .collect();
    tags.sort();
    tags
}

/// Feed entries of `org` after `cursor` (advanced past them).
async fn drain_feed(feed: &dyn ChangeFeed, org: OrgId, cursor: &mut FeedId) -> Vec<(FeedKind, String)> {
    let entries = feed.after(org, *cursor, 100).await.expect("feed after");
    if let Some(last) = entries.last() {
        *cursor = last.id;
    }
    entries
        .into_iter()
        .map(|e| (e.kind, e.subject))
        .collect()
}

fn ticket_entry(t: TicketId) -> (FeedKind, String) {
    (FeedKind::Ticket, t.to_string())
}

fn cause_entry(c: CauseId) -> (FeedKind, String) {
    (FeedKind::Cause, c.to_string())
}

/// Seeds the Triage run of the saved ticket `t` and triages it into `causes`.
async fn triage(fx: &TicketFixture, t: &Ticket, triage_run: RunId, causes: Vec<Cause>) -> Ticket {
    let org = t.org_id;
    fx.seed.run(org, triage_run, SeedRun::Triage { ticket: t.id }).await;
    let current = fx.tickets.get_ticket(org, t.id).await.expect("get_ticket").value;
    fx.tickets.begin_triage(org, t.id, &[current.status], current.content_hash, triage_run).await.expect("begin_triage");
    let w = TriageWrite {
        ticket: t.id,
        expect: TicketStatus::Triaging,
        content_hash: current.content_hash,
        triage_run,
        causes,
        next: TicketStatus::Triaged,
    };
    fx.tickets.apply_triage(org, w, &[]).await.expect("apply_triage")
}

// ---- scenarios ----

async fn tickets_insert_update_and_look_up(clock: &FakeClock, fx: &TicketFixture) {
    let (a, b) = (org(1), org(2));
    fx.seed.org(a).await;
    fx.seed.org(b).await;
    fx.seed.repo(a, repo(1)).await;
    fx.seed.customer(a, customer(1)).await;
    let mut feed_a = FeedId(0);

    let t1 = edit(&ticket(a, tid(1), "iss-1", "Checkout fails", "Pay button errors", clock.now()), |d| {
        d.slack_thread = Some(SlackThreadRef { team: "T1".into(), channel: "C1".into(), ts: "1700000000.000100".into() });
        d.customer = Some(customer(1));
        d.target = target(repo(1), 1, TargetOrigin::Deployment { environment: "prod".into() });
        d.error_strings = vec!["TypeError: x is undefined".into(), "ECONNRESET".into()];
    });
    assert_eq!(fx.tickets.save_ingested(a, &t1, None, &[job(a, "save-1")]).await, Ok(1));
    let entries = fx.feed.after(a, FeedId(0), 10).await.unwrap();
    assert_eq!(entries.iter().map(|e| e.at).collect::<Vec<_>>(), [clock.now()], "feed entries carry the store clock's time");
    assert_eq!(fx.tickets.get_ticket(a, tid(1)).await, Ok(Versioned { value: t1.clone(), rev: 1 }), "every field round-trips");
    assert_eq!(drain_jobs(&*fx.jobs).await, ["save-1"]);
    assert_eq!(drain_feed(&*fx.feed, a, &mut feed_a).await, [ticket_entry(tid(1))]);

    let same_id = edit(&t1, |d| d.source = TicketSource::Linear { issue_id: "iss-9".into(), identifier: "ENG-9".into(), url: url("https://linear.app/x") });
    assert_eq!(fx.tickets.save_ingested(a, &same_id, None, &[]).await, Err(TicketStoreError::Conflict), "duplicate ticket id");
    let same_issue = ticket(a, tid(2), "iss-1", "Other", "", clock.now());
    assert_eq!(fx.tickets.save_ingested(a, &same_issue, None, &[]).await, Err(TicketStoreError::Conflict), "duplicate Linear issue");
    let foreign = ticket(b, tid(3), "iss-3", "Foreign", "", clock.now());
    assert!(matches!(fx.tickets.save_ingested(a, &foreign, None, &[]).await, Err(TicketStoreError::Invalid(_))), "ticket of another org");
    let t4 = ticket(a, tid(4), "iss-4", "Invalid job", "", clock.now());
    assert!(matches!(fx.tickets.save_ingested(a, &t4, None, &[job(a, "never"), bad_job(a)]).await, Err(TicketStoreError::Invalid(_))));
    assert_eq!(fx.tickets.get_ticket(a, tid(4)).await, Err(TicketStoreError::NotFound), "an invalid outbox job aborts the insert");

    assert_eq!(fx.tickets.get_by_linear_issue(a, "iss-1").await, Ok(Some(Versioned { value: t1.clone(), rev: 1 })));
    assert_eq!(fx.tickets.get_by_linear_issue(a, "iss-2").await, Ok(None));
    assert_eq!(fx.tickets.get_by_linear_issue(b, "iss-1").await, Ok(None), "tenant scoped");
    assert_eq!(fx.tickets.get_ticket(b, tid(1)).await, Err(TicketStoreError::NotFound), "tenant scoped");

    clock.advance(secs(10));
    let t1b = edit(&t1, |d| {
        d.title = "Checkout fails on Safari".into();
        d.content_hash = hash(2);
        d.status = TicketStatus::Triaged;
        d.triaged_hash = Some(hash(1));
        d.triage_run = Some(run(1));
        d.slack_thread = None;
        d.customer = None;
        d.target = target(repo(1), 2, TargetOrigin::DefaultHead);
        d.error_strings = Vec::new();
        d.updated_at = clock.now();
    });
    assert_eq!(fx.tickets.save_ingested(a, &t1b, Some(1), &[job(a, "save-2")]).await, Ok(2));
    assert_eq!(fx.tickets.get_ticket(a, tid(1)).await, Ok(Versioned { value: t1b.clone(), rev: 2 }), "the update writes the ticket as given");
    assert_eq!(drain_jobs(&*fx.jobs).await, ["save-2"]);
    assert_eq!(drain_feed(&*fx.feed, a, &mut feed_a).await, [ticket_entry(tid(1))]);
    assert_eq!(fx.tickets.save_ingested(a, &t1, Some(1), &[]).await, Err(TicketStoreError::Conflict), "stale rev");
    let missing = ticket(a, tid(5), "iss-5", "Missing", "", clock.now());
    assert_eq!(fx.tickets.save_ingested(a, &missing, Some(1), &[]).await, Err(TicketStoreError::NotFound));
    let t1_in_b = edit(&t1b, |d| d.org_id = b);
    assert_eq!(fx.tickets.save_ingested(b, &t1_in_b, Some(2), &[]).await, Err(TicketStoreError::NotFound), "org B cannot update org A's ticket");
    assert!(matches!(fx.tickets.save_ingested(a, &t1b, Some(2), &[bad_job(a)]).await, Err(TicketStoreError::Invalid(_))));

    let t2 = ticket(a, tid(2), "iss-2", "Second", "", clock.now());
    assert_eq!(fx.tickets.save_ingested(a, &t2, None, &[]).await, Ok(1));
    let steal = edit(&t1b, |d| d.source = t2.source.clone());
    assert_eq!(fx.tickets.save_ingested(a, &steal, Some(2), &[]).await, Err(TicketStoreError::Conflict), "an update cannot take another ticket's issue");
    assert_eq!(fx.tickets.get_ticket(a, tid(1)).await, Ok(Versioned { value: t1b, rev: 2 }), "failed updates change nothing");
    assert_eq!(drain_jobs(&*fx.jobs).await, Vec::<String>::new());
    assert_eq!(drain_feed(&*fx.feed, a, &mut feed_a).await, [ticket_entry(tid(2))], "only the successful insert is in the feed");
    assert_eq!(drain_feed(&*fx.feed, b, &mut FeedId(0)).await, Vec::new());
}

async fn list_tickets_filters_and_pages(clock: &FakeClock, fx: &TicketFixture) {
    let (a, b) = (org(1), org(2));
    fx.seed.org(a).await;
    fx.seed.org(b).await;
    fx.seed.repo(a, repo(1)).await;
    fx.seed.repo(a, repo(2)).await;
    fx.seed.customer(a, customer(1)).await;
    let now = clock.now();
    let tickets = [
        edit(&ticket(a, tid(1), "i1", "Checkout button broken", "clicking pay does nothing", now), |d| {
            d.customer = Some(customer(1));
            d.target = target(repo(1), 1, TargetOrigin::DefaultHead);
        }),
        edit(&ticket(a, tid(2), "i2", "Login fails", "password reset email never arrives", now), |d| {
            d.status = TicketStatus::Triaged;
            d.target = target(repo(2), 1, TargetOrigin::DefaultHead);
        }),
        edit(&ticket(a, tid(3), "i3", "Checkout total wrong", "tax rounding", now), |d| {
            d.customer = Some(customer(1));
            d.target = target(repo(1), 1, TargetOrigin::Directive);
        }),
        edit(&ticket(a, tid(4), "i4", "Export CSV", "checkout export missing", now), |d| {
            d.status = TicketStatus::Resolved;
            d.target = target(repo(1), 1, TargetOrigin::DefaultHead);
        }),
        edit(&ticket(a, tid(5), "i5", "Dark mode", "colors off", now), |d| d.status = TicketStatus::Closed),
        ticket(b, tid(6), "i6", "Checkout in org B", "checkout", now),
    ];
    for t in &tickets {
        fx.tickets.save_ingested(t.org_id, t, None, &[]).await.unwrap();
    }
    let ids = |page: Vec<Ticket>| page.into_iter().map(|t| t.id).collect::<Vec<_>>();
    let list = |filter: TicketFilter| async move { ids(fx.tickets.list_tickets(a, &filter, PageRequest::first(50)).await.unwrap().items) };

    let mut all = Vec::new();
    let mut page = PageRequest::first(2);
    let mut pages = 0;
    loop {
        let got = fx.tickets.list_tickets(a, &TicketFilter::default(), page.clone()).await.unwrap();
        pages += 1;
        assert!(got.items.len() <= 2);
        all.extend(got.items.clone());
        match got.next {
            Some(cursor) => page = page.after(cursor),
            None => break,
        }
    }
    assert_eq!(pages, 3);
    assert_eq!(all, tickets[..5].iter().rev().cloned().collect::<Vec<_>>(), "pages concatenate to the id-descending list");

    assert_eq!(list(TicketFilter { status: Some(vec![TicketStatus::New]), ..Default::default() }).await, [tid(3), tid(1)]);
    assert_eq!(list(TicketFilter { status: Some(vec![TicketStatus::Resolved, TicketStatus::Closed]), ..Default::default() }).await, [tid(5), tid(4)]);
    assert_eq!(list(TicketFilter { status: Some(vec![]), ..Default::default() }).await, Vec::<TicketId>::new());
    assert_eq!(list(TicketFilter { customer: Some(customer(1)), ..Default::default() }).await, [tid(3), tid(1)]);
    assert_eq!(list(TicketFilter { repo: Some(repo(1)), ..Default::default() }).await, [tid(4), tid(3), tid(1)]);
    assert_eq!(list(TicketFilter { text: Some("checkout".into()), ..Default::default() }).await, [tid(4), tid(3), tid(1)], "title or body");
    assert_eq!(list(TicketFilter { text: Some("Checkout total".into()), ..Default::default() }).await, [tid(3)], "every word must match");
    assert_eq!(list(TicketFilter { text: Some("zebra".into()), ..Default::default() }).await, Vec::<TicketId>::new());
    let combined = TicketFilter { status: Some(vec![TicketStatus::New]), text: Some("checkout".into()), repo: Some(repo(1)), customer: Some(customer(1)) };
    assert_eq!(list(combined).await, [tid(3), tid(1)]);

    let org_b = fx.tickets.list_tickets(b, &TicketFilter::default(), PageRequest::first(50)).await.unwrap();
    assert_eq!(ids(org_b.items), [tid(6)], "tenant scoped");
    let bad = PageRequest::first(2).after(mage_core::ports::common::Cursor("not-a-cursor".into()));
    assert!(matches!(fx.tickets.list_tickets(a, &TicketFilter::default(), bad).await, Err(TicketStoreError::Invalid(_))));

    assert_eq!(fx.tickets.list_open_assumed(a, repo(1)).await, Ok(vec![tid(1)]), "assumed, non-terminal, of the repo");
    assert_eq!(fx.tickets.list_open_assumed(a, repo(2)).await, Ok(vec![tid(2)]));
    assert_eq!(fx.tickets.list_open_assumed(b, repo(1)).await, Ok(vec![]), "tenant scoped");
}

async fn transition_and_set_target_cas(clock: &FakeClock, fx: &TicketFixture) {
    let (a, b) = (org(1), org(2));
    fx.seed.org(a).await;
    fx.seed.org(b).await;
    fx.seed.repo(a, repo(1)).await;
    let t1 = ticket(a, tid(1), "iss-1", "Checkout", "", clock.now());
    fx.tickets.save_ingested(a, &t1, None, &[]).await.unwrap();
    let mut feed_a = FeedId(0);
    drain_feed(&*fx.feed, a, &mut feed_a).await;

    clock.advance(secs(5));
    let triaged = edit(&t1, |d| {
        d.status = TicketStatus::Triaged;
        d.updated_at = clock.now();
    });
    assert_eq!(fx.tickets.transition(a, tid(1), TicketStatus::New, TicketStatus::Triaged, &[job(a, "tr-1")]).await, Ok(triaged.clone()));
    assert_eq!(fx.tickets.get_ticket(a, tid(1)).await, Ok(Versioned { value: triaged.clone(), rev: 2 }));
    assert_eq!(drain_jobs(&*fx.jobs).await, ["tr-1"]);
    assert_eq!(drain_feed(&*fx.feed, a, &mut feed_a).await, [ticket_entry(tid(1))]);
    assert_eq!(fx.tickets.transition(a, tid(1), TicketStatus::New, TicketStatus::Closed, &[]).await, Err(TicketStoreError::Conflict));
    assert_eq!(fx.tickets.transition(a, tid(9), TicketStatus::New, TicketStatus::Closed, &[]).await, Err(TicketStoreError::NotFound));
    assert_eq!(fx.tickets.transition(b, tid(1), TicketStatus::Triaged, TicketStatus::Closed, &[]).await, Err(TicketStoreError::NotFound));
    assert!(matches!(
        fx.tickets.transition(a, tid(1), TicketStatus::Triaged, TicketStatus::Closed, &[bad_job(a)]).await,
        Err(TicketStoreError::Invalid(_))
    ));
    assert_eq!(fx.tickets.get_ticket(a, tid(1)).await, Ok(Versioned { value: triaged.clone(), rev: 2 }), "misses change nothing");

    clock.advance(secs(5));
    let deployed = target(repo(1), 7, TargetOrigin::Deployment { environment: "staging".into() });
    assert_eq!(fx.tickets.set_target(a, tid(1), deployed.clone(), 2, &[job(a, "tg-1")]).await, Ok(3));
    let targeted = edit(&triaged, |d| {
        d.target = deployed.clone();
        d.updated_at = clock.now();
    });
    assert_eq!(fx.tickets.get_ticket(a, tid(1)).await, Ok(Versioned { value: targeted.clone(), rev: 3 }));
    assert_eq!(drain_jobs(&*fx.jobs).await, ["tg-1"]);
    assert_eq!(drain_feed(&*fx.feed, a, &mut feed_a).await, [ticket_entry(tid(1))]);
    assert_eq!(fx.tickets.set_target(a, tid(1), None, 2, &[]).await, Err(TicketStoreError::Conflict), "stale rev");
    assert_eq!(fx.tickets.set_target(a, tid(9), None, 1, &[]).await, Err(TicketStoreError::NotFound));
    assert_eq!(fx.tickets.set_target(b, tid(1), None, 3, &[]).await, Err(TicketStoreError::NotFound));
    assert!(matches!(fx.tickets.set_target(a, tid(1), None, 3, &[bad_job(a)]).await, Err(TicketStoreError::Invalid(_))));
    assert_eq!(fx.tickets.get_ticket(a, tid(1)).await, Ok(Versioned { value: targeted.clone(), rev: 3 }));

    clock.advance(secs(5));
    assert_eq!(fx.tickets.set_target(a, tid(1), None, 3, &[]).await, Ok(4));
    let cleared = edit(&targeted, |d| {
        d.target = None;
        d.updated_at = clock.now();
    });
    assert_eq!(fx.tickets.get_ticket(a, tid(1)).await, Ok(Versioned { value: cleared, rev: 4 }));
    assert_eq!(drain_jobs(&*fx.jobs).await, Vec::<String>::new());
    assert_eq!(drain_feed(&*fx.feed, a, &mut feed_a).await, [ticket_entry(tid(1))]);
}

async fn thread_snapshots_round_trip(clock: &FakeClock, fx: &TicketFixture) {
    let (a, b) = (org(1), org(2));
    fx.seed.org(a).await;
    fx.seed.org(b).await;
    let t1 = ticket(a, tid(1), "iss-1", "Checkout", "", clock.now());
    fx.tickets.save_ingested(a, &t1, None, &[]).await.unwrap();
    let mut feed_a = FeedId(0);
    drain_feed(&*fx.feed, a, &mut feed_a).await;
    let thread = SlackThreadRef { team: "T1".into(), channel: "C1".into(), ts: "1700000000.000100".into() };
    let first = ThreadSnapshot {
        thread: thread.clone(),
        fetched_at: clock.now(),
        messages: vec![ThreadMessage {
            ts: "1700000000.000200".into(),
            author_id: "U1".into(),
            text: "still broken".into(),
            file_names: vec!["screen.png".into()],
        }],
        error: None,
    };
    assert_eq!(fx.tickets.get_thread_snapshot(a, tid(1)).await, Ok(None));
    assert_eq!(fx.tickets.put_thread_snapshot(a, tid(1), &first).await, Ok(()));
    assert_eq!(fx.tickets.get_thread_snapshot(a, tid(1)).await, Ok(Some(first.clone())));
    assert_eq!(drain_feed(&*fx.feed, a, &mut feed_a).await, [ticket_entry(tid(1))]);

    clock.advance(secs(60));
    let second = ThreadSnapshot { thread, fetched_at: clock.now(), messages: Vec::new(), error: Some("not_in_channel".into()) };
    assert_eq!(fx.tickets.put_thread_snapshot(a, tid(1), &second).await, Ok(()));
    assert_eq!(fx.tickets.get_thread_snapshot(a, tid(1)).await, Ok(Some(second.clone())), "a put replaces the snapshot");
    assert_eq!(fx.tickets.get_ticket(a, tid(1)).await.map(|v| v.rev), Ok(1), "snapshots do not bump the ticket rev");
    assert_eq!(drain_feed(&*fx.feed, a, &mut feed_a).await, [ticket_entry(tid(1))]);

    assert_eq!(fx.tickets.put_thread_snapshot(a, tid(9), &second).await, Err(TicketStoreError::NotFound));
    assert_eq!(fx.tickets.put_thread_snapshot(b, tid(1), &first).await, Err(TicketStoreError::NotFound), "tenant scoped");
    assert_eq!(fx.tickets.get_thread_snapshot(b, tid(1)).await, Ok(None), "tenant scoped");
    assert_eq!(fx.tickets.get_thread_snapshot(a, tid(1)).await, Ok(Some(second)));
}

async fn triage_is_fenced_and_supersedes_only_untested_causes(clock: &FakeClock, fx: &TicketFixture) {
    let (a, b) = (org(1), org(2));
    fx.seed.org(a).await;
    fx.seed.org(b).await;
    fx.seed.repo(a, repo(1)).await;
    let t1 = ticket(a, tid(1), "iss-1", "Checkout", "", clock.now());
    fx.tickets.save_ingested(a, &t1, None, &[]).await.unwrap();
    for n in 1..=6 {
        fx.seed.run(a, run(n), SeedRun::Triage { ticket: tid(1) }).await;
    }
    let mut feed_a = FeedId(0);
    drain_feed(&*fx.feed, a, &mut feed_a).await;

    // begin_triage: CAS on the status set and the content hash.
    clock.advance(secs(1));
    let begin = |expect: &'static [TicketStatus], h: ContentHash, r: RunId| fx.tickets.begin_triage(a, tid(1), expect, h, r);
    assert_eq!(begin(&[TicketStatus::Triaged], hash(1), run(1)).await, Err(TicketStoreError::Conflict), "status not in expect");
    assert_eq!(begin(&[TicketStatus::New], hash(2), run(1)).await, Err(TicketStoreError::Conflict), "content changed");
    assert_eq!(fx.tickets.begin_triage(a, tid(9), &[TicketStatus::New], hash(1), run(1)).await, Err(TicketStoreError::NotFound));
    assert_eq!(fx.tickets.begin_triage(b, tid(1), &[TicketStatus::New], hash(1), run(1)).await, Err(TicketStoreError::NotFound));
    let triaging = edit(&t1, |d| {
        d.status = TicketStatus::Triaging;
        d.triaged_hash = Some(hash(1));
        d.triage_run = Some(run(1));
        d.updated_at = clock.now();
    });
    assert_eq!(begin(&[TicketStatus::New, TicketStatus::TriageFailed], hash(1), run(1)).await, Ok(triaging.clone()));
    assert_eq!(fx.tickets.get_ticket(a, tid(1)).await, Ok(Versioned { value: triaging.clone(), rev: 2 }));
    assert_eq!(drain_feed(&*fx.feed, a, &mut feed_a).await, [ticket_entry(tid(1))]);

    // apply_triage: fenced on status, triage_run and content hash.
    let c1 = {
        let mut data = cause(&t1, cid(1), run(1), 1, repo(1)).into_data();
        data.evidence = vec![
            citation(repo(1), 1, Some("the handler drops the error"), Some([7; 32])),
            citation(repo(1), 2, None, None),
        ];
        data.confidence = Confidence::new(0.75).unwrap();
        data.method = ReproMethod::Api;
        Cause::rehydrate(data)
    };
    let c2 = {
        let mut data = cause(&t1, cid(2), run(1), 2, repo(1)).into_data();
        data.confidence = Confidence::new(0.25).unwrap();
        data.method = ReproMethod::Unknown;
        data.repro_steps = Vec::new();
        Cause::rehydrate(data)
    };
    let write = |r: RunId, h: ContentHash, expect: TicketStatus, causes: Vec<Cause>| TriageWrite {
        ticket: tid(1),
        expect,
        content_hash: h,
        triage_run: r,
        causes,
        next: TicketStatus::Triaged,
    };
    let both = vec![c2.clone(), c1.clone()];
    assert_eq!(fx.tickets.apply_triage(a, write(run(2), hash(1), TicketStatus::Triaging, both.clone()), &[]).await, Err(TicketStoreError::Conflict), "stale run");
    assert_eq!(fx.tickets.apply_triage(a, write(run(1), hash(2), TicketStatus::Triaging, both.clone()), &[]).await, Err(TicketStoreError::Conflict), "content hash");
    assert_eq!(fx.tickets.apply_triage(a, write(run(1), hash(1), TicketStatus::Triaged, both.clone()), &[]).await, Err(TicketStoreError::Conflict), "status");
    let elsewhere = TriageWrite { ticket: tid(9), ..write(run(1), hash(1), TicketStatus::Triaging, Vec::new()) };
    assert_eq!(fx.tickets.apply_triage(a, elsewhere, &[]).await, Err(TicketStoreError::NotFound));
    assert_eq!(fx.tickets.apply_triage(b, write(run(1), hash(1), TicketStatus::Triaging, Vec::new()), &[]).await, Err(TicketStoreError::NotFound));
    let stray = cause(&edit(&t1, |d| d.id = tid(2)), cid(9), run(1), 3, repo(1));
    assert!(matches!(
        fx.tickets.apply_triage(a, write(run(1), hash(1), TicketStatus::Triaging, vec![c1.clone(), stray]), &[]).await,
        Err(TicketStoreError::Invalid(_))
    ), "a cause of another ticket");
    assert!(matches!(
        fx.tickets.apply_triage(a, write(run(1), hash(1), TicketStatus::Triaging, both.clone()), &[bad_job(a)]).await,
        Err(TicketStoreError::Invalid(_))
    ));
    assert_eq!(fx.tickets.causes(a, tid(1), true).await, Ok(vec![]), "failed applies write nothing");
    assert_eq!(fx.tickets.get_ticket(a, tid(1)).await.map(|v| v.value), Ok(triaging.clone()));
    assert_eq!(drain_jobs(&*fx.jobs).await, Vec::<String>::new());
    assert_eq!(drain_feed(&*fx.feed, a, &mut feed_a).await, Vec::new());

    clock.advance(secs(1));
    let triaged = edit(&triaging, |d| {
        d.status = TicketStatus::Triaged;
        d.updated_at = clock.now();
    });
    assert_eq!(fx.tickets.apply_triage(a, write(run(1), hash(1), TicketStatus::Triaging, both), &[job(a, "ap-1")]).await, Ok(triaged.clone()));
    assert_eq!(fx.tickets.get_ticket(a, tid(1)).await, Ok(Versioned { value: triaged.clone(), rev: 3 }));
    assert_eq!(fx.tickets.causes(a, tid(1), false).await, Ok(vec![c1.clone(), c2.clone()]), "by rank; citations, notes, hashes round-trip");
    assert_eq!(fx.tickets.get_cause(a, cid(1)).await, Ok(c1.clone()));
    assert_eq!(drain_jobs(&*fx.jobs).await, ["ap-1"]);
    assert_eq!(drain_feed(&*fx.feed, a, &mut feed_a).await, [ticket_entry(tid(1))]);
    assert_eq!(fx.tickets.fail_triage(a, tid(1), run(1), &[]).await, Err(TicketStoreError::Conflict), "not triaging any more");
    assert_eq!(fx.tickets.causes(b, tid(1), true).await, Ok(vec![]), "tenant scoped");
    assert_eq!(fx.tickets.get_cause(b, cid(1)).await, Err(TicketStoreError::NotFound), "tenant scoped");

    // A tested cause survives retriage; untested ones are superseded.
    fx.seed.run(a, run(10), SeedRun::Diagnose { cause: cid(2), repo: repo(1), sha: sha(1) }).await;
    let c2_tested = with_status(&c2, CauseStatus::NotReproduced { run: run(10) });
    fx.tickets.set_cause_status(a, cid(2), &CauseStatus::Untested, c2_tested.status.clone(), None, &[]).await.unwrap();
    clock.advance(secs(1));
    let t_sup1 = clock.now();
    fx.tickets.begin_triage(a, tid(1), &[TicketStatus::Triaged], hash(1), run(2)).await.unwrap();
    assert_eq!(fx.tickets.apply_triage(a, write(run(1), hash(1), TicketStatus::Triaging, vec![]), &[]).await, Err(TicketStoreError::Conflict), "the old run is fenced out");
    assert_eq!(fx.tickets.fail_triage(a, tid(1), run(1), &[]).await, Err(TicketStoreError::Conflict), "the old run cannot fail it either");
    let c3 = cause(&t1, cid(3), run(2), 1, repo(1));
    fx.tickets.apply_triage(a, write(run(2), hash(1), TicketStatus::Triaging, vec![c3.clone()]), &[]).await.unwrap();
    assert_eq!(fx.tickets.causes(a, tid(1), false).await, Ok(vec![c3.clone(), c2_tested.clone()]));
    clock.advance(secs(1));
    let t_sup2 = clock.now();
    fx.tickets.begin_triage(a, tid(1), &[TicketStatus::Triaged], hash(1), run(4)).await.unwrap();
    let c4 = cause(&t1, cid(4), run(4), 1, repo(1));
    fx.tickets.apply_triage(a, write(run(4), hash(1), TicketStatus::Triaging, vec![c4.clone()]), &[]).await.unwrap();
    let history = vec![c4.clone(), c2_tested.clone(), superseded(&c3, t_sup2), superseded(&c1, t_sup1)];
    assert_eq!(fx.tickets.causes(a, tid(1), true).await, Ok(history.clone()), "current by rank, then most recently superseded first");
    assert_eq!(fx.tickets.causes(a, tid(1), false).await, Ok(vec![c4.clone(), c2_tested.clone()]));

    // fail_triage: fenced on triage_run.
    clock.advance(secs(1));
    fx.tickets.begin_triage(a, tid(1), &[TicketStatus::Triaged], hash(1), run(5)).await.unwrap();
    let rev = fx.tickets.get_ticket(a, tid(1)).await.unwrap().rev;
    drain_feed(&*fx.feed, a, &mut feed_a).await;
    drain_jobs(&*fx.jobs).await;
    assert_eq!(fx.tickets.fail_triage(a, tid(1), run(4), &[]).await, Err(TicketStoreError::Conflict));
    assert_eq!(fx.tickets.fail_triage(a, tid(9), run(5), &[]).await, Err(TicketStoreError::NotFound));
    assert_eq!(fx.tickets.fail_triage(b, tid(1), run(5), &[]).await, Err(TicketStoreError::NotFound));
    assert!(matches!(fx.tickets.fail_triage(a, tid(1), run(5), &[bad_job(a)]).await, Err(TicketStoreError::Invalid(_))));
    assert_eq!(fx.tickets.get_ticket(a, tid(1)).await.map(|v| (v.value.status, v.rev)), Ok((TicketStatus::Triaging, rev)));
    clock.advance(secs(1));
    let failed = fx.tickets.fail_triage(a, tid(1), run(5), &[job(a, "fail-1")]).await.unwrap();
    assert_eq!((failed.status, failed.triage_run, failed.updated_at), (TicketStatus::TriageFailed, Some(run(5)), clock.now()));
    assert_eq!(fx.tickets.get_ticket(a, tid(1)).await, Ok(Versioned { value: failed, rev: rev + 1 }));
    assert_eq!(fx.tickets.fail_triage(a, tid(1), run(5), &[]).await, Err(TicketStoreError::Conflict));
    assert_eq!(drain_jobs(&*fx.jobs).await, ["fail-1"]);
    assert_eq!(drain_feed(&*fx.feed, a, &mut feed_a).await, [ticket_entry(tid(1))]);

    // A duplicate cause id aborts the whole apply (supersession included).
    fx.tickets.begin_triage(a, tid(1), &[TicketStatus::TriageFailed], hash(1), run(6)).await.unwrap();
    let dup = cause(&t1, cid(1), run(6), 1, repo(1));
    assert_eq!(fx.tickets.apply_triage(a, write(run(6), hash(1), TicketStatus::Triaging, vec![dup]), &[]).await, Err(TicketStoreError::Conflict));
    assert_eq!(fx.tickets.causes(a, tid(1), true).await, Ok(history));
    assert_eq!(fx.tickets.get_ticket(a, tid(1)).await.map(|v| v.value.status), Ok(TicketStatus::Triaging));
}

async fn cause_status_variants_and_ticket_transition(clock: &FakeClock, fx: &TicketFixture) {
    let (a, b) = (org(1), org(2));
    fx.seed.org(a).await;
    fx.seed.org(b).await;
    fx.seed.repo(a, repo(1)).await;
    let t1 = ticket(a, tid(1), "iss-1", "Checkout", "", clock.now());
    fx.tickets.save_ingested(a, &t1, None, &[]).await.unwrap();
    let c1 = cause(&t1, cid(1), run(1), 1, repo(1));
    triage(fx, &t1, run(1), vec![c1.clone()]).await;
    for n in [10, 11] {
        fx.seed.run(a, run(n), SeedRun::Diagnose { cause: cid(1), repo: repo(1), sha: sha(1) }).await;
    }
    fx.seed.gate(a, gate(1), run(10)).await;
    let mut feed_a = FeedId(0);
    drain_feed(&*fx.feed, a, &mut feed_a).await;
    let rev = fx.tickets.get_ticket(a, tid(1)).await.unwrap().rev;
    let to = |expect: TicketStatus, next: TicketStatus| Some(TicketTransition { expect, next });

    clock.advance(secs(1));
    let running = with_status(&c1, CauseStatus::Running(run(10)));
    let got = fx
        .tickets
        .set_cause_status(a, cid(1), &CauseStatus::Untested, running.status.clone(), to(TicketStatus::Triaged, TicketStatus::Diagnosing), &[job(a, "cs-1")])
        .await;
    assert_eq!(got, Ok(running.clone()));
    assert_eq!(fx.tickets.get_cause(a, cid(1)).await, Ok(running.clone()));
    let ticket_now = fx.tickets.get_ticket(a, tid(1)).await.unwrap();
    assert_eq!((ticket_now.value.status, ticket_now.value.updated_at, ticket_now.rev), (TicketStatus::Diagnosing, clock.now(), rev + 1));
    assert_eq!(drain_jobs(&*fx.jobs).await, ["cs-1"]);
    assert_eq!(drain_feed(&*fx.feed, a, &mut feed_a).await, [ticket_entry(tid(1)), cause_entry(cid(1))]);

    let next = CauseStatus::Reproduced { run: run(10), gate: gate(1) };
    assert_eq!(fx.tickets.set_cause_status(a, cid(1), &CauseStatus::Untested, next.clone(), None, &[]).await, Err(TicketStoreError::Conflict));
    assert_eq!(
        fx.tickets.set_cause_status(a, cid(1), &CauseStatus::Running(run(99)), next.clone(), to(TicketStatus::Triaged, TicketStatus::Diagnosed), &[]).await,
        Err(TicketStoreError::Conflict),
        "a ticket CAS miss fails the whole write"
    );
    assert!(matches!(
        fx.tickets.set_cause_status(a, cid(1), &CauseStatus::Running(run(10)), next.clone(), None, &[bad_job(a)]).await,
        Err(TicketStoreError::Invalid(_))
    ));
    assert_eq!(fx.tickets.set_cause_status(a, cid(9), &CauseStatus::Untested, next.clone(), None, &[]).await, Err(TicketStoreError::NotFound));
    assert_eq!(fx.tickets.set_cause_status(b, cid(1), &CauseStatus::Running(run(10)), next.clone(), None, &[]).await, Err(TicketStoreError::NotFound));
    assert_eq!(fx.tickets.get_cause(a, cid(1)).await, Ok(running.clone()), "misses leave the cause unchanged");
    assert_eq!(fx.tickets.get_ticket(a, tid(1)).await.map(|v| (v.value.status, v.rev)), Ok((TicketStatus::Diagnosing, rev + 1)));
    assert_eq!(drain_jobs(&*fx.jobs).await, Vec::<String>::new());
    assert_eq!(drain_feed(&*fx.feed, a, &mut feed_a).await, Vec::new());

    clock.advance(secs(1));
    let reproduced = with_status(&c1, next);
    let got = fx
        .tickets
        .set_cause_status(a, cid(1), &CauseStatus::Running(run(99)), reproduced.status.clone(), to(TicketStatus::Diagnosing, TicketStatus::Diagnosed), &[])
        .await;
    assert_eq!(got, Ok(reproduced.clone()), "the CAS compares the variant only");
    assert_eq!(fx.tickets.get_cause(a, cid(1)).await, Ok(reproduced.clone()));
    assert_eq!(fx.tickets.get_ticket(a, tid(1)).await.map(|v| (v.value.status, v.rev)), Ok((TicketStatus::Diagnosed, rev + 2)));

    let chain = [
        CauseStatus::Inconclusive { run: run(11), reason: "flaky environment".into() },
        CauseStatus::NotReproduced { run: run(11) },
        CauseStatus::Untested,
        CauseStatus::Running(run(11)),
    ];
    let mut current = reproduced.status.clone();
    for status in chain {
        let expected = with_status(&c1, status.clone());
        assert_eq!(fx.tickets.set_cause_status(a, cid(1), &current, status.clone(), None, &[]).await, Ok(expected.clone()));
        assert_eq!(fx.tickets.get_cause(a, cid(1)).await, Ok(expected), "{} round-trips", status.name());
        current = status;
    }
    assert_eq!(drain_feed(&*fx.feed, a, &mut feed_a).await.len(), 2 + 4, "ticket + cause, then one cause entry per write");
}

async fn fix_brief_versions_under_concurrency(clock: &FakeClock, fx: &TicketFixture) {
    let (a, b) = (org(1), org(2));
    fx.seed.org(a).await;
    fx.seed.org(b).await;
    fx.seed.repo(a, repo(1)).await;
    let t1 = ticket(a, tid(1), "iss-1", "Checkout", "", clock.now());
    fx.tickets.save_ingested(a, &t1, None, &[]).await.unwrap();
    triage(fx, &t1, run(1), vec![cause(&t1, cid(1), run(1), 1, repo(1)), cause(&t1, cid(2), run(1), 2, repo(1))]).await;
    fx.seed.run(a, run(10), SeedRun::Diagnose { cause: cid(1), repo: repo(1), sha: sha(1) }).await;
    fx.seed.artifact(a, artifact(1), run(10)).await;
    let mut feed_a = FeedId(0);
    drain_feed(&*fx.feed, a, &mut feed_a).await;
    let brief = |n: u128, cause: CauseId| FixBrief {
        id: brief_id(n),
        org_id: a,
        cause_id: cause,
        version: 77,
        diagnose_run: run(10),
        artifact: artifact(1),
        markdown: format!("# Brief {n}"),
        created_at: clock.now(),
    };

    let job_sets: Vec<Vec<NewJob>> = (1..=8).map(|n| vec![job(a, &format!("brief-{n}"))]).collect();
    let results = join_all((1..=8u128).map(|n| fx.tickets.put_fix_brief(a, brief(n, cid(1)), &job_sets[n as usize - 1]))).await;
    let mut stored: Vec<FixBrief> = results.into_iter().map(|r| r.expect("concurrent put_fix_brief")).collect();
    stored.sort_by_key(|f| f.version);
    assert_eq!(stored.iter().map(|f| f.version).collect::<Vec<_>>(), (1..=8).collect::<Vec<u32>>(), "versions are 1..=8 without gaps or repeats");
    for f in &stored {
        let n = f.id.as_uuid().as_u128() - 0x1400;
        assert_eq!(*f, FixBrief { version: f.version, ..brief(n, cid(1)) }, "the given version is ignored, the rest is stored as given");
    }
    assert_eq!(fx.tickets.fix_briefs(a, cid(1)).await, Ok(stored.clone()), "oldest version first");
    assert_eq!(fx.tickets.latest_fix_brief(a, cid(1)).await, Ok(stored.last().cloned()));
    assert_eq!(drain_jobs(&*fx.jobs).await, (1..=8).map(|n| format!("brief-{n}")).collect::<Vec<_>>());
    assert_eq!(drain_feed(&*fx.feed, a, &mut feed_a).await, vec![cause_entry(cid(1)); 8]);

    clock.advance(secs(1));
    let ninth = fx.tickets.put_fix_brief(a, brief(9, cid(1)), &[]).await.unwrap();
    assert_eq!(ninth, FixBrief { version: 9, ..brief(9, cid(1)) });
    assert!(matches!(fx.tickets.put_fix_brief(a, FixBrief { org_id: b, ..brief(10, cid(1)) }, &[]).await, Err(TicketStoreError::Invalid(_))));
    assert_eq!(fx.tickets.put_fix_brief(a, brief(10, cid(9)), &[]).await, Err(TicketStoreError::NotFound));
    assert_eq!(fx.tickets.put_fix_brief(a, brief(1, cid(1)), &[]).await, Err(TicketStoreError::Conflict), "duplicate brief id");
    assert!(matches!(fx.tickets.put_fix_brief(a, brief(10, cid(1)), &[bad_job(a)]).await, Err(TicketStoreError::Invalid(_))));
    assert_eq!(fx.tickets.fix_briefs(a, cid(1)).await.map(|v| v.len()), Ok(9), "failed puts write nothing");
    assert_eq!(fx.tickets.latest_fix_brief(a, cid(1)).await, Ok(Some(ninth)));
    assert_eq!(drain_feed(&*fx.feed, a, &mut feed_a).await, [cause_entry(cid(1))]);

    assert_eq!(fx.tickets.latest_fix_brief(a, cid(2)).await, Ok(None));
    assert_eq!(fx.tickets.fix_briefs(a, cid(2)).await, Ok(vec![]));
    assert_eq!(fx.tickets.latest_fix_brief(b, cid(1)).await, Ok(None), "tenant scoped");
    assert_eq!(fx.tickets.fix_briefs(b, cid(1)).await, Ok(vec![]), "tenant scoped");
    assert_eq!(fx.tickets.put_fix_brief(b, FixBrief { org_id: b, ..brief(11, cid(1)) }, &[]).await, Err(TicketStoreError::NotFound), "tenant scoped");
}

async fn pull_requests_upsert_progress_and_lookup(clock: &FakeClock, fx: &TicketFixture) {
    let (a, b) = (org(1), org(2));
    fx.seed.org(a).await;
    fx.seed.org(b).await;
    fx.seed.repo(a, repo(1)).await;
    fx.seed.repo(a, repo(2)).await;
    let t1 = edit(&ticket(a, tid(1), "iss-1", "Checkout", "", clock.now()), |d| d.target = target(repo(1), 1, TargetOrigin::DefaultHead));
    fx.tickets.save_ingested(a, &t1, None, &[]).await.unwrap();
    triage(fx, &t1, run(1), vec![cause(&t1, cid(1), run(1), 1, repo(1))]).await;
    fx.tickets.transition(a, tid(1), TicketStatus::Triaged, TicketStatus::FixReady, &[]).await.unwrap();
    fx.seed.run(a, run(20), SeedRun::Fix { cause: cid(1), repo: repo(1), sha: sha(1) }).await;
    fx.seed.gate(a, gate(20), run(20)).await;
    let mut feed_a = FeedId(0);
    drain_feed(&*fx.feed, a, &mut feed_a).await;
    let plan = |id: FixPullRequestId, digest: u8, at: OffsetDateTime| FixPullRequest {
        id,
        org_id: a,
        ticket_id: tid(1),
        fix_run: run(20),
        gate: gate(20),
        change_set: ChangeSetDigest::from_bytes([digest; 32]),
        branch: BranchName::new(format!("mage/eng-iss-1-{digest:02x}")).unwrap(),
        base_sha: sha(1),
        head_commit: None,
        number: None,
        url: None,
        state: FixPrState::Planned,
        created_at: at,
        updated_at: at,
    };

    let p1 = plan(pr_id(1), 1, clock.now());
    assert!(matches!(fx.tickets.upsert_pull_request(a, FixPullRequest { org_id: b, ..p1.clone() }, &[]).await, Err(TicketStoreError::Invalid(_))));
    assert_eq!(fx.tickets.upsert_pull_request(a, FixPullRequest { ticket_id: tid(9), ..p1.clone() }, &[]).await, Err(TicketStoreError::NotFound));
    let inconsistent = FixPullRequest { state: FixPrState::CommitCreated { commit: sha(2) }, ..p1.clone() };
    assert!(matches!(fx.tickets.upsert_pull_request(a, inconsistent, &[]).await, Err(TicketStoreError::Invalid(_))), "state payload without its column");
    assert!(matches!(fx.tickets.upsert_pull_request(a, p1.clone(), &[bad_job(a)]).await, Err(TicketStoreError::Invalid(_))));
    assert_eq!(fx.tickets.pull_requests(a, tid(1)).await, Ok(vec![]), "failed upserts write nothing");
    assert_eq!(drain_feed(&*fx.feed, a, &mut feed_a).await, Vec::new());

    assert_eq!(fx.tickets.upsert_pull_request(a, p1.clone(), &[job(a, "pr-1")]).await, Ok((p1.clone(), true)));
    assert_eq!(drain_jobs(&*fx.jobs).await, ["pr-1"]);
    assert_eq!(drain_feed(&*fx.feed, a, &mut feed_a).await, [ticket_entry(tid(1))]);
    clock.advance(secs(1));
    let again = FixPullRequest { id: pr_id(2), branch: BranchName::new("mage/other").unwrap(), ..plan(pr_id(2), 1, clock.now()) };
    assert_eq!(fx.tickets.upsert_pull_request(a, again, &[job(a, "pr-dup")]).await, Ok((p1.clone(), false)), "the stored row, unchanged");
    assert_eq!(drain_jobs(&*fx.jobs).await, Vec::<String>::new(), "no jobs when not created");
    assert_eq!(drain_feed(&*fx.feed, a, &mut feed_a).await, Vec::new());
    assert_eq!(fx.tickets.upsert_pull_request(a, plan(pr_id(1), 2, clock.now()), &[]).await, Err(TicketStoreError::Conflict), "duplicate PR id");

    // set_pull_request_state: CAS by variant, payload columns.
    clock.advance(secs(1));
    let committed = FixPullRequest { head_commit: Some(sha(2)), state: FixPrState::CommitCreated { commit: sha(2) }, updated_at: clock.now(), ..p1.clone() };
    assert_eq!(fx.tickets.set_pull_request_state(a, pr_id(1), &FixPrState::Planned, committed.state.clone(), &[job(a, "st-1")]).await, Ok(committed.clone()));
    assert_eq!(fx.tickets.pull_requests(a, tid(1)).await, Ok(vec![committed.clone()]));
    assert_eq!(drain_jobs(&*fx.jobs).await, ["st-1"]);
    assert_eq!(drain_feed(&*fx.feed, a, &mut feed_a).await, [ticket_entry(tid(1))]);
    assert_eq!(fx.tickets.set_pull_request_state(a, pr_id(1), &FixPrState::Planned, FixPrState::BranchCreated, &[]).await, Err(TicketStoreError::Conflict));
    assert_eq!(fx.tickets.set_pull_request_state(a, pr_id(9), &FixPrState::Planned, FixPrState::BranchCreated, &[]).await, Err(TicketStoreError::NotFound));
    let any_commit = FixPrState::CommitCreated { commit: sha(9) };
    assert_eq!(fx.tickets.set_pull_request_state(b, pr_id(1), &any_commit, FixPrState::BranchCreated, &[]).await, Err(TicketStoreError::NotFound));
    assert!(matches!(
        fx.tickets.set_pull_request_state(a, pr_id(1), &any_commit, FixPrState::BranchCreated, &[bad_job(a)]).await,
        Err(TicketStoreError::Invalid(_))
    ));
    assert_eq!(fx.tickets.pull_requests(a, tid(1)).await, Ok(vec![committed.clone()]));

    // update_pull_request: branch, head, number, url and state, optional ticket CAS.
    clock.advance(secs(1));
    let branched = FixPullRequest {
        branch: BranchName::new("mage/eng-iss-1-01-2").unwrap(),
        head_commit: Some(sha(3)),
        state: FixPrState::BranchCreated,
        ..committed.clone()
    };
    let got = fx.tickets.update_pull_request(a, &branched, &any_commit, None, &[]).await;
    assert_eq!(got, Ok(FixPullRequest { updated_at: clock.now(), ..branched.clone() }), "the CAS compares the variant only");
    let branched = FixPullRequest { updated_at: clock.now(), ..branched };
    clock.advance(secs(1));
    let opened = FixPullRequest {
        number: Some(42),
        url: Some(url("https://github.com/acme/web/pull/42")),
        state: FixPrState::Opened { number: 42 },
        ..branched.clone()
    };
    let pr_open = Some(TicketTransition { expect: TicketStatus::FixReady, next: TicketStatus::PrOpen });
    assert_eq!(fx.tickets.update_pull_request(a, &opened, &FixPrState::Planned, None, &[]).await, Err(TicketStoreError::Conflict));
    assert!(matches!(fx.tickets.update_pull_request(a, &opened, &FixPrState::BranchCreated, pr_open, &[bad_job(a)]).await, Err(TicketStoreError::Invalid(_))));
    let wrong_number = FixPullRequest { number: Some(43), ..opened.clone() };
    assert!(matches!(fx.tickets.update_pull_request(a, &wrong_number, &FixPrState::BranchCreated, None, &[]).await, Err(TicketStoreError::Invalid(_))));
    assert_eq!(fx.tickets.update_pull_request(a, &FixPullRequest { id: pr_id(9), ..opened.clone() }, &FixPrState::BranchCreated, None, &[]).await, Err(TicketStoreError::NotFound));
    assert_eq!(fx.tickets.update_pull_request(b, &opened, &FixPrState::BranchCreated, None, &[]).await, Err(TicketStoreError::NotFound));
    let stale_ticket = Some(TicketTransition { expect: TicketStatus::Fixing, next: TicketStatus::PrOpen });
    assert_eq!(fx.tickets.update_pull_request(a, &opened, &FixPrState::BranchCreated, stale_ticket, &[]).await, Err(TicketStoreError::Conflict), "ticket CAS miss");
    assert_eq!(fx.tickets.pull_requests(a, tid(1)).await, Ok(vec![branched.clone()]), "misses leave the row unchanged");
    drain_feed(&*fx.feed, a, &mut feed_a).await;
    let rev = fx.tickets.get_ticket(a, tid(1)).await.unwrap().rev;
    let got = fx.tickets.update_pull_request(a, &opened, &FixPrState::BranchCreated, pr_open, &[job(a, "up-1")]).await;
    let opened = FixPullRequest { updated_at: clock.now(), ..opened };
    assert_eq!(got, Ok(opened.clone()));
    assert_eq!(fx.tickets.get_ticket(a, tid(1)).await.map(|v| (v.value.status, v.value.updated_at, v.rev)), Ok((TicketStatus::PrOpen, clock.now(), rev + 1)));
    assert_eq!(drain_jobs(&*fx.jobs).await, ["up-1"]);
    assert_eq!(drain_feed(&*fx.feed, a, &mut feed_a).await, [ticket_entry(tid(1)), ticket_entry(tid(1))]);

    clock.advance(secs(1));
    let closed = FixPullRequest { state: FixPrState::Closed { merged: true }, updated_at: clock.now(), ..opened.clone() };
    assert_eq!(fx.tickets.set_pull_request_state(a, pr_id(1), &FixPrState::Opened { number: 1 }, closed.state.clone(), &[]).await, Ok(closed.clone()));

    // Lookups.
    assert_eq!(fx.tickets.pull_request_by_number(a, repo(1), 42).await, Ok(Some(closed.clone())));
    assert_eq!(fx.tickets.pull_request_by_number(a, repo(2), 42).await, Ok(None), "another repo's PR #42");
    assert_eq!(fx.tickets.pull_request_by_number(a, repo(1), 43).await, Ok(None));
    assert_eq!(fx.tickets.pull_request_by_number(b, repo(1), 42).await, Ok(None), "tenant scoped");
    clock.advance(secs(1));
    let p3 = plan(pr_id(3), 3, clock.now());
    assert_eq!(fx.tickets.upsert_pull_request(a, p3.clone(), &[]).await, Ok((p3.clone(), true)));
    assert_eq!(fx.tickets.pull_requests(a, tid(1)).await, Ok(vec![closed, p3]), "oldest first");
    assert_eq!(fx.tickets.pull_requests(a, tid(9)).await, Ok(vec![]));
    assert_eq!(fx.tickets.pull_requests(b, tid(1)).await, Ok(vec![]), "tenant scoped");
}

async fn resolve_writes_resolution_and_ticket_once(clock: &FakeClock, fx: &TicketFixture) {
    let (a, b) = (org(1), org(2));
    fx.seed.org(a).await;
    fx.seed.org(b).await;
    fx.seed.repo(a, repo(1)).await;
    fx.seed.user(user(1)).await;
    let t1 = ticket(a, tid(1), "iss-1", "Checkout", "", clock.now());
    let t2 = ticket(a, tid(2), "iss-2", "Login", "", clock.now());
    fx.tickets.save_ingested(a, &t1, None, &[]).await.unwrap();
    fx.tickets.save_ingested(a, &t2, None, &[]).await.unwrap();
    triage(fx, &t1, run(1), vec![cause(&t1, cid(1), run(1), 1, repo(1))]).await;
    triage(fx, &t2, run(2), vec![cause(&t2, cid(2), run(2), 1, repo(1))]).await;
    fx.seed.run(a, run(10), SeedRun::Diagnose { cause: cid(1), repo: repo(1), sha: sha(1) }).await;
    fx.seed.artifact(a, artifact(1), run(10)).await;
    let mut feed_a = FeedId(0);
    drain_feed(&*fx.feed, a, &mut feed_a).await;
    let rev = fx.tickets.get_ticket(a, tid(1)).await.unwrap().rev;

    clock.advance(secs(1));
    let r = Resolution {
        ticket_id: tid(1),
        org_id: a,
        confirmed_cause: Some(cid(1)),
        root_cause: "the webhook handler swallowed the timeout".into(),
        fix_pr_url: Some(url("https://github.com/acme/web/pull/42")),
        repro_script: Some(artifact(1)),
        resolved_by: user(1),
        at: clock.now(),
    };
    assert_eq!(fx.tickets.resolve(a, r.clone(), TicketStatus::Diagnosed, &[]).await, Err(TicketStoreError::Conflict), "status CAS");
    assert!(matches!(fx.tickets.resolve(a, Resolution { org_id: b, ..r.clone() }, TicketStatus::Triaged, &[]).await, Err(TicketStoreError::Invalid(_))));
    assert_eq!(fx.tickets.resolve(a, Resolution { ticket_id: tid(9), ..r.clone() }, TicketStatus::Triaged, &[]).await, Err(TicketStoreError::NotFound));
    assert!(matches!(
        fx.tickets.resolve(a, Resolution { confirmed_cause: Some(cid(2)), ..r.clone() }, TicketStatus::Triaged, &[]).await,
        Err(TicketStoreError::Invalid(_))
    ), "the confirmed cause belongs to another ticket");
    assert!(matches!(fx.tickets.resolve(a, r.clone(), TicketStatus::Triaged, &[bad_job(a)]).await, Err(TicketStoreError::Invalid(_))));
    assert_eq!(fx.tickets.resolve(b, Resolution { org_id: b, ..r.clone() }, TicketStatus::Triaged, &[]).await, Err(TicketStoreError::NotFound), "tenant scoped");
    assert_eq!(fx.tickets.resolution(a, tid(1)).await, Ok(None), "failed resolves write nothing");
    assert_eq!(fx.tickets.get_ticket(a, tid(1)).await.map(|v| (v.value.status, v.rev)), Ok((TicketStatus::Triaged, rev)));
    assert_eq!(drain_feed(&*fx.feed, a, &mut feed_a).await, Vec::new());

    let resolved = fx.tickets.resolve(a, r.clone(), TicketStatus::Triaged, &[job(a, "res-1")]).await.unwrap();
    assert_eq!((resolved.status, resolved.updated_at), (TicketStatus::Resolved, clock.now()));
    assert_eq!(fx.tickets.get_ticket(a, tid(1)).await, Ok(Versioned { value: resolved, rev: rev + 1 }));
    assert_eq!(fx.tickets.resolution(a, tid(1)).await, Ok(Some(r.clone())));
    assert_eq!(drain_jobs(&*fx.jobs).await, ["res-1"]);
    assert_eq!(drain_feed(&*fx.feed, a, &mut feed_a).await, [ticket_entry(tid(1))]);
    assert_eq!(fx.tickets.resolve(a, r.clone(), TicketStatus::Resolved, &[]).await, Err(TicketStoreError::Conflict), "resolved once");
    assert_eq!(fx.tickets.resolution(b, tid(1)).await, Ok(None), "tenant scoped");

    let bare = Resolution { ticket_id: tid(2), confirmed_cause: None, fix_pr_url: None, repro_script: None, ..r };
    assert_eq!(fx.tickets.resolve(a, bare.clone(), TicketStatus::Triaged, &[]).await.map(|t| t.status), Ok(TicketStatus::Resolved));
    assert_eq!(fx.tickets.resolution(a, tid(2)).await, Ok(Some(bare)));
}

async fn similar_resolved_filters_matches_and_ranks(clock: &FakeClock, fx: &TicketFixture) {
    let (a, b) = (org(1), org(2));
    fx.seed.org(a).await;
    fx.seed.org(b).await;
    for (o, r) in [(a, repo(1)), (a, repo(2)), (b, repo(3))] {
        fx.seed.repo(o, r).await;
    }
    fx.seed.user(user(1)).await;
    let t0 = clock.now();
    let stripe = vec!["StripeTimeoutError: webhook".to_string()];
    struct Spec {
        org: OrgId,
        n: u128,
        repo: RepoId,
        title: &'static str,
        body: &'static str,
        errors: bool,
        root: &'static str,
        resolved_after: Option<u64>,
    }
    let specs = [
        // A: text and errors.
        Spec { org: a, n: 1, repo: repo(1), title: "Checkout payment fails", body: "stripe webhook", errors: true, root: "handler timed out", resolved_after: Some(10) },
        // B: text only.
        Spec { org: a, n: 2, repo: repo(1), title: "Checkout page slow", body: "", errors: false, root: "query per row", resolved_after: Some(20) },
        // C: errors only (and root cause text "permissions").
        Spec { org: a, n: 3, repo: repo(1), title: "Profile avatar", body: "upload broken", errors: true, root: "bucket permissions", resolved_after: Some(30) },
        // D: other repo.
        Spec { org: a, n: 4, repo: repo(2), title: "Checkout payment fails", body: "", errors: true, root: "other repo", resolved_after: Some(40) },
        // E: excluded.
        Spec { org: a, n: 5, repo: repo(1), title: "Checkout payment fails", body: "", errors: true, root: "excluded", resolved_after: Some(50) },
        // F: resolved after the cutoff.
        Spec { org: a, n: 6, repo: repo(1), title: "Checkout payment fails", body: "", errors: true, root: "too late", resolved_after: Some(100) },
        // G: not resolved.
        Spec { org: a, n: 7, repo: repo(1), title: "Checkout payment fails", body: "", errors: true, root: "", resolved_after: None },
        // H: another org.
        Spec { org: b, n: 8, repo: repo(3), title: "Checkout payment fails", body: "", errors: true, root: "org b", resolved_after: Some(10) },
        // I: neither text nor errors.
        Spec { org: a, n: 9, repo: repo(1), title: "Dark mode", body: "colors", errors: false, root: "css", resolved_after: Some(10) },
    ];
    let mut cause_a = None;
    for s in &specs {
        let t = edit(&ticket(s.org, tid(s.n), &format!("iss-{}", s.n), s.title, s.body, t0), |d| {
            d.target = target(s.repo, 1, TargetOrigin::DefaultHead);
            if s.errors {
                d.error_strings = stripe.clone();
            }
        });
        fx.tickets.save_ingested(s.org, &t, None, &[]).await.unwrap();
        let Some(after) = s.resolved_after else { continue };
        let confirmed = if s.n == 1 {
            let mut data = cause(&t, cid(1), run(1), 1, s.repo).into_data();
            data.evidence.push(citation(s.repo, 9, Some("timeout dropped here"), Some([3; 32])));
            let c = Cause::rehydrate(data);
            triage(fx, &t, run(1), vec![c.clone()]).await;
            cause_a = Some(c.clone());
            Some(c.id)
        } else {
            None
        };
        let r = Resolution {
            ticket_id: t.id,
            org_id: s.org,
            confirmed_cause: confirmed,
            root_cause: s.root.into(),
            fix_pr_url: (s.n == 1).then(|| url("https://github.com/acme/web/pull/7")),
            repro_script: None,
            resolved_by: user(1),
            at: t0 + secs(after),
        };
        let status = fx.tickets.get_ticket(s.org, t.id).await.unwrap().value.status;
        fx.tickets.resolve(s.org, r, status, &[]).await.unwrap();
    }
    let cause_a = cause_a.unwrap();
    let query = SimilarQuery {
        repos: vec![repo(1)],
        text: "checkout".into(),
        error_strings: vec!["StripeTimeoutError: webhook".into()],
        exclude: vec![tid(5)],
        as_of: Some(t0 + secs(60)),
    };

    let got = fx.tickets.similar_resolved(a, &query, 10).await.unwrap();
    let ids: Vec<TicketId> = got.iter().map(|s| s.ticket).collect();
    assert_eq!(ids.first(), Some(&tid(1)), "text + error match ranks above single-signal matches: {ids:?}");
    assert_eq!(ids.iter().copied().collect::<BTreeSet<_>>(), [tid(1), tid(2), tid(3)].into_iter().collect(), "repo, exclude, as_of, status and match filters");
    for s in &got {
        assert!(s.score > 0.0 && s.score <= 1.0, "score {} of {} outside (0, 1]", s.score, s.ticket);
        assert!(s.ticket == tid(1) || s.score < got[0].score, "A ranks strictly first");
    }
    let a_hit = &got[0];
    assert_eq!(a_hit.identifier, "ENG-iss-1");
    assert_eq!(a_hit.title, "Checkout payment fails");
    assert_eq!(a_hit.url, "https://linear.app/acme/issue/iss-1");
    assert_eq!(a_hit.resolved_at, t0 + secs(10));
    assert_eq!(a_hit.root_cause, "handler timed out");
    assert_eq!(a_hit.fix_pr_url.as_deref(), Some("https://github.com/acme/web/pull/7"));
    assert_eq!(a_hit.cause_citations, cause_a.evidence, "citations of the confirmed cause, in order");
    assert_eq!(a_hit.matched_on, [SimilarMatch::Text, SimilarMatch::ErrorString]);
    let by_id = |id: TicketId| got.iter().find(|s| s.ticket == id).unwrap();
    assert_eq!(by_id(tid(2)).matched_on, [SimilarMatch::Text]);
    assert_eq!(by_id(tid(2)).cause_citations, Vec::<Citation>::new(), "no confirmed cause");
    assert_eq!(by_id(tid(2)).fix_pr_url, None);
    assert_eq!(by_id(tid(3)).matched_on, [SimilarMatch::ErrorString]);

    let ids_of = |v: Vec<crate::ports::ticket::SimilarResolved>| v.into_iter().map(|s| s.ticket).collect::<Vec<_>>();
    assert_eq!(ids_of(fx.tickets.similar_resolved(a, &query, 1).await.unwrap()), [tid(1)], "limit");
    assert_eq!(ids_of(fx.tickets.similar_resolved(a, &query, 0).await.unwrap()), [tid(1)], "limit is at least 1");
    let early = SimilarQuery { as_of: Some(t0 + secs(15)), ..query.clone() };
    assert_eq!(ids_of(fx.tickets.similar_resolved(a, &early, 10).await.unwrap()), [tid(1)], "as_of is inclusive and cuts later resolutions");
    let at_cut = SimilarQuery { as_of: Some(t0 + secs(10)), ..query.clone() };
    assert_eq!(ids_of(fx.tickets.similar_resolved(a, &at_cut, 10).await.unwrap()), [tid(1)]);
    let all_time = SimilarQuery { as_of: None, exclude: vec![], ..query.clone() };
    let all: BTreeSet<TicketId> = ids_of(fx.tickets.similar_resolved(a, &all_time, 10).await.unwrap()).into_iter().collect();
    assert_eq!(all, [tid(1), tid(2), tid(3), tid(5), tid(6)].into_iter().collect());
    let root_text = SimilarQuery { text: "permissions".into(), error_strings: vec![], ..query.clone() };
    let hits = fx.tickets.similar_resolved(a, &root_text, 10).await.unwrap();
    assert_eq!((ids_of(hits.clone()), hits[0].matched_on.clone()), (vec![tid(3)], vec![SimilarMatch::Text]), "the root cause is searched too");
    let words = SimilarQuery { text: "checkout slow".into(), error_strings: vec![], ..query.clone() };
    assert_eq!(ids_of(fx.tickets.similar_resolved(a, &words, 10).await.unwrap()), [tid(2)], "every word must match");
    let nothing = SimilarQuery { text: String::new(), error_strings: vec![], ..query.clone() };
    assert_eq!(fx.tickets.similar_resolved(a, &nothing, 10).await, Ok(vec![]));
    let no_repo = SimilarQuery { repos: vec![], ..query.clone() };
    assert_eq!(fx.tickets.similar_resolved(a, &no_repo, 10).await, Ok(vec![]));
    let other_repo = SimilarQuery { repos: vec![repo(2)], ..query.clone() };
    assert_eq!(ids_of(fx.tickets.similar_resolved(a, &other_repo, 10).await.unwrap()), [tid(4)]);
    let org_b = SimilarQuery { repos: vec![repo(1), repo(3)], ..query.clone() };
    assert_eq!(ids_of(fx.tickets.similar_resolved(b, &org_b, 10).await.unwrap()), [tid(8)], "tenant scoped");
}

#[cfg(test)]
mod tests {
    use mage_core::testing::{MemJobQueue, SeqIdGen};

    use super::*;
    use crate::testing::NoSeed;
    use crate::testing::feed::MemChangeFeed;
    use crate::testing::ticket::MemTicketStore;

    #[tokio::test]
    async fn mem_ticket_store_passes_the_contract() {
        contract_ticket_store(|clock| async move {
            let jobs = Arc::new(MemJobQueue::new(clock.clone(), Arc::new(SeqIdGen::new())));
            let feed = Arc::new(MemChangeFeed::new(clock.clone()));
            let tickets = Arc::new(MemTicketStore::new(clock, jobs.clone(), feed.clone()));
            TicketFixture { tickets, jobs, feed, seed: Arc::new(NoSeed) }
        })
        .await;
    }
}
