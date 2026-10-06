//! `exec_host` against the executor contract suite, plus recovery after a simulated crash.
//!
//! - Dev mode runs everywhere (needs `/bin/sh` and `git`).
//! - Database cases: set `MAGE_TEST_RUNNER_PG_URL` to a superuser URL of a disposable Postgres ≥ 15
//!   (e.g. `docker run -d --rm -p 55432:5432 -e POSTGRES_PASSWORD=pw postgres:16` and
//!   `postgres://postgres:pw@127.0.0.1:55432/postgres`).
//! - Enforced mode (Linux runner VM provisioned per executor.md, run as `mage`): set
//!   `MAGE_TEST_ENFORCED=1` and `MAGE_TEST_ENFORCED_DATA_DIR` (a dir on the reflink data disk).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;

use crate::app::runner::recover::recover;
use crate::ports::executor::*;
use crate::ports::runner_state::{BaseClaim, RunnerState};
use crate::testing::contract::executor::{self as contract, ExecutorFixture};
use crate::testing::exec_host::DevHost;

struct HostFixture {
    host: DevHost,
    database: bool,
}

#[async_trait]
impl ExecutorFixture for HostFixture {
    fn executor(&self) -> Arc<dyn Executor> {
        self.host.executor.clone()
    }

    async fn bundle(&self, files: &[(&str, &[u8])]) -> BundleSource {
        self.host.publish(files)
    }

    async fn symlink(&self, env: &dyn Env, path: &EnvPath, target: &str) {
        let root = match path.area {
            Area::Workspace => env.root(),
            Area::Scratch => env.scratch_root(),
        };
        let link = Path::new(root).join(path.rel.as_str());
        if let Some(parent) = link.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::os::unix::fs::symlink(target, link).unwrap();
    }

    fn processes(&self) -> bool {
        true
    }

    fn database(&self) -> bool {
        self.database
    }
}

/// Gone, or a zombie nobody reaps (the test leaked the child handle on purpose).
fn dead(pid: u32) -> bool {
    let out = std::process::Command::new("ps").args(["-o", "stat=", "-p", &pid.to_string()]).output().unwrap();
    let stat = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    stat.is_empty() || stat.starts_with('Z')
}

