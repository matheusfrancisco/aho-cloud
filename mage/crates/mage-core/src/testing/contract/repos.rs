//! `RepoStore` contract (storage.md "Testing" → `RepoStore`; knowledge.md layer and batch rules).

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use time::OffsetDateTime;
use uuid::Uuid;

use crate::domain::customer::{Deployment, DeploymentSource};
use crate::domain::ids::{CommitSha, InstallationId, OrgId, RecipeHash, RepoId, RepoVersionKey, TicketId, TreeOid};
use crate::domain::job::{IndexPriority, JobKind, JobLane, JobPayload};
use crate::domain::recipe::{Recipe, RecipeData, RecipeStatus};
use crate::domain::repo::{
    AppMapRev, ErrorsRev, IndexRevs, Layer, LayerEvent, LayerState, ParserRev, Repo, RepoAccess, SourceBundleRef, VersionReason,
};
use crate::ports::clock::Clock;
use crate::ports::common::PageRequest;
use crate::ports::jobs::{JobQueue, LeaseRequest, LeasedJob, NewJob};
use crate::ports::repos::{RepoStore, RepoStoreError};
use crate::testing::clock::FakeClock;
use crate::testing::contract::RepoStoreFixture;

/// Runs every `RepoStore` scenario, each against a fresh fixture from `make`.
pub async fn contract_repo_store<F, Fut>(make: F)
where
    F: Fn(Arc<FakeClock>) -> Fut,
    Fut: Future<Output = RepoStoreFixture>,
{
    macro_rules! scenario {
        ($name:ident) => {{
            let clock = Arc::new(FakeClock::new());
            let fixture = make(clock.clone()).await;
            $name(&clock, &*fixture.repos, &*fixture.jobs).await;
        }};
    }
    scenario!(repos_upsert_by_external_id_and_look_up);
    scenario!(version_reasons_are_idempotent_and_singletons_move);
    scenario!(layer_events_follow_the_state_table_with_outbox);
    scenario!(layer_batches_count_once_and_fire_on_last_once);
    scenario!(rebuild_replaces_batches_and_keeps_ready);
    scenario!(revs_and_bundles_with_reuse_by_tree);
    scenario!(delete_version_removes_reasons_layers_and_batches);
    scenario!(recipes_insert_once_and_latest_prefers_validated);
    scenario!(deployments_are_an_append_only_history);
}

fn org(n: u128) -> OrgId {
    OrgId::from_uuid(Uuid::from_u128(0x0200 + n))
}

fn repo_id(n: u128) -> RepoId {
    RepoId::from_uuid(Uuid::from_u128(0x0300 + n))
}

fn sha(n: u8) -> CommitSha {
    CommitSha::from_bytes([n; 20])
}

fn repo(org: OrgId, id: RepoId, external_id: u64, owner: &str, name: &str, created_at: OffsetDateTime) -> Repo {
    Repo {
        id,
        org_id: org,
        external_id,
        owner: owner.into(),
        name: name.into(),
        default_branch: "main".into(),
        installation_id: InstallationId(42),
        access: RepoAccess::Granted,
        created_at,
    }
}

async fn seed_repo(clock: &FakeClock, repos: &dyn RepoStore, org: OrgId, id: RepoId) -> Repo {
    let external = id.as_uuid().as_u128() as u64;
    repos.upsert_repo(&repo(org, id, external, "acme", &format!("repo-{external}"), clock.now()), &[]).await.expect("upsert_repo")
}

fn key(repo: RepoId, n: u8) -> RepoVersionKey {
    RepoVersionKey::new(repo, sha(n))
}

/// Indexer-lane job (`index.error_sites`) for `(org, key)`.
fn error_sites_job(org: OrgId, key: RepoVersionKey) -> NewJob {
    NewJob::new(JobPayload::IndexErrorSites { org_id: org, repo_id: key.repo_id, sha: key.sha, prio: IndexPriority::Normal })
}

fn invalid_job(org: OrgId, key: RepoVersionKey) -> NewJob {
    NewJob { kind: JobKind::IndexGc, ..error_sites_job(org, key) }
}

async fn leased_kinds(jobs: &dyn JobQueue, lane: JobLane) -> Vec<LeasedJob> {
    jobs.lease(&LeaseRequest { lane, kinds: None, worker: "contract".into(), max: 100 }).await.expect("lease")
}

