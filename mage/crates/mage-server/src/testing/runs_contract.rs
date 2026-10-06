//! Generic `RunStore` + `RunSignals` contract suite (runs.md "Testing", "Store invariants"), run
//! against [`MemRunStore`](super::MemRunStore) and `PgRunStore`. Every scenario starts from an
//! empty store built by the factory, driven by the suite's `FakeClock`. Panics on the first
//! violated expectation.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use time::OffsetDateTime;
use uuid::Uuid;

use mage_core::domain::{
    ArtifactKind, Budget, CommitSha, Gate, GateEvent, GateId, GateKind, GateStatus, OrgId, RepoId, RepoVersionKey, RunCommand, RunEventKind,
    RunId, RunKind, RunStatus, RunStatusEvent, RunSubject, RunnerId, Usage, UserId, VerifySpec,
};
use mage_core::ports::clock::Clock;
use mage_core::testing::FakeClock;
use mage_core::wire::runner::{Arch, Capabilities, CredentialModeKind, ExecutorKind, HarnessAvailability, Os, SlotCounts};

use crate::app::runs::machine::{Initiator, step};
use crate::ports::runs::{
    CommandDraft, GateRecord, GateWrite, IdempotentReply, Ingest, LeaseFence, LeaseQuery, NewArtifact, NewRun, RunConflict, RunEventDraft,
    RunQuery, RunRecord, RunSignals, RunStore, RunStoreError, RunUpdate, RunWrite, RunnerSession, SignalTopic, Woke,
};
use crate::ports::tokens::RunnerScope;
use crate::testing::runs::{sample_run, sample_spec};
use crate::testing::seed::Seed;

/// Inserts runner identities (`runs.runner_id` references `runners`).
#[async_trait]
pub trait RunnerSeed: Send + Sync {
    async fn runner(&self, runner: RunnerId);
}

/// For the in-memory fake: nothing to insert.
pub struct NoRunnerSeed;

#[async_trait]
impl RunnerSeed for NoRunnerSeed {
    async fn runner(&self, _runner: RunnerId) {}
}

/// One empty store with its signals and seeders.
pub struct RunStoreFixture {
    pub store: Arc<dyn RunStore>,
    pub signals: Arc<dyn RunSignals>,
    pub seed: Arc<dyn Seed>,
    pub runners: Arc<dyn RunnerSeed>,
}

const T0: OffsetDateTime = FakeClock::DEFAULT_START;

fn budget() -> Budget {
    Budget { max_wall: Duration::from_secs(600), max_turns: 10, max_cost_micros: 1000, max_gate_attempts: 2 }
}

fn caps() -> Capabilities {
    Capabilities {
        arch: Arch::Aarch64,
        os: Os::Linux,
        executor: ExecutorKind::Host,
        harnesses: vec![HarnessAvailability { kind: mage_core::domain::HarnessKind::Omp, version: "1".into(), credential_modes: vec![CredentialModeKind::Gateway] }],
        browser: true,
        slots: SlotCounts { light: 2, work: 2, verify: 2 },
    }
}

struct World {
    f: RunStoreFixture,
    clock: Arc<FakeClock>,
    org: OrgId,
    repo: RepoId,
    n: std::sync::atomic::AtomicU64,
}

impl World {
    async fn new(f: RunStoreFixture, clock: Arc<FakeClock>) -> World {
        let org = OrgId::from_uuid(Uuid::now_v7());
        let repo = RepoId::from_uuid(Uuid::now_v7());
        f.seed.org(org).await;
        f.seed.repo(org, repo).await;
        World { f, clock, org, repo, n: std::sync::atomic::AtomicU64::new(1) }
    }

    fn sha(&self) -> CommitSha {
        let n = self.n.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let mut b = [0u8; 20];
        b[12..].copy_from_slice(&n.to_be_bytes());
        CommitSha(b)
    }

    async fn runner(&self) -> RunnerId {
        let r = RunnerId::from_uuid(Uuid::now_v7());
        self.f.runners.runner(r).await;
        r
    }

