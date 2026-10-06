//! `RunnerState` contract suite (executor.md "Testing"): `claim_base` single-flight,
//! `lease_ports` uniqueness under concurrent callers, quarantine respected, `remove_base` conflict
//! with live forks, resources cascade on `remove_fork`, `recovery_snapshot` after a simulated
//! crash (rows left in each table).

use std::collections::BTreeSet;
use std::sync::Arc;

use mage_core::domain::{CommitSha, OrgId, RepoId, RunId, RunKind};
use time::OffsetDateTime;
use time::macros::datetime;
use uuid::Uuid;

use crate::ports::executor::{Arch, BaseKey, BaseKind, EnvId, ExecutorKind, ForkMode, ForkPurpose, PortName, ProcessRole};
use crate::ports::runner_state::{
    ActiveRun, BaseClaim, BaseMeta, BaseStatus, EnvResource, ForkRecord, ForkStatus, PortOwner, PortRange, ProcessRecord, RunPhase,
    RunnerState, RunnerStateError,
};

fn key(n: u8) -> BaseKey {
    BaseKey {
        org_id: OrgId::from_uuid(Uuid::from_u128(1)),
        repo_id: RepoId::from_uuid(Uuid::from_u128(2)),
        sha: CommitSha([n; 20]),
        recipe_hash: None,
        arch: Arch::Aarch64,
        executor: ExecutorKind::Host,
    }
}

fn env(n: u128) -> EnvId {
    EnvId::from_uuid(Uuid::from_u128(0x1000 + n))
}

fn run_id(n: u128) -> RunId {
    RunId::from_uuid(Uuid::from_u128(0x2000 + n))
}

const T0: OffsetDateTime = datetime!(2026-01-01 00:00 UTC);

fn meta(dir: &str) -> BaseMeta {
    BaseMeta { kind: BaseKind::SourceOnly, dir: dir.to_owned(), bytes: 1234, fork_mode: ForkMode::Copy, template_db: None }
}

fn fork(n: u128, base: Option<BaseKey>) -> ForkRecord {
    ForkRecord {
        env: env(n),
        base,
        run_id: run_id(n),
        purpose: ForkPurpose::Verify,
        dir: format!("/data/forks/{n}"),
        status: ForkStatus::Creating,
        created_at: T0,
        resources: vec![EnvResource::new("dir", format!("/data/forks/{n}"))],
    }
}

fn active(n: u128) -> ActiveRun {
    ActiveRun {
        run_id: run_id(n),
        org_id: OrgId::from_uuid(Uuid::from_u128(1)),
        kind: RunKind::Diagnose,
        attempt: 1,
        phase: RunPhase::Leased,
        env: None,
        started_at: T0,
    }
}