async fn repos_upsert_by_external_id_and_look_up(clock: &FakeClock, repos: &dyn RepoStore, jobs: &dyn JobQueue) {
    let t0 = clock.now();
    let a = repo(org(1), repo_id(1), 1001, "Acme", "Web", t0);
    let resync = NewJob::new(JobPayload::IntakeGithubResync { org_id: org(1) });
    assert_eq!(repos.upsert_repo(&a, std::slice::from_ref(&resync)).await, Ok(a.clone()));
    let leased = leased_kinds(jobs, JobLane::Server).await;
    assert_eq!(leased.iter().map(|l| l.kind).collect::<Vec<_>>(), vec![JobKind::IntakeGithubResync], "upsert_repo enqueues its jobs");

    clock.advance(Duration::from_secs(60));
    let renamed = Repo {
        id: repo_id(99),
        owner: "acme".into(),
        name: "webapp".into(),
        default_branch: "trunk".into(),
        installation_id: InstallationId(43),
        access: RepoAccess::Lost,
        created_at: clock.now(),
        ..a.clone()
    };
    let stored = repos.upsert_repo(&renamed, &[]).await.unwrap();
    assert_eq!(stored, Repo { id: a.id, created_at: a.created_at, ..renamed.clone() }, "the external id finds the row; id and created_at stay");
    assert_eq!(repos.repo(org(1), repo_id(1)).await, Ok(stored.clone()));
    assert_eq!(repos.repo(org(1), repo_id(99)).await, Err(RepoStoreError::NotFound));
    assert_eq!(repos.get_by_external_id(org(1), 1001).await, Ok(Some(stored.clone())));
    assert_eq!(repos.get_by_external_id(org(2), 1001).await, Ok(None), "tenant scoped");
    assert_eq!(repos.repo_by_name(org(1), "ACME", "WebApp").await, Ok(Some(stored.clone())), "names are case-insensitive");
    assert_eq!(repos.repo_by_name(org(1), "acme", "web").await, Ok(None));

    let clash = repo(org(1), repo_id(1), 2002, "acme", "other", t0);
    assert_eq!(repos.upsert_repo(&clash, &[]).await, Err(RepoStoreError::Conflict), "a new external id cannot reuse a repo id");
    let invalid = NewJob { kind: JobKind::IndexGc, ..resync };
    let b = repo(org(1), repo_id(2), 1002, "acme", "api", t0);
    assert!(matches!(repos.upsert_repo(&b, &[invalid]).await, Err(RepoStoreError::Invalid(_))));
    assert_eq!(repos.repo(org(1), repo_id(2)).await, Err(RepoStoreError::NotFound), "an invalid outbox job aborts the write");

    repos.upsert_repo(&b, &[]).await.unwrap();
    repos.upsert_repo(&repo(org(1), repo_id(3), 1003, "Acme", "Admin", t0), &[]).await.unwrap();
    repos.upsert_repo(&repo(org(2), repo_id(4), 1001, "acme", "web", t0), &[]).await.unwrap();
    let names: Vec<String> = repos.list_repos(org(1)).await.unwrap().into_iter().map(|r| r.name).collect();
    assert_eq!(names, vec!["Admin", "api", "webapp"], "ordered by lowercase owner, name");

    assert_eq!(repos.set_access(org(1), repo_id(1), RepoAccess::Granted).await, Ok(()));
    assert_eq!(repos.set_access(org(1), repo_id(3), RepoAccess::Lost).await, Ok(()));
    assert_eq!(repos.set_access(org(2), repo_id(1), RepoAccess::Lost).await, Err(RepoStoreError::NotFound));
    let active: BTreeSet<(OrgId, RepoId)> = repos.active_repos_system().await.unwrap().into_iter().collect();
    assert_eq!(active, BTreeSet::from([(org(1), repo_id(1)), (org(1), repo_id(2)), (org(2), repo_id(4))]), "granted repos of every org");
}