    /// A Learn run (Work slot) on the world's repo at a fresh sha, inserted `Queued`.
    async fn learn(&self, create_key: Option<&str>, source_ready: bool) -> RunRecord {
        let sha = self.sha();
        self.learn_at(sha, create_key, source_ready).await.expect("insert learn run")
    }

    async fn learn_at(&self, sha: CommitSha, create_key: Option<&str>, source_ready: bool) -> Result<RunRecord, RunStoreError> {
        let id = RunId::from_uuid(Uuid::now_v7());
        let key = RepoVersionKey::new(self.repo, sha);
        let run = sample_run(self.org, id, RunKind::Learn, RunSubject::RepoVersion(key), budget(), self.clock.now());
        let mut spec = sample_spec(RunKind::Learn);
        spec.repo_id = Some(self.repo);
        spec.sha = Some(sha);
        let w = RunWrite {
            inserts: vec![NewRun { run, spec, create_key: create_key.map(str::to_owned), source_ready, affinity: None }],
            events: vec![RunEventDraft { run_id: id, attempt: 1, at: self.clock.now(), kind: RunEventKind::Status(RunStatus::Queued) }],
            ..RunWrite::default()
        };
        self.f.store.commit(self.org, w).await?;
        self.f.store.get(self.org, id).await
    }

    fn query(&self, runner: RunnerId, free: SlotCounts) -> LeaseQuery {
        let now = self.clock.now();
        LeaseQuery {
            runner_id: runner,
            instance_id: Uuid::from_u128(runner.as_uuid().as_u128()),
            scope: RunnerScope::Shared,
            capabilities: caps(),
            free,
            prepared: Vec::new(),
            request_id: Uuid::now_v7(),
            until: now + time::Duration::seconds(60),
            now,
            repo_limit: 1,
        }
    }
}

fn all_free() -> SlotCounts {
    SlotCounts { light: 1, work: 1, verify: 1 }
}

/// Runs every scenario; `make` builds an empty store on the given clock.
pub async fn contract_run_store<F, Fut>(make: F)
where
    F: Fn(Arc<FakeClock>) -> Fut,
    Fut: Future<Output = RunStoreFixture>,
{
    macro_rules! world {
        () => {{
            let clock = Arc::new(FakeClock::new());
            World::new(make(clock.clone()).await, clock).await
        }};
    }
    one_winner_for_concurrent_leases(world!()).await;
    repo_slot_is_unique_under_concurrent_leasing(world!()).await;
    seq_is_gapless_under_concurrent_commits_and_ingest_dedupes(world!()).await;
    stale_version_duplicate_and_reply_conflicts(world!()).await;
    extend_lease_is_fenced_and_keeps_version(world!()).await;
    signals_follow_commits_not_rollbacks(world!()).await;
    tenancy_and_queries(world!()).await;
    gates_commands_and_artifacts(world!()).await;
    due_and_source_ready(world!()).await;
    lease_order_affinity_and_sessions(world!()).await;
}

async fn one_winner_for_concurrent_leases(w: World) {
    let run = w.learn(None, true).await;
    let mut runners = Vec::new();
    for _ in 0..8 {
        runners.push(w.runner().await);
    }
    let qs: Vec<LeaseQuery> = runners.iter().map(|r| w.query(*r, all_free())).collect();
    let results = futures::future::join_all(qs.iter().map(|q| w.f.store.lease_next(q))).await;
    let winners: Vec<RunRecord> = results.into_iter().map(|r| r.expect("lease_next")).flatten().collect();
    assert_eq!(winners.len(), 1, "exactly one runner leases the run");
    let leased = &winners[0];
    assert_eq!(leased.id(), run.id());
    assert_eq!(leased.run.status, RunStatus::Leased);
    assert!(leased.version > run.version);
    let stored = w.f.store.get(w.org, run.id()).await.unwrap();
    assert_eq!(stored.lease.map(|l| l.runner_id), leased.lease.map(|l| l.runner_id));
    let events = w.f.store.events_after(w.org, run.id(), 0, 10).await.unwrap();
    assert_eq!(events.last().map(|e| &e.kind), Some(&RunEventKind::Status(RunStatus::Leased)));
    // lease_by_request returns it for the winner only
    let winner_q = qs.iter().find(|q| Some(q.runner_id) == leased.lease.map(|l| l.runner_id)).unwrap();
    let again = w.f.store.lease_by_request(winner_q.runner_id, winner_q.request_id).await.unwrap();
    assert_eq!(again.map(|r| r.id()), Some(run.id()));
    assert_eq!(w.f.store.lease_by_request(winner_q.runner_id, Uuid::now_v7()).await.unwrap(), None);
    // not leasable twice
    assert!(w.f.store.lease_next(&w.query(winner_q.runner_id, all_free())).await.unwrap().is_none());
}

