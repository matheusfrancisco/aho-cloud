//! v1 host `Executor` (executor.md "Host adapter"): bases prepared from a SourceBundle plus the
//! recipe, copy-on-write forks under `<data_dir>/forks`, processes through one launcher (dev:
//! process group + cleared env; enforced/Linux: `mage-agent`, mount namespace, cgroup), recipe
//! services, runner-local Postgres template databases, docker compose projects, a per-env egress
//! proxy, git baselines for patches, and a reaper that makes crashes recoverable.
//!
//! Wiring: open the `RunnerState` at [`ExecHostConfig::state_db_path`], then
//! [`HostExecutor::open`].

mod bundle;
mod cgroup;
mod clone;
mod compose;
mod config;
mod confine;
#[cfg(test)]
mod contract_tests;
mod env;
mod git;
mod launch;
mod logs;
mod ports;
mod postgres;
mod prepare;
mod proxy;
mod reaper;
mod sandbox;
mod services;
mod sys;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::Duration;

use async_trait::async_trait;
use mage_core::domain::RunId;
use mage_core::ports::{Clock, IdGen};
use parking_lot::Mutex;
use tokio::sync::watch;

pub use config::{
    AgentConfig, BaselineConfig, CgroupConfig, DevConfig, DiskConfig, EgressConfig, EgressMode, ExecConfig, ExecHostConfig, ForkConfig, PortsConfig,
    PostgresConfig, PrepareConfig, RunnerEnvironment,
};
pub use postgres::PgStrategy;

use self::compose::Compose;
use self::confine::Owners;
use self::env::HostEnv;
use self::launch::Launcher;
use self::postgres::RunnerPg;
use crate::ports::executor::{
    Arch, BaseId, BaseKey, BaseRef, BaseKind, Env, EnvId, ExecError, Executor, ExecutorCapabilities, ExecutorKind, ForkMode, ForkOptions,
    HarnessPlacement, IsolationMode, KillReason, PrepareObserver, PrepareRequest, ReapReport, ReapScope,
};
use crate::ports::runner_state::{BaseStatus, RunnerState};

/// Interval of the per-fork disk usage check (executor.md UC9 step 4).
const QUOTA_INTERVAL: Duration = Duration::from_secs(60);

/// Paths under `runner.data_dir` (executor.md "Directory layout").
#[derive(Debug, Clone)]
pub(crate) struct Layout {
    pub root: PathBuf,
}

impl Layout {
    pub(crate) fn bundles(&self) -> PathBuf {
        self.root.join("bundles")
    }
    pub(crate) fn bases(&self) -> PathBuf {
        self.root.join("bases")
    }
    pub(crate) fn base(&self, id: &BaseId) -> PathBuf {
        self.bases().join(id.as_str())
    }
    pub(crate) fn forks(&self) -> PathBuf {
        self.root.join("forks")
    }
    pub(crate) fn fork(&self, env: &EnvId) -> PathBuf {
        self.forks().join(env.to_string())
    }
    pub(crate) fn runs(&self) -> PathBuf {
        self.root.join("runs")
    }
    pub(crate) fn trash(&self) -> PathBuf {
        self.root.join("trash")
    }
}

/// State shared by the executor, its envs, and prepares.
pub(crate) struct HostShared {
    pub cfg: ExecHostConfig,
    pub layout: Layout,
    pub state: Arc<dyn RunnerState>,
    pub clock: Arc<dyn Clock>,
    pub ids: Arc<dyn IdGen>,
    pub launcher: Launcher,
    pub owners: Owners,
    pub pg: Option<RunnerPg>,
    pub compose: Compose,
    /// SourceBundle downloads: no proxy.
    pub http: reqwest::Client,
    /// Health probes: no proxy, no redirects.
    pub probe_http: reqwest::Client,
    pub boot_id: String,
    pub arch: Arch,
    pub fork_mode: ForkMode,
    /// Delegated runner cgroup (enforced mode).
    pub cgroup_root: Option<PathBuf>,
    pub disk_pressure: AtomicBool,
    pub envs: Mutex<HashMap<EnvId, Weak<HostEnv>>>,
    /// In-process single-flight per base: the sender is dropped when the prepare ends.
    pub preparing: Mutex<HashMap<BaseId, watch::Receiver<()>>>,
}

impl HostShared {
    pub(crate) fn pg(&self) -> Result<&RunnerPg, ExecError> {
        self.pg.as_ref().ok_or_else(|| ExecError::Unavailable("runner postgres is not configured (runner.postgres.url)".into()))
    }

    pub(crate) fn live_envs(&self) -> Vec<Arc<HostEnv>> {
        self.envs.lock().values().filter_map(Weak::upgrade).collect()
    }