async fn version_reasons_are_idempotent_and_singletons_move(clock: &FakeClock, repos: &dyn RepoStore, _: &dyn JobQueue) {
    let (o, r) = (org(1), repo_id(1));
    seed_repo(clock, repos, o, r).await;
    assert!(matches!(repos.add_version_reason(o, repo_id(9), sha(1), VersionReason::DefaultHead).await, Err(RepoStoreError::Invalid(_))));
    assert!(matches!(repos.add_version_reason(org(2), r, sha(1), VersionReason::DefaultHead).await, Err(RepoStoreError::Invalid(_))), "tenant scoped");

    let t0 = clock.now();
    assert_eq!(repos.add_version_reason(o, r, sha(1), VersionReason::DefaultHead).await, Ok(true), "created");
    assert_eq!(repos.add_version_reason(o, r, sha(1), VersionReason::DefaultHead).await, Ok(false), "idempotent");
    let v1 = repos.version(o, key(r, 1)).await.unwrap();
    assert_eq!((v1.org_id, v1.repo_id, v1.sha, v1.created_at), (o, r, sha(1), t0));
    assert_eq!(v1.reasons, BTreeSet::from([VersionReason::DefaultHead]));
    assert!(Layer::ALL.iter().all(|l| v1.layers.get(l) == Some(&LayerState::Pending)), "every layer starts Pending: {:?}", v1.layers);
    assert_eq!((v1.bundle, v1.revs), (None, None));
    assert_eq!(repos.default_head(o, r).await, Ok(Some(sha(1))));
    assert_eq!(repos.version(o, key(r, 7)).await, Err(RepoStoreError::NotFound));

    clock.advance(Duration::from_secs(10));
    let t1 = clock.now();
    assert_eq!(repos.set_default_head(o, r, sha(2)).await, Ok(true));
    assert_eq!(repos.default_head(o, r).await, Ok(Some(sha(2))));
    assert!(repos.version(o, key(r, 1)).await.unwrap().reasons.is_empty(), "the head moved away from sha 1");

    assert_eq!(repos.add_version_reason(o, r, sha(1), VersionReason::Release("v1".into())).await, Ok(false));
    assert_eq!(repos.add_version_reason(o, r, sha(2), VersionReason::Release("v2".into())).await, Ok(false));
    assert_eq!(repos.add_version_reason(o, r, sha(1), VersionReason::Deployed("prod".into())).await, Ok(false));
    assert_eq!(repos.add_version_reason(o, r, sha(1), VersionReason::Deployed("staging".into())).await, Ok(false));
    clock.advance(Duration::from_secs(10));
    let t2 = clock.now();
    let ticket = TicketId::from_uuid(Uuid::from_u128(5));
    assert_eq!(repos.add_version_reason(o, r, sha(2), VersionReason::Deployed("prod".into())).await, Ok(false));
    assert_eq!(repos.add_version_reason(o, r, sha(3), VersionReason::OnDemand(ticket)).await, Ok(true));
    assert_eq!(repos.add_version_reason(o, r, sha(1), VersionReason::OnDemand(ticket)).await, Ok(false), "OnDemand is additive");

    let current = |n: u8| async move { repos.version(o, key(r, n)).await.unwrap().reasons };
    assert_eq!(
        current(1).await,
        BTreeSet::from([VersionReason::Release("v1".into()), VersionReason::Deployed("staging".into()), VersionReason::OnDemand(ticket)])
    );
    assert_eq!(
        current(2).await,
        BTreeSet::from([VersionReason::DefaultHead, VersionReason::Release("v2".into()), VersionReason::Deployed("prod".into())])
    );

    let history = repos.version_reasons(o, r).await.unwrap();
    let find = |s: u8, reason: VersionReason| history.iter().find(|h| h.sha == sha(s) && h.reason == reason).cloned().expect("reason record");
    let head1 = find(1, VersionReason::DefaultHead);
    assert_eq!((head1.added_at, head1.ended_at), (t0, Some(t1)), "a moved singleton records ended_at");
    assert_eq!(find(2, VersionReason::DefaultHead).ended_at, None);
    assert_eq!(find(1, VersionReason::Deployed("prod".into())).ended_at, Some(t2));
    assert_eq!(find(1, VersionReason::Deployed("staging".into())).ended_at, None, "Deployed is singleton per environment");
    assert_eq!(history.len(), 9);
    assert!(history.windows(2).all(|w| w[0].added_at <= w[1].added_at), "ordered by added_at");

    clock.advance(Duration::from_secs(10));
    assert_eq!(repos.set_default_head(o, r, sha(1)).await, Ok(false), "the head returns to an existing version");
    let history = repos.version_reasons(o, r).await.unwrap();
    let heads: Vec<_> = history.iter().filter(|h| h.reason == VersionReason::DefaultHead).collect();
    assert_eq!(heads.len(), 3, "a re-added reason is a new record");
    assert_eq!(heads.iter().filter(|h| h.ended_at.is_none()).map(|h| h.sha).collect::<Vec<_>>(), vec![sha(1)]);

    let shas: Vec<CommitSha> = repos.versions(o, r).await.unwrap().into_iter().map(|v| v.sha).collect();
    assert_eq!(shas, vec![sha(1), sha(2), sha(3)], "ordered by created_at");
    assert!(repos.versions(org(2), r).await.unwrap().is_empty());
}