fn git_available() -> bool {
    let ok = std::process::Command::new("git").arg("--version").output().is_ok_and(|o| o.status.success());
    if !ok {
        println!("skipped: git not on PATH");
    }
    ok
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dev_host_contract_core() {
    if !git_available() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let fx = HostFixture { host: DevHost::start(&tmp.path().join("runner")).await.unwrap(), database: false };
    contract::run_core(&fx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dev_host_contract_processes() {
    if !git_available() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let fx = HostFixture { host: DevHost::start(&tmp.path().join("runner")).await.unwrap(), database: false };
    contract::run_processes(&fx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dev_host_contract_database() {
    let Ok(url) = std::env::var("MAGE_TEST_RUNNER_PG_URL") else {
        println!("skipped: MAGE_TEST_RUNNER_PG_URL not set");
        return;
    };
    if !git_available() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let fx = HostFixture { host: DevHost::start_with_pg(&tmp.path().join("runner"), &url).await.unwrap(), database: true };
    contract::run_database(&fx).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dev_mode_env_is_plain_and_exec_records_processes() {
    if !git_available() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let host = DevHost::start(&tmp.path().join("runner")).await.unwrap();
    let env = host
        .executor
        .empty_env(ForkOptions { run_id: mage_core::domain::RunId::from_uuid(uuid::Uuid::now_v7()), purpose: ForkPurpose::Triage, egress: EgressProfile::ServerOnly })
        .await
        .unwrap();
    assert_eq!(env.isolation(), IsolationMode::Dev);
    assert!(env.root().starts_with(tmp.path().to_str().unwrap()), "dev mode uses real paths");
    let mut spec = ExecSpec::shell("echo \"$HOME|$TMPDIR|$CI|$LANG|${HTTPS_PROXY:-none}\"", ProcessRole::Exec);
    spec.cwd = EnvPath::scratch(".").unwrap();
    let out = env.exec(spec, None).await.unwrap();
    let line = String::from_utf8_lossy(&out.stdout.head).trim().to_owned();
    let fields: Vec<&str> = line.split('|').collect();
    assert!(fields[0].ends_with("/home") && fields[1].ends_with("/tmp"), "{line}");
    assert_eq!(&fields[2..], &["1", "C.UTF-8", "none"], "egress proxy is off in the dev profile");
    // While a child runs, its process group is recorded; after wait it is cleared.
    let mut child = env
        .spawn(SpawnSpec { argv: vec!["sleep".into(), "30".into()], cwd: EnvPath::scratch(".").unwrap(), env: vec![], role: ProcessRole::Harness })
        .await
        .unwrap();
    let recorded = host.state.processes().await.unwrap();
    assert!(recorded.iter().any(|p| p.pid as u32 == child.pid() && p.env == Some(env.id())), "{recorded:?}");
    child.terminate(Duration::from_secs(2)).await.unwrap();
    assert!(host.state.processes().await.unwrap().iter().all(|p| p.pid as u32 != child.pid()));
    env.destroy().await.unwrap();
    assert!(host.state.forks().await.unwrap().is_empty());
    assert!(host.state.port_leases().await.unwrap().is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reaper_cleans_up_after_a_simulated_crash() {
    if !git_available() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let data = tmp.path().join("runner");
    let (base_key, child_pid, fork_dir, staging, stale_key) = {
        let host = DevHost::start(&data).await.unwrap();
        let base = host.prepare_files(&[("a.txt", b"a\n")], None).await.unwrap();
        let env = host
            .executor
            .fork(&base, ForkOptions { run_id: mage_core::domain::RunId::from_uuid(uuid::Uuid::now_v7()), purpose: ForkPurpose::Session, egress: EgressProfile::Registries })
            .await
            .unwrap();
        env.write(&EnvPath::ws("work.txt").unwrap(), b"in progress\n", WriteMode::Create).await.unwrap();
        let child = env
            .spawn(SpawnSpec { argv: vec!["sleep".into(), "300".into()], cwd: EnvPath::ws(".").unwrap(), env: vec![], role: ProcessRole::Harness })
            .await
            .unwrap();
        let pid = child.pid();
        // A crash: no destroy, no kill, nothing cleaned up.
        std::mem::forget(child);
        let fork_dir = Path::new(env.root()).parent().unwrap().to_path_buf();
        // A prepare that died mid-way: Preparing row + staging dir.
        let mut stale = base.key;
        stale.sha = mage_core::domain::CommitSha([0xee; 20]);
        let staging = data.join("bases").join(format!("{}.staging-deadbeef0000", stale.id()));
        std::fs::create_dir_all(staging.join("tree")).unwrap();
        assert_eq!(host.state.claim_base(&stale, &staging.display().to_string()).await.unwrap(), BaseClaim::Owned);
        std::mem::forget(env);
        (base.key, pid, fork_dir, staging, stale)
    };
    assert!(!dead(child_pid), "the orphan still runs");

    let host = DevHost::start(&data).await.unwrap();
    let held = recover(host.executor.as_ref(), host.state.as_ref()).await.unwrap();
    assert!(held.is_empty(), "no runs were recorded by this test");
    assert!(host.state.forks().await.unwrap().is_empty());
    assert!(host.state.processes().await.unwrap().is_empty());
    assert!(host.state.port_leases().await.unwrap().is_empty());
    assert!(host.state.base(&stale_key).await.unwrap().is_none(), "Preparing base dropped");
    assert!(!staging.exists(), "staging dir removed");
    assert!(!fork_dir.exists(), "fork dir removed");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(dead(child_pid), "orphaned process group killed");
    assert!(host.executor.base(&base_key).await.unwrap().is_some(), "Ready bases survive");
    // The surviving base still forks.
    let base = host.executor.base(&base_key).await.unwrap().unwrap();
    let env = host
        .executor
        .fork(&base, ForkOptions { run_id: mage_core::domain::RunId::from_uuid(uuid::Uuid::now_v7()), purpose: ForkPurpose::Verify, egress: EgressProfile::Registries })
        .await
        .unwrap();
    assert!(env.read(&EnvPath::ws("work.txt").unwrap(), ReadRange::Whole { max_bytes: 10 }).await.is_err(), "the crashed fork's work is gone");
    env.destroy().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn periodic_reap_destroys_forks_of_inactive_runs_only() {
    if !git_available() {
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let host = DevHost::start(&tmp.path().join("runner")).await.unwrap();
    let active_run = mage_core::domain::RunId::from_uuid(uuid::Uuid::now_v7());
    host.state
        .begin_run(crate::ports::runner_state::ActiveRun {
            run_id: active_run,
            org_id: host.org_id,
            kind: mage_core::domain::RunKind::Triage,
            attempt: 1,
            phase: crate::ports::runner_state::RunPhase::Running,
            env: None,
            started_at: time::OffsetDateTime::now_utc(),
        })
        .await
        .unwrap();
    let keep = host.executor.empty_env(ForkOptions { run_id: active_run, purpose: ForkPurpose::Triage, egress: EgressProfile::ServerOnly }).await.unwrap();
    let gone = host
        .executor
        .empty_env(ForkOptions { run_id: mage_core::domain::RunId::from_uuid(uuid::Uuid::now_v7()), purpose: ForkPurpose::Triage, egress: EgressProfile::ServerOnly })
        .await
        .unwrap();
    let report = host.executor.reap(ReapScope::Periodic).await.unwrap();
    assert_eq!(report.forks_destroyed, 1);
    assert!(matches!(gone.list(&EnvPath::ws(".").unwrap(), ListOpts::default()).await, Err(ExecError::Destroyed)));
    assert!(keep.list(&EnvPath::ws(".").unwrap(), ListOpts::default()).await.is_ok());
    keep.destroy().await.unwrap();
}

/// Enforced mode on a provisioned Linux runner VM (see module docs).
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn enforced_host_contract_and_isolation() {
    if std::env::var("MAGE_TEST_ENFORCED").ok().as_deref() != Some("1") {
        println!("skipped: MAGE_TEST_ENFORCED != 1");
        return;
    }
    let data = std::env::var("MAGE_TEST_ENFORCED_DATA_DIR").map(std::path::PathBuf::from).unwrap_or_else(|_| std::path::PathBuf::from("/var/lib/mage-test"));
    let mut cfg = crate::adapters::driven::exec_host::ExecHostConfig { data_dir: data.clone(), ..Default::default() };
    cfg.postgres.url = std::env::var("MAGE_TEST_RUNNER_PG_URL").ok();
    let database = cfg.postgres.url.is_some();
    let fx = HostFixture { host: DevHost::start_with(cfg).await.unwrap(), database };
    contract::run_core(&fx).await;
    contract::run_processes(&fx).await;
    let env = fx
        .host
        .executor
        .empty_env(ForkOptions { run_id: mage_core::domain::RunId::from_uuid(uuid::Uuid::now_v7()), purpose: ForkPurpose::Triage, egress: EgressProfile::ServerOnly })
        .await
        .unwrap();
    assert_eq!(env.root(), "/mage/ws");
    let probe = |cmd: String| ExecSpec { cwd: EnvPath::scratch(".").unwrap(), ..ExecSpec::shell(cmd, ProcessRole::Exec) };
    let checks = [
        (format!("ls {}", data.display()), false),
        ("cat /etc/mage/* 2>&1 | grep -v 'No such file' | head -c 1 | grep -q . && exit 1 || exit 0".to_owned(), true),
        ("id -un".to_owned(), true),
        ("exec 3<>/dev/tcp/1.1.1.1/443".to_owned(), false),
        ("sudo -n true".to_owned(), false),
    ];
    for (cmd, ok) in checks {
        let out = env.exec(probe(cmd.clone()), None).await.unwrap();
        assert_eq!(out.exit.success(), ok, "{cmd}: {:?} {}", out.exit, String::from_utf8_lossy(&out.stderr.head));
    }
    let who = env.exec(probe("id -un".into()), None).await.unwrap();
    assert_eq!(String::from_utf8_lossy(&who.stdout.head).trim(), "mage-agent");
    env.destroy().await.unwrap();
}