    pub(crate) fn base_ref(&self, rec: &crate::ports::runner_state::BaseRecord) -> BaseRef {
        BaseRef { key: rec.key, id: rec.id.clone(), kind: rec.kind, ready_at: rec.last_used_at }
    }
}

/// Dependencies injected by the integration wave.
#[derive(Clone)]
pub struct HostDeps {
    pub state: Arc<dyn RunnerState>,
    pub clock: Arc<dyn Clock>,
    pub ids: Arc<dyn IdGen>,
}

/// The v1 host executor.
pub struct HostExecutor {
    shared: Arc<HostShared>,
    quota_task: tokio::task::JoinHandle<()>,
}

impl Drop for HostExecutor {
    fn drop(&mut self) {
        self.quota_task.abort();
    }
}

impl ExecHostConfig {
    /// Where the integration opens the SQLite `RunnerState`.
    pub fn state_db_path(&self) -> PathBuf {
        self.data_dir.join("state.db")
    }
}

fn host_arch() -> Arch {
    if cfg!(target_arch = "aarch64") { Arch::Aarch64 } else { Arch::X86_64 }
}

fn mkdir_mode(path: &Path, mode: u32) -> Result<(), ExecError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path).map_err(|e| ExecError::Unavailable(format!("create {}: {e}", path.display())))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(|e| ExecError::Unavailable(format!("chmod {}: {e}", path.display())))
}

impl HostExecutor {
    /// Validates the config, runs the mode's start checks (the daemon refuses to start when they
    /// fail), creates the data dir layout, probes reflink support, and starts the per-fork quota
    /// monitor. Run `app::runner::recover::recover` before leasing.
    pub async fn open(cfg: ExecHostConfig, deps: HostDeps) -> Result<Arc<Self>, ExecError> {
        cfg.validate()?;
        let layout = Layout { root: cfg.data_dir.clone() };
        mkdir_mode(&layout.root, 0o750)?;
        for dir in [layout.bundles(), layout.bases(), layout.forks(), layout.trash()] {
            mkdir_mode(&dir, 0o750)?;
        }
        mkdir_mode(&layout.runs(), 0o700)?;

        let (launcher, owners, cgroup_root) = match cfg.isolation {
            IsolationMode::Dev => {
                if cfg.egress.mode == EgressMode::Off {
                    tracing::warn!("runner isolation = dev: egress is not enforced and nothing is hidden from env processes");
                }
                (Launcher::Dev, Owners { agent: None }, None)
            }
            IsolationMode::Enforced => enforced_checks(&cfg).await?,
        };
        let fork_mode = clone::probe(&layout.root);
        if fork_mode == ForkMode::Copy && !cfg.fork.allow_copy_fallback {
            return Err(ExecError::Unavailable("data dir filesystem lacks reflink (XFS reflink=1 required) and runner.fork.allow_copy_fallback = false".into()));
        }
        let pg = match &cfg.postgres.url {
            Some(url) => Some(RunnerPg::connect_lazy(url, cfg.postgres.password.as_deref(), cfg.postgres.strategy)?),
            None => None,
        };
        let http = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| ExecError::Unavailable(format!("http client: {e}")))?;
        let probe_http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| ExecError::Unavailable(format!("http client: {e}")))?;
        let shared = Arc::new(HostShared {
            cfg,
            layout,
            state: deps.state,
            clock: deps.clock,
            ids: deps.ids,
            launcher,
            owners,
            pg,
            compose: Compose::new(None),
            http,
            probe_http,
            boot_id: sys::boot_id(),
            arch: host_arch(),
            fork_mode,
            cgroup_root,
            disk_pressure: AtomicBool::new(false),
            envs: Mutex::new(HashMap::new()),
            preparing: Mutex::new(HashMap::new()),
        });
        let quota_task = tokio::spawn(quota_monitor(Arc::downgrade(&shared)));
        Ok(Arc::new(Self { shared, quota_task }))
    }

    pub fn data_dir(&self) -> &Path {
        &self.shared.layout.root
    }

    /// `<data_dir>/runs/<run_id>/outbox` (0700): exported artifacts awaiting upload.
    pub fn outbox(&self, run: RunId) -> Result<PathBuf, ExecError> {
        let dir = self.shared.layout.runs().join(run.to_string()).join("outbox");
        mkdir_mode(&dir, 0o700)?;
        Ok(dir)
    }

    pub fn fork_mode(&self) -> ForkMode {
        self.shared.fork_mode
    }
}