async fn layer_events_follow_the_state_table_with_outbox(clock: &FakeClock, repos: &dyn RepoStore, jobs: &dyn JobQueue) {
    let (o, r) = (org(1), repo_id(1));
    seed_repo(clock, repos, o, r).await;
    let k = key(r, 1);
    assert_eq!(repos.apply_layer_event(o, k, Layer::Files, LayerEvent::Start, &[]).await, Err(RepoStoreError::NotFound));
    repos.add_version_reason(o, r, sha(1), VersionReason::DefaultHead).await.unwrap();

    assert_eq!(repos.apply_layer_event(o, k, Layer::Files, LayerEvent::Start, &[]).await, Ok(LayerState::Running));
    let invalid = repos.apply_layer_event(o, k, Layer::Symbols, LayerEvent::Complete { at: clock.now() }, &[]).await;
    assert!(matches!(invalid, Err(RepoStoreError::Invalid(_))), "Pending + Complete is not a transition: {invalid:?}");
    assert_eq!(repos.version(o, k).await.unwrap().layer(Layer::Symbols), Some(&LayerState::Pending));

    let bad_outbox = repos.apply_layer_event(o, k, Layer::Files, LayerEvent::Complete { at: clock.now() }, &[invalid_job(o, k)]).await;
    assert!(matches!(bad_outbox, Err(RepoStoreError::Invalid(_))));
    assert_eq!(repos.version(o, k).await.unwrap().layer(Layer::Files), Some(&LayerState::Running), "nothing written");

    let at = clock.now();
    let done = repos.apply_layer_event(o, k, Layer::Files, LayerEvent::Complete { at }, &[error_sites_job(o, k)]).await;
    assert_eq!(done, Ok(LayerState::Ready { at }));
    let leased = leased_kinds(jobs, JobLane::Indexer).await;
    assert_eq!(leased.iter().map(|l| l.kind).collect::<Vec<_>>(), vec![JobKind::IndexErrorSites], "jobs commit with the event");

    let skipped = repos.apply_layer_event(o, k, Layer::Semantic, LayerEvent::Skip { reason: "disabled".into() }, &[]).await;
    assert_eq!(skipped, Ok(LayerState::Skipped { reason: "disabled".into() }));
    assert_eq!(repos.apply_layer_event(o, k, Layer::Semantic, LayerEvent::Reset, &[]).await, Ok(LayerState::Pending));
    let failed = async {
        repos.apply_layer_event(o, k, Layer::History, LayerEvent::Start, &[]).await?;
        repos.apply_layer_event(o, k, Layer::History, LayerEvent::Fail { reason: "dead".into() }, &[]).await
    };
    assert_eq!(failed.await, Ok(LayerState::Failed { reason: "dead".into() }));
    assert_eq!(repos.apply_layer_event(org(2), k, Layer::Files, LayerEvent::Reset, &[]).await, Err(RepoStoreError::NotFound), "tenant scoped");
}