async fn repo_slot_is_unique_under_concurrent_leasing(w: World) {
    let a = w.learn(None, true).await;
    let b = w.learn(None, true).await;
    let (r1, r2) = (w.runner().await, w.runner().await);
    let (q1, q2) = (w.query(r1, all_free()), w.query(r2, all_free()));
    let (x, y) = futures::join!(w.f.store.lease_next(&q1), w.f.store.lease_next(&q2));
    let leased: Vec<RunRecord> = [x.unwrap(), y.unwrap()].into_iter().flatten().collect();
    assert_eq!(leased.len(), 1, "one Work lease per repo");
    let other = if leased[0].id() == a.id() { b.id() } else { a.id() };
    assert_eq!(w.f.store.get(w.org, other).await.unwrap().run.status, RunStatus::Queued);
    // a Verify child of the leased run is still leasable (verify slot class)
    let parent = &leased[0];
    let vid = RunId::from_uuid(Uuid::now_v7());
    let v = sample_run(w.org, vid, RunKind::Verify, RunSubject::Run(parent.id()), budget(), w.clock.now());
    let mut spec = sample_spec(RunKind::Verify);
    spec.repo_id = parent.spec.repo_id;
    spec.sha = parent.spec.sha;
    spec.verify = Some(VerifySpec::Boot);
    w.f.store.commit(w.org, RunWrite { inserts: vec![NewRun { run: v, spec, create_key: None, source_ready: true, affinity: None }], ..RunWrite::default() }).await.unwrap();
    let r3 = w.runner().await;
    let got = w.f.store.lease_next(&w.query(r3, SlotCounts { light: 0, work: 0, verify: 1 })).await.unwrap();
    assert_eq!(got.map(|r| r.id()), Some(vid));
    // releasing the slot makes the other Work run leasable
    let mut freed = w.f.store.get(w.org, parent.id()).await.unwrap();
    let mut wr = RunWrite::default();
    step(&mut freed, RunStatusEvent::Cancel, Initiator::Server, w.clock.now(), &mut wr).unwrap();
    freed.lease = None;
    freed.run.release_lease();
    wr.updates.push(RunUpdate::of(freed));
    w.f.store.commit(w.org, wr).await.unwrap();
    let got = w.f.store.lease_next(&w.query(r2, all_free())).await.unwrap();
    assert_eq!(got.map(|r| r.id()), Some(other));
}