/// Enforced-mode start checks: Linux, `mage-agent` exists, mount points present, nftables
/// rules installed (egress proxy mode), delegated cgroup usable.
#[cfg(target_os = "linux")]
async fn enforced_checks(cfg: &ExecHostConfig) -> Result<(Launcher, Owners, Option<PathBuf>), ExecError> {
    let (uid, gid) = sys::lookup_user(&cfg.agent.user)?;
    for mount in ["/mage/ws", "/mage/home", "/mage/scratch"] {
        if !Path::new(mount).is_dir() {
            return Err(ExecError::Unavailable(format!("enforced isolation needs the mount point {mount}")));
        }
    }
    if cfg.egress.mode == EgressMode::Proxy {
        let out = tokio::process::Command::new("nft")
            .args(["-j", "list", "table", "inet", "mage"])
            .env_clear()
            .env("PATH", "/usr/sbin:/sbin:/usr/bin:/bin")
            .output()
            .await
            .map_err(|e| ExecError::Unavailable(format!("nft: {e}")))?;
        if !out.status.success() || !String::from_utf8_lossy(&out.stdout).contains("mage-agent") {
            return Err(ExecError::Unavailable("nftables table `inet mage` restricting mage-agent to lo is not installed".into()));
        }
    }
    let root = cgroup::runner_root(cfg.cgroup.root.as_deref())?;
    cgroup::setup_root(&root)?;
    let launcher = Launcher::Enforced(launch::EnforcedLauncher { uid, gid, data_dir: cfg.data_dir.clone(), etc_mage: PathBuf::from("/etc/mage") });
    Ok((launcher, Owners { agent: Some((uid, gid)) }, Some(root)))
}

#[cfg(not(target_os = "linux"))]
async fn enforced_checks(_cfg: &ExecHostConfig) -> Result<(Launcher, Owners, Option<PathBuf>), ExecError> {
    Err(ExecError::Unavailable("isolation = \"enforced\" needs Linux".into()))
}

/// Every 60 s: forks over `disk.fork_max_gib` enter `QuotaExceeded` (running execs are killed
/// with `KillReason::Quota`, new operations fail).
async fn quota_monitor(shared: Weak<HostShared>) {
    let mut timer = tokio::time::interval(QUOTA_INTERVAL);
    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        timer.tick().await;
        let Some(shared) = shared.upgrade() else { return };
        let limit = shared.cfg.disk.fork_max_gib << 30;
        for env in shared.live_envs() {
            if env.sandbox().kill_reason().is_some() {
                continue;
            }
            let dir = env.sandbox().dir.clone();
            let used = tokio::task::spawn_blocking(move || sys::allocated_bytes(&dir)).await.unwrap_or(0);
            if used > limit {
                tracing::warn!(env = %env.id(), used, limit, "fork exceeded runner.disk.fork_max_gib");
                let sb = Arc::clone(env.sandbox());
                let grace = sb.kill_grace();
                sb.kill_all(KillReason::Quota, grace).await;
            }
        }
    }
}

#[async_trait]
impl Executor for HostExecutor {
    async fn capabilities(&self) -> Result<ExecutorCapabilities, ExecError> {
        let s = &self.shared;
        let bases = s
            .state
            .bases()
            .await?
            .into_iter()
            .filter(|b| b.status == BaseStatus::Ready && b.kind == BaseKind::Prepared)
            .map(|b| b.key)
            .collect();
        let live = s.state.forks().await?.len();
        let (free, _) = sys::disk_space(&s.layout.root)?;
        Ok(ExecutorCapabilities {
            kind: ExecutorKind::Host,
            arch: s.arch,
            bases,
            free_env_slots: s.cfg.max_envs.saturating_sub(u16::try_from(live).unwrap_or(u16::MAX)),
            disk_free_bytes: free,
            disk_pressure: s.disk_pressure.load(Ordering::SeqCst),
            fork_mode: s.fork_mode,
            harness_placement: HarnessPlacement::InEnv,
            isolation: s.cfg.isolation,
        })
    }

    async fn prepare(&self, req: PrepareRequest, obs: &dyn PrepareObserver) -> Result<BaseRef, ExecError> {
        prepare::prepare(&self.shared, req, obs).await
    }

    async fn fork(&self, base: &BaseRef, opts: ForkOptions) -> Result<Arc<dyn Env>, ExecError> {
        reaper::ensure_disk(&self.shared, 0).await?;
        let env = HostEnv::fork(&self.shared, base, opts).await?;
        Ok(env)
    }

    async fn empty_env(&self, opts: ForkOptions) -> Result<Arc<dyn Env>, ExecError> {
        let env = HostEnv::empty(&self.shared, opts).await?;
        Ok(env)
    }

    async fn base(&self, key: &BaseKey) -> Result<Option<BaseRef>, ExecError> {
        Ok(self.shared.state.base(key).await?.filter(|b| b.status == BaseStatus::Ready).map(|b| self.shared.base_ref(&b)))
    }

    async fn evict(&self, key: &BaseKey) -> Result<u64, ExecError> {
        reaper::evict_base(&self.shared, key).await
    }

    async fn reap(&self, scope: ReapScope) -> Result<ReapReport, ExecError> {
        reaper::reap(&self.shared, scope).await
    }
}