async fn layer_batches_count_once_and_fire_on_last_once(clock: &FakeClock, repos: &dyn RepoStore, jobs: &dyn JobQueue) {
    let (o, r) = (org(1), repo_id(1));
    seed_repo(clock, repos, o, r).await;
    let k = key(r, 1);
    repos.add_version_reason(o, r, sha(1), VersionReason::DefaultHead).await.unwrap();
    assert!(matches!(repos.begin_layer_batches(o, k, Layer::Symbols, 0, &[]).await, Err(RepoStoreError::Invalid(_))));

    let parse = |batch: u32| {
        NewJob::new(JobPayload::IndexParse { org_id: o, repo_id: r, sha: sha(1), prio: IndexPriority::Normal, batch, oids: vec![] })
    };
    repos.begin_layer_batches(o, k, Layer::Symbols, 3, &[parse(1), parse(2), parse(3)]).await.unwrap();
    assert_eq!(repos.version(o, k).await.unwrap().layer(Layer::Symbols), Some(&LayerState::Running), "begin starts the layer");
    assert_eq!(leased_kinds(jobs, JobLane::Indexer).await.len(), 3, "batch jobs enqueued with begin");

    let on_last = [error_sites_job(o, k)];
    assert_eq!(repos.finish_layer_batch(o, k, Layer::Symbols, 1, &on_last).await, Ok(2));
    assert_eq!(repos.finish_layer_batch(o, k, Layer::Symbols, 1, &on_last).await, Ok(2), "a repeated batch counts once");
    assert_eq!(repos.finish_layer_batch(o, k, Layer::Symbols, 4, &on_last).await, Err(RepoStoreError::NotFound));
    assert_eq!(repos.finish_layer_batch(o, k, Layer::Symbols, 2, &on_last).await, Ok(1));
    assert!(leased_kinds(jobs, JobLane::Indexer).await.is_empty(), "on_last waits for the last batch");

    clock.advance(Duration::from_secs(5));
    assert_eq!(repos.finish_layer_batch(o, k, Layer::Symbols, 3, &on_last).await, Ok(0));
    assert_eq!(repos.version(o, k).await.unwrap().layer(Layer::Symbols), Some(&LayerState::Ready { at: clock.now() }));
    let fired = leased_kinds(jobs, JobLane::Indexer).await;
    assert_eq!(fired.iter().map(|l| l.kind).collect::<Vec<_>>(), vec![JobKind::IndexErrorSites]);
    jobs.complete(&fired[0].lease, &[]).await.unwrap();

    assert_eq!(repos.finish_layer_batch(o, k, Layer::Symbols, 3, &on_last).await, Ok(0));
    assert!(leased_kinds(jobs, JobLane::Indexer).await.is_empty(), "on_last is enqueued exactly once");
    assert_eq!(repos.finish_layer_batch(org(2), k, Layer::Symbols, 3, &on_last).await, Err(RepoStoreError::NotFound));
}

async fn rebuild_replaces_batches_and_keeps_ready(clock: &FakeClock, repos: &dyn RepoStore, jobs: &dyn JobQueue) {
    let (o, r) = (org(1), repo_id(1));
    seed_repo(clock, repos, o, r).await;
    let k = key(r, 1);
    repos.add_version_reason(o, r, sha(1), VersionReason::DefaultHead).await.unwrap();
    let on_last = [error_sites_job(o, k)];
    let symbols = || async move { repos.version(o, k).await.unwrap().layer(Layer::Symbols).cloned() };

    // An interrupted build: 3 batches, one done; the retried manifest job plans 2 batches.
    repos.begin_layer_batches(o, k, Layer::Symbols, 3, &[]).await.unwrap();
    assert_eq!(repos.finish_layer_batch(o, k, Layer::Symbols, 1, &on_last).await, Ok(2));
    repos.begin_layer_batches(o, k, Layer::Symbols, 2, &[]).await.unwrap();
    assert_eq!(symbols().await, Some(LayerState::Running));
    assert_eq!(repos.finish_layer_batch(o, k, Layer::Symbols, 3, &on_last).await, Err(RepoStoreError::NotFound), "the old batch set is gone");
    assert_eq!(repos.finish_layer_batch(o, k, Layer::Symbols, 1, &on_last).await, Ok(1), "batch 1 is open again in the new set");
    clock.advance(Duration::from_secs(5));
    assert_eq!(repos.finish_layer_batch(o, k, Layer::Symbols, 2, &on_last).await, Ok(0));
    let first_ready = LayerState::Ready { at: clock.now() };
    assert_eq!(symbols().await, Some(first_ready.clone()));
    let fired = leased_kinds(jobs, JobLane::Indexer).await;
    assert_eq!(fired.iter().map(|l| l.kind).collect::<Vec<_>>(), vec![JobKind::IndexErrorSites]);
    jobs.complete(&fired[0].lease, &[]).await.unwrap();

    // A second build of the same version (shadow rebuild at a new rev): Ready keeps serving.
    clock.advance(Duration::from_secs(60));
    repos.begin_layer_batches(o, k, Layer::Symbols, 2, &[]).await.unwrap();
    assert_eq!(symbols().await, Some(first_ready.clone()), "Ready stays Ready while rebuilding");
    assert_eq!(repos.finish_layer_batch(o, k, Layer::Symbols, 1, &on_last).await, Ok(1), "the rebuild has fresh open batches");
    assert_eq!(symbols().await, Some(first_ready));
    clock.advance(Duration::from_secs(5));
    assert_eq!(repos.finish_layer_batch(o, k, Layer::Symbols, 2, &on_last).await, Ok(0));
    assert_eq!(symbols().await, Some(LayerState::Ready { at: clock.now() }), "the rebuild's last batch completes the layer again");
    let fired = leased_kinds(jobs, JobLane::Indexer).await;
    assert_eq!(fired.iter().map(|l| l.kind).collect::<Vec<_>>(), vec![JobKind::IndexErrorSites], "the second build fans in too");

    // Start still applies to every other state: a Failed layer must be Reset first.
    repos.apply_layer_event(o, k, Layer::History, LayerEvent::Start, &[]).await.unwrap();
    repos.apply_layer_event(o, k, Layer::History, LayerEvent::Fail { reason: "dead".into() }, &[]).await.unwrap();
    let begin_failed = repos.begin_layer_batches(o, k, Layer::History, 1, &[]).await;
    assert!(matches!(begin_failed, Err(RepoStoreError::Invalid(_))), "{begin_failed:?}");
    assert_eq!(repos.finish_layer_batch(o, k, Layer::History, 1, &[]).await, Err(RepoStoreError::NotFound), "a rejected begin creates no batches");
}