async fn seq_is_gapless_under_concurrent_commits_and_ingest_dedupes(w: World) {
    let run = w.learn(None, true).await;
    let commits = (0..10).map(|i| {
        let ev = RunEventDraft { run_id: run.id(), attempt: 1, at: w.clock.now(), kind: RunEventKind::Notice(format!("n{i}")) };
        w.f.store.commit(w.org, RunWrite { events: vec![ev.clone(), ev], ..RunWrite::default() })
    });
    for r in futures::future::join_all(commits).await {
        let receipt = r.unwrap();
        let (_, first, last) = receipt.seqs[0];
        assert_eq!(last, first + 1, "no gap inside one commit");
    }
    let events = w.f.store.events_after(w.org, run.id(), 0, 100).await.unwrap();
    let seqs: Vec<u64> = events.iter().map(|e| e.seq).collect();
    assert_eq!(seqs, (1..=21).collect::<Vec<_>>(), "strictly increasing, gapless");
    // ingest cursor
    let draft = |n: u64| (n, RunEventDraft { run_id: run.id(), attempt: 1, at: w.clock.now(), kind: RunEventKind::AssistantText(format!("t{n}")) });
    let batch = |evs: Vec<(u64, RunEventDraft)>| RunWrite { ingest: Some(Ingest { run_id: run.id(), attempt: 1, events: evs }), ..RunWrite::default() };
    let r1 = w.f.store.commit(w.org, batch(vec![draft(1), draft(2), draft(3)])).await.unwrap();
    assert_eq!(r1.ingested, Some((3, 24)));
    let r2 = w.f.store.commit(w.org, batch(vec![draft(2), draft(3), draft(4)])).await.unwrap();
    assert_eq!(r2.ingested, Some((4, 25)), "duplicates dropped");
    let r3 = w.f.store.commit(w.org, batch(vec![draft(1), draft(2)])).await.unwrap();
    assert_eq!(r3.ingested, Some((4, 25)), "a replayed batch is ingested once");
    let texts = w.f.store.events_after(w.org, run.id(), 21, 100).await.unwrap();
    assert_eq!(texts.len(), 4);
    assert_eq!(w.f.store.get(w.org, run.id()).await.unwrap().last_seq, 25);
}