/// Runs every case, each against a fresh state from `make`.
pub async fn run<F, Fut>(make: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Arc<dyn RunnerState>>,
{
    runs_lifecycle(make().await).await;
    claim_base_is_single_flight(make().await).await;
    base_lifecycle(make().await).await;
    remove_base_conflicts_with_live_forks(make().await).await;
    resources_cascade_on_remove_fork(make().await).await;
    lease_ports_unique_under_concurrency(make().await).await;
    quarantine_is_respected(make().await).await;
    pool_exhaustion_is_unavailable(make().await).await;
    processes_round_trip(make().await).await;
}

async fn runs_lifecycle(state: Arc<dyn RunnerState>) {
    state.begin_run(active(1)).await.unwrap();
    assert!(matches!(state.begin_run(active(1)).await, Err(RunnerStateError::Conflict(_))));
    state.set_phase(run_id(1), RunPhase::Running).await.unwrap();
    state.attach_env(run_id(1), env(1)).await.unwrap();
    let runs = state.active_runs().await.unwrap();
    assert_eq!(runs, vec![ActiveRun { phase: RunPhase::Running, env: Some(env(1)), ..active(1) }]);
    assert_eq!(state.set_phase(run_id(2), RunPhase::Running).await, Err(RunnerStateError::NotFound));
    state.end_run(run_id(1)).await.unwrap();
    state.end_run(run_id(1)).await.unwrap();
    assert!(state.active_runs().await.unwrap().is_empty());
}

async fn claim_base_is_single_flight(state: Arc<dyn RunnerState>) {
    let k = key(1);
    let claims = futures::future::join_all((0..8).map(|i| {
        let state = Arc::clone(&state);
        async move { state.claim_base(&k, &format!("/data/bases/x.staging-{i}")).await.unwrap() }
    }))
    .await;
    let owned = claims.iter().filter(|c| matches!(c, BaseClaim::Owned)).count();
    let in_progress = claims.iter().filter(|c| matches!(c, BaseClaim::InProgress)).count();
    assert_eq!((owned, in_progress), (1, 7), "exactly one claimant owns the prepare: {claims:?}");
}

async fn base_lifecycle(state: Arc<dyn RunnerState>) {
    let k = key(2);
    assert_eq!(state.claim_base(&k, "/s").await.unwrap(), BaseClaim::Owned);
    let preparing = state.base(&k).await.unwrap().unwrap();
    assert_eq!((preparing.status, preparing.dir.as_str()), (BaseStatus::Preparing, "/s"));
    state.base_ready(&k, meta("/data/bases/b")).await.unwrap();
    let BaseClaim::Ready(rec) = state.claim_base(&k, "/other").await.unwrap() else { panic!("expected Ready") };
    assert_eq!((rec.status, rec.dir.as_str(), rec.bytes, rec.fork_mode), (BaseStatus::Ready, "/data/bases/b", 1234, Some(ForkMode::Copy)));
    assert_eq!(rec.id, k.id());
    let later = T0 + time::Duration::days(400);
    state.touch_base(&k, later).await.unwrap();
    assert_eq!(state.base(&k).await.unwrap().unwrap().last_used_at, later);
    // Abandon only drops Preparing bases.
    state.base_abandon(&k).await.unwrap();
    assert!(state.base(&k).await.unwrap().is_some());
    let k3 = key(3);
    state.claim_base(&k3, "/s3").await.unwrap();
    state.base_abandon(&k3).await.unwrap();
    assert!(state.base(&k3).await.unwrap().is_none());
    assert_eq!(state.claim_base(&k3, "/s3").await.unwrap(), BaseClaim::Owned, "an abandoned base can be claimed again");
    assert_eq!(state.base_ready(&key(9), meta("/x")).await, Err(RunnerStateError::NotFound));
    assert_eq!(state.bases().await.unwrap().len(), 2);
}

async fn remove_base_conflicts_with_live_forks(state: Arc<dyn RunnerState>) {
    let k = key(4);
    state.claim_base(&k, "/s").await.unwrap();
    state.base_ready(&k, meta("/b")).await.unwrap();
    state.begin_fork(fork(1, Some(k))).await.unwrap();
    state.begin_fork(fork(2, Some(k))).await.unwrap();
    assert_eq!(state.base(&k).await.unwrap().unwrap().live_forks, 2);
    assert!(matches!(state.remove_base(&k).await, Err(RunnerStateError::Conflict(_))));
    state.remove_fork(env(1)).await.unwrap();
    state.remove_fork(env(2)).await.unwrap();
    assert_eq!(state.base(&k).await.unwrap().unwrap().live_forks, 0);
    state.remove_base(&k).await.unwrap();
    assert!(state.base(&k).await.unwrap().is_none());
    assert_eq!(state.begin_fork(fork(3, Some(k))).await, Err(RunnerStateError::NotFound), "a fork needs its base");
}

async fn resources_cascade_on_remove_fork(state: Arc<dyn RunnerState>) {
    state.begin_fork(fork(5, None)).await.unwrap();
    assert!(matches!(state.begin_fork(fork(5, None)).await, Err(RunnerStateError::Conflict(_))));
    state.add_resource(env(5), EnvResource::new("port", "20001")).await.unwrap();
    state.add_resource(env(5), EnvResource::new("port", "20001")).await.unwrap();
    state.add_resource(env(5), EnvResource::new("pg_db", "mf_x")).await.unwrap();
    state.remove_resource(env(5), &EnvResource::new("dir", "/data/forks/5")).await.unwrap();
    state.set_fork_status(env(5), ForkStatus::Destroying).await.unwrap();
    let forks = state.forks().await.unwrap();
    assert_eq!(forks.len(), 1);
    assert_eq!(forks[0].status, ForkStatus::Destroying);
    let kinds: BTreeSet<(String, String)> = forks[0].resources.iter().map(|r| (r.kind.clone(), r.value.clone())).collect();
    assert_eq!(kinds, BTreeSet::from([("pg_db".into(), "mf_x".into()), ("port".into(), "20001".into())]));
    assert_eq!(state.add_resource(env(6), EnvResource::new("dir", "/x")).await, Err(RunnerStateError::NotFound));
    state.remove_fork(env(5)).await.unwrap();
    assert!(state.forks().await.unwrap().is_empty());
    // A new fork with the same id starts without the old resources.
    state.begin_fork(ForkRecord { resources: vec![], ..fork(5, None) }).await.unwrap();
    assert!(state.forks().await.unwrap()[0].resources.is_empty());
}

async fn lease_ports_unique_under_concurrency(state: Arc<dyn RunnerState>) {
    let pool = PortRange { start: 30000, end: 30099 };
    let leases = futures::future::join_all((0..10u128).map(|i| {
        let state = Arc::clone(&state);
        async move {
            let names = [Some(PortName::new("api").unwrap()), Some(PortName::new("web").unwrap()), None];
            state.lease_ports(&PortOwner::Env(env(i)), &names, pool).await.unwrap()
        }
    }))
    .await;
    let ports: Vec<u16> = leases.iter().flatten().map(|l| l.port).collect();
    let unique: BTreeSet<u16> = ports.iter().copied().collect();
    assert_eq!(ports.len(), 30);
    assert_eq!(unique.len(), 30, "leases overlap: {ports:?}");
    assert!(ports.iter().all(|p| (30000..=30099).contains(p)));
    assert_eq!(unique.iter().next_back().copied(), Some(30029), "lowest free ports first");
    state.release_ports(&PortOwner::Env(env(0))).await.unwrap();
    assert_eq!(state.port_leases().await.unwrap().len(), 27);
    let again = state.lease_ports(&PortOwner::RunMcp(run_id(1)), &[None], pool).await.unwrap();
    assert!(leases[0].iter().any(|l| l.port == again[0].port), "released ports are reused lowest-first");
    assert_eq!(again[0].owner, PortOwner::RunMcp(run_id(1)));
}

async fn quarantine_is_respected(state: Arc<dyn RunnerState>) {
    let pool = PortRange { start: 31000, end: 31002 };
    let owner = PortOwner::Base(key(1).id());
    let first = state.lease_ports(&owner, &[None], pool).await.unwrap();
    assert_eq!(first[0].port, 31000);
    let far = OffsetDateTime::now_utc() + time::Duration::minutes(10);
    state.quarantine_port(31000, far).await.unwrap();
    assert!(state.port_leases().await.unwrap().iter().all(|l| l.port != 31000), "quarantine drops the lease");
    let next = state.lease_ports(&owner, &[None, None], pool).await.unwrap();
    assert_eq!(next.iter().map(|l| l.port).collect::<Vec<_>>(), vec![31001, 31002]);
    assert!(matches!(state.lease_ports(&owner, &[None], pool).await, Err(RunnerStateError::Unavailable(_))));
    // An expired quarantine frees the port again.
    state.quarantine_port(31001, OffsetDateTime::now_utc() - time::Duration::minutes(1)).await.unwrap();
    let reused = state.lease_ports(&owner, &[None], pool).await.unwrap();
    assert_eq!(reused[0].port, 31001);
}

async fn pool_exhaustion_is_unavailable(state: Arc<dyn RunnerState>) {
    let pool = PortRange { start: 32000, end: 32001 };
    let err = state.lease_ports(&PortOwner::Env(env(1)), &[None, None, None], pool).await.unwrap_err();
    assert!(matches!(err, RunnerStateError::Unavailable(_)));
    assert!(state.port_leases().await.unwrap().is_empty(), "a failed lease inserts nothing");
    assert!(matches!(
        state.lease_ports(&PortOwner::Env(env(1)), &[None], PortRange { start: 2, end: 1 }).await,
        Err(RunnerStateError::Invalid(_))
    ));
}

async fn processes_round_trip(state: Arc<dyn RunnerState>) {
    let p = ProcessRecord {
        pgid: 4242,
        pid: 4242,
        start_time: 123_456_789,
        boot_id: "boot".into(),
        run_id: Some(run_id(1)),
        env: Some(env(1)),
        role: ProcessRole::Service("api".into()),
        cgroup: Some("/sys/fs/cgroup/mage/env-x/svc-api".into()),
    };
    state.record_process(p.clone()).await.unwrap();
    assert_eq!(state.processes().await.unwrap(), vec![p]);
    state.clear_process(4242).await.unwrap();
    assert!(state.processes().await.unwrap().is_empty());
}

/// `open` returns the state over the same storage on every call. Rows are written through one
/// instance, which is then dropped without cleanup (a crash); a second instance must report all of
/// them in `recovery_snapshot`.
pub async fn recovery_after_restart<F, Fut>(open: F)
where
    F: Fn() -> Fut,
    Fut: Future<Output = Arc<dyn RunnerState>>,
{
    let k = key(7);
    let staging = key(8);
    {
        let state = open().await;
        state.begin_run(active(1)).await.unwrap();
        state.set_phase(run_id(1), RunPhase::AwaitingGate).await.unwrap();
        state
            .record_process(ProcessRecord {
                pgid: 77,
                pid: 77,
                start_time: 1,
                boot_id: "b".into(),
                run_id: Some(run_id(1)),
                env: Some(env(1)),
                role: ProcessRole::Harness,
                cgroup: None,
            })
            .await
            .unwrap();
        state.claim_base(&k, "/s").await.unwrap();
        state.base_ready(&k, meta("/b")).await.unwrap();
        state.claim_base(&staging, "/data/bases/x.staging-1").await.unwrap();
        state.begin_fork(fork(1, Some(k))).await.unwrap();
        state.add_resource(env(1), EnvResource::new("pg_db", "mf_1")).await.unwrap();
        state
            .lease_ports(&PortOwner::Env(env(1)), &[Some(PortName::new("api").unwrap())], PortRange { start: 33000, end: 33010 })
            .await
            .unwrap();
    }
    let state = open().await;
    let snap = state.recovery_snapshot().await.unwrap();
    assert_eq!(snap.runs.len(), 1);
    assert_eq!(snap.runs[0].phase, RunPhase::AwaitingGate);
    assert_eq!(snap.processes.iter().map(|p| p.pgid).collect::<Vec<_>>(), vec![77]);
    assert_eq!(snap.forks.len(), 1);
    assert_eq!(snap.forks[0].base, Some(k));
    assert_eq!(snap.forks[0].resources.len(), 2);
    assert_eq!(snap.preparing_bases.iter().map(|b| b.key).collect::<Vec<_>>(), vec![staging], "only Preparing bases");
    assert_eq!(snap.ports.len(), 1);
    assert_eq!(snap.ports[0].name.as_ref().map(PortName::as_str), Some("api"));
}