async fn revs_and_bundles_with_reuse_by_tree(clock: &FakeClock, repos: &dyn RepoStore, jobs: &dyn JobQueue) {
    let (o, r) = (org(1), repo_id(1));
    seed_repo(clock, repos, o, r).await;
    seed_repo(clock, repos, o, repo_id(2)).await;
    for n in [1, 2] {
        repos.add_version_reason(o, r, sha(n), VersionReason::Release(format!("v{n}"))).await.unwrap();
    }
    repos.add_version_reason(o, repo_id(2), sha(1), VersionReason::DefaultHead).await.unwrap();

    let revs = IndexRevs { parser: ParserRev(3), errors: ErrorsRev(2), appmap: AppMapRev(1) };
    assert_eq!(repos.set_index_revs(o, key(r, 9), &revs).await, Err(RepoStoreError::NotFound));
    repos.set_index_revs(o, key(r, 1), &revs).await.unwrap();
    assert_eq!(repos.version(o, key(r, 1)).await.unwrap().revs, Some(revs));

    let tree = TreeOid::from_bytes([0xee; 20]);
    let bundle = SourceBundleRef { key: format!("org/x/repos/y/bundles/{tree}.tar.zst"), tree, bytes: 1234, sha256: [9; 32] };
    let ready = NewJob::new(JobPayload::RunsSourceReady { org_id: o, repo_id: r, sha: sha(1) });
    assert_eq!(repos.bundle_for_tree(o, r, tree).await, Ok(None));
    repos.set_source_bundle(o, key(r, 1), &bundle, &[ready]).await.unwrap();
    assert_eq!(repos.version(o, key(r, 1)).await.unwrap().bundle, Some(bundle.clone()));
    let leased = leased_kinds(jobs, JobLane::Server).await;
    assert_eq!(leased.iter().map(|l| l.kind).collect::<Vec<_>>(), vec![JobKind::RunsSourceReady], "set_source_bundle outbox");

    assert_eq!(repos.bundle_for_tree(o, r, tree).await, Ok(Some(bundle.clone())), "identical trees share one bundle");
    assert_eq!(repos.bundle_for_tree(o, repo_id(2), tree).await, Ok(None), "per repo");
    assert_eq!(repos.bundle_for_tree(org(2), r, tree).await, Ok(None), "per org");
    assert_eq!(repos.bundle_for_tree(o, r, TreeOid::from_bytes([1; 20])).await, Ok(None));
    assert_eq!(repos.set_source_bundle(o, key(r, 9), &bundle, &[]).await, Err(RepoStoreError::NotFound));
}