async fn stale_version_duplicate_and_reply_conflicts(w: World) {
    let run = w.learn(Some("job-1"), true).await;
    // Duplicate create key → the existing run
    let dup = w.learn_at(w.sha(), Some("job-1"), true).await;
    assert_eq!(dup.unwrap_err(), RunStoreError::Conflict(RunConflict::Duplicate { run: run.id() }));
    // StaleVersion: two updates from the same read
    let mut a = run.clone();
    a.session_lost = true;
    let mut b = run.clone();
    b.source_ready = false;
    w.f.store.commit(w.org, RunWrite { updates: vec![RunUpdate::of(a)], ..RunWrite::default() }).await.unwrap();
    let err = w.f.store.commit(w.org, RunWrite { updates: vec![RunUpdate::of(b)], ..RunWrite::default() }).await.unwrap_err();
    assert_eq!(err, RunStoreError::Conflict(RunConflict::StaleVersion { run: run.id() }));
    let stored = w.f.store.get(w.org, run.id()).await.unwrap();
    assert!(stored.session_lost && stored.source_ready, "the failed commit wrote nothing");
    assert_eq!(stored.version, run.version + 1);
    // ReplyExists fails the whole commit
    let reply = |hash: u8| IdempotentReply { scope: "runner:x:result:y".into(), key: "k".into(), request_hash: [hash; 32], body: br#"{"ok":1}"#.to_vec() };
    let ev = RunEventDraft { run_id: run.id(), attempt: 1, at: w.clock.now(), kind: RunEventKind::Notice("once".into()) };
    w.f.store.commit(w.org, RunWrite { reply: Some(reply(1)), events: vec![ev.clone()], ..RunWrite::default() }).await.unwrap();
    let err = w.f.store.commit(w.org, RunWrite { reply: Some(reply(2)), events: vec![ev], ..RunWrite::default() }).await.unwrap_err();
    match err {
        RunStoreError::Conflict(RunConflict::ReplyExists(stored)) => {
            assert_eq!(stored.request_hash, [1; 32]);
            assert_eq!(serde_json::from_slice::<serde_json::Value>(&stored.body).unwrap(), serde_json::json!({"ok": 1}));
        }
        other => panic!("expected ReplyExists, got {other:?}"),
    }
    let notices = w.f.store.events_after(w.org, run.id(), 0, 100).await.unwrap();
    assert_eq!(notices.iter().filter(|e| e.kind == RunEventKind::Notice("once".into())).count(), 1);
    assert_eq!(w.f.store.reply("runner:x:result:y", "k").await.unwrap().map(|r| r.request_hash), Some([1; 32]));
    assert_eq!(w.f.store.reply("runner:x:result:y", "other").await.unwrap(), None);
}

async fn extend_lease_is_fenced_and_keeps_version(w: World) {
    w.learn(None, true).await;
    let r = w.runner().await;
    let q = w.query(r, all_free());
    let leased = w.f.store.lease_next(&q).await.unwrap().unwrap();
    let fence = LeaseFence { runner_id: r, instance_id: q.instance_id, attempt: 1 };
    let until = w.clock.now() + time::Duration::seconds(120);
    let ext = w.f.store.extend_lease(w.org, leased.id(), fence, until).await.unwrap();
    assert_eq!(ext.version, leased.version, "extend_lease never bumps version");
    assert_eq!(ext.lease.map(|l| l.until), Some(until));
    assert_eq!(ext.run.runner.map(|l| l.until), Some(until));
    // a commit from an older read keeps the later until (same lease identity)
    let mut older = leased.clone();
    older.session_lost = false;
    w.f.store.commit(w.org, RunWrite { updates: vec![RunUpdate::of(older)], ..RunWrite::default() }).await.unwrap();
    assert_eq!(w.f.store.get(w.org, leased.id()).await.unwrap().lease.map(|l| l.until), Some(until));
    let wrong = LeaseFence { instance_id: Uuid::now_v7(), ..fence };
    assert_eq!(w.f.store.extend_lease(w.org, leased.id(), wrong, until).await.unwrap_err(), RunStoreError::Conflict(RunConflict::LeaseLost));
    // lease lost → requeued attempt 2 → the old fence is refused
    let mut rec = w.f.store.get(w.org, leased.id()).await.unwrap();
    let mut wr = RunWrite::default();
    step(&mut rec, RunStatusEvent::LeaseLost { max_attempts: 3 }, Initiator::Server, w.clock.now(), &mut wr).unwrap();
    wr.updates.push(RunUpdate::of(rec));
    w.f.store.commit(w.org, wr).await.unwrap();
    let before = w.f.store.get(w.org, leased.id()).await.unwrap();
    assert_eq!((before.run.status, before.run.attempt), (RunStatus::Queued, 2));
    assert_eq!(w.f.store.extend_lease(w.org, leased.id(), fence, until).await.unwrap_err(), RunStoreError::Conflict(RunConflict::LeaseLost));
    assert_eq!(w.f.store.get(w.org, leased.id()).await.unwrap().version, before.version);
    assert_eq!(w.f.store.extend_lease(w.org, RunId::from_uuid(Uuid::now_v7()), fence, until).await.unwrap_err(), RunStoreError::NotFound);
}

async fn signals_follow_commits_not_rollbacks(w: World) {
    let run = w.learn(None, true).await;
    let mut events = w.f.signals.subscribe(SignalTopic::RunEvents(run.id())).await.unwrap();
    let mut commands = w.f.signals.subscribe(SignalTopic::RunCommands(run.id())).await.unwrap();
    let mut queue = w.f.signals.subscribe(SignalTopic::Queue).await.unwrap();
    // Drain wakeups from earlier (e.g. a listener connect).
    let _ = events.wait(Duration::from_secs(1)).await;
    let _ = commands.wait(Duration::from_millis(10)).await;
    let _ = queue.wait(Duration::from_millis(10)).await;
    // a rolled-back commit (stale version, with events) wakes nobody
    let mut stale = run.clone();
    stale.version += 7;
    let ev = RunEventDraft { run_id: run.id(), attempt: 1, at: w.clock.now(), kind: RunEventKind::Notice("x".into()) };
    let failed = RunWrite { updates: vec![RunUpdate { record: stale.clone(), expected_version: stale.version }], events: vec![ev.clone()], ..RunWrite::default() };
    assert!(w.f.store.commit(w.org, failed).await.is_err());
    assert_eq!(events.wait(Duration::from_millis(500)).await, Woke::Timeout, "no wake for a rolled-back commit");
    w.f.store.commit(w.org, RunWrite { events: vec![ev], ..RunWrite::default() }).await.unwrap();
    assert_eq!(events.wait(Duration::from_secs(5)).await, Woke::Signal);
    let cmd = CommandDraft { run_id: run.id(), attempt: 1, command: RunCommand::Steer { text: "hi".into(), by: UserId::from_uuid(Uuid::now_v7()) } };
    w.f.store.commit(w.org, RunWrite { commands: vec![cmd], ..RunWrite::default() }).await.unwrap();
    assert_eq!(commands.wait(Duration::from_secs(5)).await, Woke::Signal);
    w.learn(None, true).await;
    assert_eq!(queue.wait(Duration::from_secs(5)).await, Woke::Signal, "a leasable insert signals Queue");
}

async fn tenancy_and_queries(w: World) {
    let a = w.learn(None, true).await;
    w.clock.advance(Duration::from_secs(1));
    let b = w.learn(None, false).await;
    let other = OrgId::from_uuid(Uuid::now_v7());
    w.f.seed.org(other).await;
    assert_eq!(w.f.store.get(other, a.id()).await.unwrap_err(), RunStoreError::NotFound);
    assert_eq!(w.f.store.events_after(other, a.id(), 0, 10).await.unwrap_err(), RunStoreError::NotFound);
    assert_eq!(w.f.store.locate(a.id()).await.unwrap(), w.org);
    let newest = w.f.store.list(w.org, &RunQuery { newest_first: true, ..RunQuery::default() }).await.unwrap();
    assert_eq!(newest.iter().map(|r| r.id()).collect::<Vec<_>>(), vec![b.id(), a.id()]);
    let page = w.f.store.list(w.org, &RunQuery { newest_first: true, limit: 1, cursor: Some(b.id()), ..RunQuery::default() }).await.unwrap();
    assert_eq!(page.iter().map(|r| r.id()).collect::<Vec<_>>(), vec![a.id()]);
    let by_sha = w.f.store.list(w.org, &RunQuery { repo: Some(w.repo), sha: b.spec.sha, ..RunQuery::default() }).await.unwrap();
    assert_eq!(by_sha.len(), 1);
    let subject = w.f.store.list(w.org, &RunQuery { subject: Some(a.run.subject), ..RunQuery::default() }).await.unwrap();
    assert_eq!(subject.iter().map(|r| r.id()).collect::<Vec<_>>(), vec![a.id()]);
    let queued = w.f.store.list(w.org, &RunQuery { statuses: vec![RunStatus::Running], ..RunQuery::default() }).await.unwrap();
    assert!(queued.is_empty());
    assert!(w.f.store.list(other, &RunQuery::default()).await.unwrap().is_empty());
    // usage report
    let mut rec = w.f.store.get(w.org, a.id()).await.unwrap();
    rec.usage_metered = Usage { cost_micros: 77, ..Usage::default() };
    w.f.store.commit(w.org, RunWrite { updates: vec![RunUpdate::of(rec)], ..RunWrite::default() }).await.unwrap();
    let usage = w.f.store.usage(w.org, T0, T0 + time::Duration::hours(1)).await.unwrap();
    assert_eq!(usage.len(), 2);
    assert_eq!(usage[0].run_id, a.id());
    assert_eq!(usage[0].usage.cost_micros, 77);
}

async fn gates_commands_and_artifacts(w: World) {
    let parent = w.learn(None, true).await;
    let vid = RunId::from_uuid(Uuid::now_v7());
    let v = sample_run(w.org, vid, RunKind::Verify, RunSubject::Run(parent.id()), budget(), w.clock.now());
    let mut spec = sample_spec(RunKind::Verify);
    spec.repo_id = parent.spec.repo_id;
    spec.sha = parent.spec.sha;
    spec.verify = Some(VerifySpec::Boot);
    let gate = GateRecord {
        gate: Gate { id: GateId::from_uuid(Uuid::now_v7()), org_id: w.org, run_id: parent.id(), kind: GateKind::Boot, attempt: 1, status: GateStatus::Pending, verify_run: Some(vid) },
        feedback: None,
        details: Some(serde_json::json!({"x": 1})),
        created_at: w.clock.now(),
    };
    w.f.store
        .commit(
            w.org,
            RunWrite {
                inserts: vec![NewRun { run: v, spec, create_key: None, source_ready: true, affinity: None }],
                gates: vec![GateWrite::Insert(gate.clone())],
                ..RunWrite::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(w.f.store.gate_for_verify(w.org, vid).await.unwrap(), Some(gate.clone()));
    let mut decided = gate.clone();
    decided.gate.status = GateStatus::Pending.apply(GateEvent::Fail { reason: "boom".into(), at: w.clock.now() }).unwrap();
    decided.feedback = Some("fix it".into());
    w.f.store.commit(w.org, RunWrite { gates: vec![GateWrite::Update(decided.clone())], ..RunWrite::default() }).await.unwrap();
    assert_eq!(w.f.store.gates(w.org, parent.id()).await.unwrap(), vec![decided]);
    let again = RunWrite { gates: vec![GateWrite::Insert(gate)], ..RunWrite::default() };
    assert!(matches!(w.f.store.commit(w.org, again).await, Err(RunStoreError::Invalid(_))), "one gate per (run, kind, attempt)");
    // commands are per attempt
    let steer = |attempt| CommandDraft { run_id: parent.id(), attempt, command: RunCommand::Steer { text: format!("a{attempt}"), by: UserId::from_uuid(Uuid::nil()) } };
    let receipt = w.f.store.commit(w.org, RunWrite { commands: vec![steer(1), steer(2), steer(1)], ..RunWrite::default() }).await.unwrap();
    assert_eq!(receipt.command_seqs.iter().map(|(_, s)| *s).collect::<Vec<_>>(), vec![1, 2, 3]);
    let first = w.f.store.commands_after(w.org, parent.id(), 1, 0, 10).await.unwrap();
    assert_eq!(first.iter().map(|c| c.seq).collect::<Vec<_>>(), vec![1, 3]);
    assert_eq!(w.f.store.commands_after(w.org, parent.id(), 1, 1, 10).await.unwrap().len(), 1);
    assert_eq!(w.f.store.commands_after(w.org, parent.id(), 2, 0, 10).await.unwrap().len(), 1);
    // artifacts: reserve is idempotent, complete flips state
    let new = NewArtifact {
        run_id: parent.id(),
        attempt: 1,
        kind: ArtifactKind::Log,
        name: "verify-r1.log".into(),
        bytes: 12,
        sha256: [9; 32],
        content_type: "text/plain".into(),
    };
    let a1 = w.f.store.reserve_artifact(w.org, new.clone()).await.unwrap();
    let a2 = w.f.store.reserve_artifact(w.org, new.clone()).await.unwrap();
    assert_eq!(a1, a2);
    assert_eq!(a1.state, crate::ports::runs::ArtifactState::Pending);
    assert!(a1.artifact.key.ends_with("/log/verify-r1.log"));
    assert_eq!((a1.artifact.bytes, a1.artifact.sha256), (12, [9; 32]), "pending rows keep the declaration");
    w.f.store.commit(w.org, RunWrite { complete_artifacts: vec![(a1.artifact.id, 12)], ..RunWrite::default() }).await.unwrap();
    let done = w.f.store.artifact(w.org, a1.artifact.id).await.unwrap();
    assert_eq!(done.state, crate::ports::runs::ArtifactState::Complete);
    assert_eq!(w.f.store.artifacts(w.org, parent.id()).await.unwrap(), vec![done]);
    let other = OrgId::from_uuid(Uuid::now_v7());
    assert_eq!(w.f.store.artifact(other, a1.artifact.id).await.unwrap_err(), RunStoreError::NotFound);
    assert!(w.f.store.reserve_artifact(w.org, NewArtifact { name: "../x".into(), ..new }).await.is_err());
}

async fn due_and_source_ready(w: World) {
    let waiting = w.learn(None, false).await;
    let runner = w.runner().await;
    let leased = {
        w.learn(None, true).await;
        w.f.store.lease_next(&w.query(runner, all_free())).await.unwrap().unwrap()
    };
    assert!(w.f.store.due(w.clock.now(), 100).await.unwrap().is_empty(), "nothing due yet");
    // the lease expires
    w.clock.advance(Duration::from_secs(61));
    let due = w.f.store.due(w.clock.now(), 100).await.unwrap();
    assert!(due.iter().any(|d| d.run_id == leased.id() && d.reason == crate::ports::runs::DueReason::LeaseExpired), "{due:?}");
    assert_eq!(w.f.store.held_by(runner).await.unwrap(), vec![(w.org, leased.id())]);
    // queue wait (default threshold 2 h)
    w.clock.advance(Duration::from_secs(3 * 3600));
    let due = w.f.store.due(w.clock.now(), 100).await.unwrap();
    assert!(due.iter().any(|d| d.run_id == waiting.id() && d.reason == crate::ports::runs::DueReason::QueueWait), "{due:?}");
    // source ready flips only Queued runs at (repo, sha) and bumps their version
    let n = w.f.store.mark_source_ready(w.org, w.repo, waiting.spec.sha.unwrap()).await.unwrap();
    assert_eq!(n, 1);
    let ready = w.f.store.get(w.org, waiting.id()).await.unwrap();
    assert!(ready.source_ready);
    assert_eq!(ready.version, waiting.version + 1);
    assert_eq!(w.f.store.mark_source_ready(w.org, w.repo, waiting.spec.sha.unwrap()).await.unwrap(), 0);
}

async fn lease_order_affinity_and_sessions(w: World) {
    let parent = w.learn(None, true).await;
    let (mine, other) = (w.runner().await, w.runner().await);
    let mk_verify = |p: &RunRecord, affinity: Option<crate::ports::runs::Affinity>| {
        let vid = RunId::from_uuid(Uuid::now_v7());
        let v = sample_run(w.org, vid, RunKind::Verify, RunSubject::Run(p.id()), budget(), w.clock.now());
        let mut spec = sample_spec(RunKind::Verify);
        spec.repo_id = p.spec.repo_id;
        spec.sha = p.spec.sha;
        spec.verify = Some(VerifySpec::Boot);
        (vid, NewRun { run: v, spec, create_key: None, source_ready: true, affinity })
    };
    let until = w.clock.now() + time::Duration::seconds(30);
    let (vid, new) = mk_verify(&parent, Some(crate::ports::runs::Affinity { runner_id: mine, until }));
    w.f.store.commit(w.org, RunWrite { inserts: vec![new], ..RunWrite::default() }).await.unwrap();
    let verify_only = SlotCounts { light: 0, work: 0, verify: 1 };
    assert!(w.f.store.lease_next(&w.query(other, verify_only)).await.unwrap().is_none(), "affinity holds others off");
    w.clock.advance(Duration::from_secs(31));
    let got = w.f.store.lease_next(&w.query(other, verify_only)).await.unwrap();
    assert_eq!(got.map(|r| r.id()), Some(vid), "after the wait any capable runner may take it");
    // own affinity comes first
    let (v2, n2) = mk_verify(&parent, None);
    let until = w.clock.now() + time::Duration::seconds(30);
    let (v3, n3) = mk_verify(&parent, Some(crate::ports::runs::Affinity { runner_id: mine, until }));
    w.f.store.commit(w.org, RunWrite { inserts: vec![n2, n3], ..RunWrite::default() }).await.unwrap();
    let got = w.f.store.lease_next(&w.query(mine, verify_only)).await.unwrap();
    assert_eq!(got.map(|r| r.id()), Some(v3));
    let got = w.f.store.lease_next(&w.query(mine, verify_only)).await.unwrap();
    assert_eq!(got.map(|r| r.id()), Some(v2));
    // scope
    let scoped = LeaseQuery { scope: RunnerScope::Orgs(vec![OrgId::from_uuid(Uuid::now_v7())]), ..w.query(mine, all_free()) };
    assert!(w.f.store.lease_next(&scoped).await.unwrap().is_none(), "out-of-scope runs are not leased");
    // sessions
    let s = RunnerSession { runner_id: mine, instance_id: Uuid::from_u128(1), version: "1.0".into(), capabilities: caps(), at: w.clock.now() };
    assert_eq!(w.f.store.record_runner_session(&s).await.unwrap(), None);
    let s2 = RunnerSession { instance_id: Uuid::from_u128(2), ..s.clone() };
    assert_eq!(w.f.store.record_runner_session(&s2).await.unwrap(), Some(Uuid::from_u128(1)));
    assert_eq!(w.f.store.runner_session(mine).await.unwrap(), Some(s2));
}