async fn delete_version_removes_reasons_layers_and_batches(clock: &FakeClock, repos: &dyn RepoStore, _: &dyn JobQueue) {
    let (o, r) = (org(1), repo_id(1));
    seed_repo(clock, repos, o, r).await;
    let k = key(r, 1);
    repos.add_version_reason(o, r, sha(1), VersionReason::DefaultHead).await.unwrap();
    repos.add_version_reason(o, r, sha(2), VersionReason::Release("v2".into())).await.unwrap();
    repos.begin_layer_batches(o, k, Layer::Symbols, 2, &[]).await.unwrap();

    repos.delete_version(o, k).await.unwrap();
    assert_eq!(repos.version(o, k).await, Err(RepoStoreError::NotFound));
    assert_eq!(repos.default_head(o, r).await, Ok(None));
    assert!(repos.version_reasons(o, r).await.unwrap().iter().all(|h| h.sha != sha(1)));
    assert_eq!(repos.delete_version(o, k).await, Ok(()), "deleting an absent version is Ok");
    assert_eq!(repos.versions(o, r).await.unwrap().len(), 1);

    assert_eq!(repos.add_version_reason(o, r, sha(1), VersionReason::DefaultHead).await, Ok(true), "a fresh version");
    let fresh = repos.version(o, k).await.unwrap();
    assert_eq!(fresh.layer(Layer::Symbols), Some(&LayerState::Pending));
    assert_eq!(repos.finish_layer_batch(o, k, Layer::Symbols, 1, &[]).await, Err(RepoStoreError::NotFound), "old batches are gone");
}

fn recipe(o: OrgId, r: RepoId, n: u8, status: RecipeStatus) -> Recipe {
    Recipe::rehydrate(RecipeData {
        org_id: o,
        repo_id: r,
        hash: RecipeHash::from_bytes([n; 32]),
        body: format!("version = 1 # {n}"),
        validated_at_sha: None,
        status,
    })
}

async fn recipes_insert_once_and_latest_prefers_validated(clock: &FakeClock, repos: &dyn RepoStore, jobs: &dyn JobQueue) {
    let (o, r) = (org(1), repo_id(1));
    seed_repo(clock, repos, o, r).await;
    assert_eq!(repos.latest_recipe(o, r).await, Ok(None));
    assert!(matches!(repos.put_recipe(o, &recipe(org(2), r, 1, RecipeStatus::Draft)).await, Err(RepoStoreError::Invalid(_))));
    assert!(matches!(repos.put_recipe(o, &recipe(o, repo_id(9), 1, RecipeStatus::Draft)).await, Err(RepoStoreError::Invalid(_))));

    let d1 = recipe(o, r, 1, RecipeStatus::Draft);
    assert_eq!(repos.put_recipe(o, &d1).await, Ok(true));
    assert_eq!(repos.put_recipe(o, &d1).await, Ok(false), "insert-if-absent by hash");
    clock.advance(Duration::from_secs(10));
    let d2 = recipe(o, r, 2, RecipeStatus::Draft);
    repos.put_recipe(o, &d2).await.unwrap();
    clock.advance(Duration::from_secs(10));
    let d3 = recipe(o, r, 3, RecipeStatus::Draft);
    repos.put_recipe(o, &d3).await.unwrap();
    assert_eq!(repos.get_recipe(o, r, d2.hash).await, Ok(d2.clone()));
    assert_eq!(repos.get_recipe(o, r, RecipeHash::from_bytes([7; 32])).await, Err(RepoStoreError::NotFound));
    assert_eq!(repos.get_recipe(org(2), r, d2.hash).await, Err(RepoStoreError::NotFound));
    assert_eq!(repos.latest_recipe(o, r).await, Ok(Some(d3.clone())), "no validated recipe: the latest draft");

    let validated_at = clock.now();
    let v1 = Recipe::rehydrate(RecipeData {
        status: RecipeStatus::Validated { at: validated_at },
        validated_at_sha: Some(sha(4)),
        ..d1.clone().into_data()
    });
    let job = NewJob::new(JobPayload::IndexVersion { org_id: o, repo_id: r, sha: sha(4), reason: VersionReason::DefaultHead });
    assert_eq!(repos.update_recipe(o, &v1, &RecipeStatus::Draft, &[job]).await, Ok(()));
    assert_eq!(repos.get_recipe(o, r, d1.hash).await, Ok(v1.clone()));
    assert_eq!(leased_kinds(jobs, JobLane::Indexer).await.len(), 1, "update_recipe outbox");
    assert_eq!(repos.update_recipe(o, &v1, &RecipeStatus::Draft, &[]).await, Err(RepoStoreError::Conflict), "CAS on the stored status");
    assert_eq!(repos.update_recipe(o, &recipe(o, r, 8, RecipeStatus::Draft), &RecipeStatus::Draft, &[]).await, Err(RepoStoreError::NotFound));
    assert_eq!(repos.latest_recipe(o, r).await, Ok(Some(v1.clone())), "a validated recipe beats newer drafts");

    clock.advance(Duration::from_secs(10));
    let broken = Recipe::rehydrate(RecipeData { status: RecipeStatus::Broken { reason: "boot".into(), at: clock.now() }, ..v1.clone().into_data() });
    repos.update_recipe(o, &broken, &v1.status, &[]).await.unwrap();
    assert_eq!(repos.latest_recipe(o, r).await, Ok(Some(d3.clone())), "broken recipes are never latest");

    let first = repos.list_recipes(o, r, PageRequest::first(2)).await.unwrap();
    assert_eq!(first.items.iter().map(|x| x.hash).collect::<Vec<_>>(), vec![d3.hash, d2.hash], "newest inserted first");
    let second = repos.list_recipes(o, r, PageRequest::first(2).after(first.next.expect("next page"))).await.unwrap();
    assert_eq!((second.items.iter().map(|x| x.hash).collect::<Vec<_>>(), second.next), (vec![d1.hash], None));
}

async fn deployments_are_an_append_only_history(clock: &FakeClock, repos: &dyn RepoStore, _: &dyn JobQueue) {
    let (o, r) = (org(1), repo_id(1));
    seed_repo(clock, repos, o, r).await;
    let t0 = clock.now();
    let at = |secs: u64| t0 + Duration::from_secs(secs);
    let deployment = |env: &str, n: u8, secs: u64| Deployment {
        org_id: o,
        repo_id: r,
        environment: env.into(),
        sha: sha(n),
        observed_at: at(secs),
        source: DeploymentSource::GitHubDeployments,
    };
    let first = deployment("prod", 1, 100);
    let second = deployment("prod", 2, 200);
    assert_eq!(repos.record_deployment(o, &first).await, Ok(true));
    assert_eq!(repos.record_deployment(o, &first).await, Ok(false), "same (repo, env, observed_at) recorded once");
    repos.record_deployment(o, &second).await.unwrap();
    repos.record_deployment(o, &deployment("staging", 3, 150)).await.unwrap();
    assert!(matches!(repos.record_deployment(o, &Deployment { repo_id: repo_id(9), ..first.clone() }).await, Err(RepoStoreError::Invalid(_))));
    assert!(matches!(repos.record_deployment(org(2), &first).await, Err(RepoStoreError::Invalid(_))));

    assert_eq!(repos.latest_deployment(o, r, "prod", at(50)).await, Ok(None), "before the first observation");
    assert_eq!(repos.latest_deployment(o, r, "prod", at(100)).await, Ok(Some(first.clone())), "as_of is inclusive");
    assert_eq!(repos.latest_deployment(o, r, "prod", at(150)).await, Ok(Some(first.clone())), "between");
    assert_eq!(repos.latest_deployment(o, r, "prod", at(10_000)).await, Ok(Some(second.clone())), "after");
    assert_eq!(repos.latest_deployment(o, r, "dev", at(10_000)).await, Ok(None));
    assert_eq!(repos.latest_deployment(org(2), r, "prod", at(10_000)).await, Ok(None));

    let first_page = repos.list_deployments(o, r, PageRequest::first(2)).await.unwrap();
    let envs: Vec<(String, u8)> = first_page.items.iter().map(|d| (d.environment.clone(), d.sha.0[0])).collect();
    assert_eq!(envs, vec![("prod".into(), 2), ("staging".into(), 3)], "newest observation first");
    let rest = repos.list_deployments(o, r, PageRequest::first(2).after(first_page.next.expect("next page"))).await.unwrap();
    assert_eq!((rest.items, rest.next), (vec![first], None));
}
