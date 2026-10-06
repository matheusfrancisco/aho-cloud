//! A `Sandbox` is the process/file context of one fork, or of a base being prepared: area roots,
//! base environment, template rendering, process launch with capture and kill escalation, and
//! the registry of live process groups (executor.md "Spawning processes", "Output capture").

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use mage_core::domain::RunId;
use mage_core::domain::recipe::RecipeSpec;
use nix::sys::signal::Signal;
use parking_lot::Mutex;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::watch;

use super::HostShared;
use super::confine::AreaRoot;
use super::launch::{LaunchSpec, Launched, MountSet, kill_group};
use super::logs::LogWriter;
use super::postgres::DbCreds;
use super::sys;
use crate::ports::executor::{
    Area, CaptureLimits, Captured, EnvChild, EnvId, EnvPath, ExecError, ExecOutcome, ExecSink, ExecSpec, Exit, IsolationMode, KillReason,
    LogSource, PortMap, ProcessRole, StdStream, check_extra_env, render_template,
};
use crate::ports::runner_state::{PortOwner, ProcessRecord};

/// How long pipes are drained after the process group is gone.
const DRAIN_AFTER_EXIT: Duration = Duration::from_secs(2);

/// Mutable recipe-derived state of a sandbox.
#[derive(Debug, Clone, Default)]
pub(crate) struct SandboxVars {
    pub recipe: Option<RecipeSpec>,
    pub ports: PortMap,
    pub db: Option<DbCreds>,
    /// `provider = "service"`: rendered `database.url`.
    pub service_db_url: Option<String>,
    pub proxy_port: Option<u16>,
}

struct ProcEntry {
    cgroup_leaf: Option<PathBuf>,
}

pub(crate) struct Sandbox {
    pub host: Arc<HostShared>,
    /// Display id for `MAGE_ENV_ID` and logs: `env_…` or `base_<id>`.
    pub label: String,
    pub env_id: Option<EnvId>,
    pub run_id: Option<RunId>,
    pub dir: PathBuf,
    pub ws: AreaRoot,
    pub scratch: AreaRoot,
    /// Workspace / scratch as env processes see them.
    pub visible_ws: String,
    pub visible_scratch: String,
    pub vars: Mutex<SandboxVars>,
    /// Env cgroup (enforced mode).
    pub cgroup: Option<PathBuf>,
    /// Owner of this sandbox's port leases (`Env` for forks, `Base` while preparing).
    pub port_owner: PortOwner,
    /// Compose project name: `mage-<env slug>` or `mage-b<base id>`.
    pub compose_name: String,
    procs: Mutex<HashMap<i32, ProcEntry>>,
    exec_seq: AtomicU64,
    leaf_seq: AtomicU64,
    /// Set once by destroy (`Destroyed`) or the quota monitor (`Quota`).
    kill: watch::Sender<Option<KillReason>>,
}

impl Sandbox {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        host: Arc<HostShared>,
        label: String,
        env_id: Option<EnvId>,
        run_id: Option<RunId>,
        dir: PathBuf,
        cgroup: Option<PathBuf>,
        port_owner: PortOwner,
        compose_name: String,
    ) -> Self {
        let owners = host.owners;
        let (visible_ws, visible_scratch) = match host.cfg.isolation {
            IsolationMode::Enforced => ("/mage/ws".to_owned(), "/mage/scratch".to_owned()),
            IsolationMode::Dev => (dir.join("tree").display().to_string(), dir.join("scratch").display().to_string()),
        };
        Self {
            ws: AreaRoot::new(dir.join("tree"), Area::Workspace, owners),
            scratch: AreaRoot::new(dir.join("scratch"), Area::Scratch, owners),
            host,
            label,
            env_id,
            run_id,
            dir,
            visible_ws,
            visible_scratch,
            vars: Mutex::new(SandboxVars::default()),
            cgroup,
            port_owner,
            compose_name,
            procs: Mutex::new(HashMap::new()),
            exec_seq: AtomicU64::new(0),
            leaf_seq: AtomicU64::new(0),
            kill: watch::channel(None).0,
        }
    }

    pub(crate) fn area(&self, area: Area) -> &AreaRoot {
        match area {
            Area::Workspace => &self.ws,
            Area::Scratch => &self.scratch,
        }
    }

    pub(crate) fn logs_dir(&self) -> PathBuf {
        self.dir.join("logs")
    }

    pub(crate) fn run_dir(&self) -> PathBuf {
        self.dir.join("run")
    }

    pub(crate) fn log_path(&self, source: &LogSource) -> PathBuf {
        let logs = self.logs_dir();
        match source {
            LogSource::Prepare => logs.join("prepare.log"),
            LogSource::Build(step) => logs.join("build").join(format!("{step}.log")),
            LogSource::Service(name) => logs.join("svc").join(format!("{name}.log")),
            LogSource::Exec(seq) => logs.join("exec").join(format!("{seq}.log")),
            LogSource::Mcp => logs.join("mcp.jsonl"),
            LogSource::Egress => logs.join("egress.log"),
        }
    }

    pub(crate) fn kill_reason(&self) -> Option<KillReason> {
        *self.kill.borrow()
    }

    /// Fails with `Destroyed` / `QuotaExceeded` once the sandbox was killed.
    pub(crate) fn check_live(&self) -> Result<(), ExecError> {
        match self.kill_reason() {
            Some(KillReason::Destroyed) => Err(ExecError::Destroyed),
            Some(KillReason::Quota) => Err(ExecError::QuotaExceeded(format!("{} exceeded runner.disk.fork_max_gib", self.label))),
            Some(KillReason::Cancelled) | None => Ok(()),
        }
    }

    // -----------------------------------------------------------------------------------------
    // Rendering and environment
    // -----------------------------------------------------------------------------------------

    fn lookup(&self, vars: &SandboxVars, name: &str, build: bool) -> Option<String> {
        match name {
            "ws" => return Some(self.visible_ws.clone()),
            "scratch" => return Some(self.visible_scratch.clone()),
            _ if build => return None,
            _ => {}
        }
        if let Some(port) = name.strip_prefix("port.") {
            return vars.ports.get(port).map(|a| a.port().to_string());
        }
        if let Some(service) = name.strip_prefix("service.").and_then(|s| s.strip_suffix(".url")) {
            let first = vars.recipe.as_ref()?.services.get(service)?.ports.iter().min()?.clone();
            return vars.ports.get(&first).map(|a| format!("http://127.0.0.1:{}", a.port()));
        }
        let field = name.strip_prefix("db.")?;
        if let Some(db) = &vars.db {
            return match field {
                "url" => Some(db.url()),
                "host" => Some(db.host.clone()),
                "port" => Some(db.port.to_string()),
                "name" => Some(db.database.clone()),
                "user" => Some(db.user.clone()),
                "password" => Some(db.password.clone()),
                _ => None,
            };
        }
        let url = url::Url::parse(vars.service_db_url.as_deref()?).ok()?;
        match field {
            "url" => Some(url.to_string()),
            "host" => url.host_str().map(str::to_owned),
            "port" => url.port().map(|p| p.to_string()),
            "name" => Some(url.path().trim_start_matches('/').to_owned()),
            "user" => Some(url.username().to_owned()),
            "password" => url.password().map(str::to_owned),
            _ => None,
        }
    }

    /// `Env::render` semantics with this sandbox's variables.
    pub(crate) fn render(&self, template: &str) -> Result<String, ExecError> {
        let vars = self.vars.lock().clone();
        render_template(template, |name| self.lookup(&vars, name, false))
    }

    /// Build-step rendering: only `${ws}` and `${scratch}` (build output must not bake ports).
    pub(crate) fn render_build(&self, template: &str) -> Result<String, ExecError> {
        render_template(template, |name| self.lookup(&SandboxVars::default(), name, true))
    }

    /// The base environment of every spawned process plus `step_env` and `extra` (executor.md
    /// table, in order; later entries win).
    pub(crate) fn process_env(
        &self,
        role: &ProcessRole,
        step_env: Option<&BTreeMap<String, String>>,
        extra: &[(String, String)],
    ) -> Result<Vec<(String, String)>, ExecError> {
        let cfg = &self.host.cfg;
        let vars = self.vars.lock().clone();
        let build = *role == ProcessRole::Build;
        let enforced = cfg.isolation == IsolationMode::Enforced;
        let mut env: BTreeMap<String, String> = BTreeMap::new();
        let mut path: Vec<String> = Vec::new();
        if *role == ProcessRole::Scip {
            path.push(cfg.scip_bin.display().to_string());
        }
        if let Some(recipe) = &vars.recipe {
            for (tool, version) in &recipe.toolchain {
                if let Some(dir) = cfg.toolchains.get(tool).and_then(|v| v.get(version)) {
                    path.push(dir.display().to_string());
                }
            }
        }
        path.extend(cfg.agent.path.iter().map(|p| p.display().to_string()));
        env.insert("PATH".into(), path.join(":"));
        let home = if enforced {
            "/mage/home".to_owned()
        } else if *role == ProcessRole::Harness && cfg.dev.harness_real_home {
            std::env::var("HOME").unwrap_or_else(|_| self.dir.join("home").display().to_string())
        } else {
            self.dir.join("home").display().to_string()
        };
        env.insert("HOME".into(), home);
        let user = if enforced { cfg.agent.user.clone() } else { sys::current_user() };
        env.insert("USER".into(), user.clone());
        env.insert("LOGNAME".into(), user);
        env.insert("SHELL".into(), "/bin/sh".into());
        env.insert("LANG".into(), "C.UTF-8".into());
        env.insert("LC_ALL".into(), "C.UTF-8".into());
        env.insert("TZ".into(), "UTC".into());
        env.insert("TMPDIR".into(), if enforced { "/tmp".to_owned() } else { self.dir.join("tmp").display().to_string() });
        env.insert("CI".into(), "1".into());
        if let Some(port) = vars.proxy_port {
            let proxy = format!("http://127.0.0.1:{port}");
            for key in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
                env.insert(key.into(), proxy.clone());
            }
        }
        env.insert("NO_PROXY".into(), "127.0.0.1,localhost,::1".into());
        env.insert("no_proxy".into(), "127.0.0.1,localhost,::1".into());
        env.insert("MAGE_ENV_ID".into(), self.label.clone());
        if let Some(run) = self.run_id {
            env.insert("MAGE_RUN_ID".into(), run.to_string());
        }
        env.insert("MAGE_WS".into(), self.visible_ws.clone());
        env.insert("MAGE_SCRATCH".into(), self.visible_scratch.clone());
        if !build {
            for (name, addr) in &vars.ports.0 {
                env.insert(format!("MAGE_PORT_{}", name.as_str().to_ascii_uppercase()), addr.port().to_string());
            }
            if let Some(url) = vars.db.as_ref().map(DbCreds::url).or_else(|| vars.service_db_url.clone()) {
                env.insert("MAGE_DB_URL".into(), url);
            }
            if let Some(browser) = vars.recipe.as_ref().and_then(|r| r.browser.as_ref()) {
                let render = |t: &str| render_template(t, |n| self.lookup(&vars, n, false));
                if let Ok(base) = render(&browser.base_url) {
                    if let Some(login) = &browser.login {
                        let login_url = if login.url.starts_with('/') { format!("{}{}", base.trim_end_matches('/'), login.url) } else { render(&login.url).unwrap_or_default() };
                        env.insert("MAGE_LOGIN_URL".into(), login_url);
                        env.insert("MAGE_LOGIN_USER".into(), login.username.clone());
                        env.insert("MAGE_LOGIN_PASSWORD".into(), login.password.clone());
                    }
                    env.insert("MAGE_BASE_URL".into(), base);
                }
            }
        }
        let layered = |table: &BTreeMap<String, String>, env: &mut BTreeMap<String, String>| {
            for (key, value) in table {
                // Entries whose variables are unavailable here are left out: build steps never
                // see port/DB values, and a Learn env has no DB before `setup_db`.
                if let Ok(v) = render_template(value, |n| self.lookup(&vars, n, build)) {
                    env.insert(key.clone(), v);
                }
            }
        };
        if let Some(recipe) = &vars.recipe {
            layered(&recipe.env, &mut env);
        }
        if let Some(step) = step_env {
            layered(step, &mut env);
        }
        for name in &cfg.agent.env_passthrough {
            if let Ok(value) = std::env::var(name) {
                env.insert(name.clone(), value);
            }
        }
        check_extra_env(extra)?;
        for (k, v) in extra {
            env.insert(k.clone(), v.clone());
        }
        Ok(env.into_iter().collect())
    }

    // -----------------------------------------------------------------------------------------
    // Launching
    // -----------------------------------------------------------------------------------------

    fn leaf_name(&self, role: &ProcessRole) -> String {
        let n = self.leaf_seq.fetch_add(1, Ordering::SeqCst) + 1;
        match role {
            ProcessRole::Harness => format!("harness-{n}"),
            ProcessRole::Service(name) => format!("svc-{name}"),
            ProcessRole::Exec => format!("exec-{n}"),
            ProcessRole::Build => format!("build-{n}"),
            ProcessRole::Test => format!("test-{n}"),
            ProcessRole::Playwright => format!("pw-{n}"),
            ProcessRole::Scip => format!("scip-{n}"),
            ProcessRole::Precheck => format!("precheck-{n}"),
        }
    }

    /// Resolves a cwd inside an area: `(path for the launcher, real path)`.
    pub(crate) fn resolve_cwd(&self, cwd: &EnvPath) -> Result<PathBuf, ExecError> {
        let (real, resolved) = self.area(cwd.area).dir_path(&cwd.rel)?;
        Ok(match self.host.cfg.isolation {
            IsolationMode::Dev => real,
            IsolationMode::Enforced => {
                let mut p = PathBuf::from(if cwd.area == Area::Workspace { "/mage/ws" } else { "/mage/scratch" });
                p.extend(resolved);
                p
            }
        })
    }

    /// Launches and records a process; returns it with its leaf cgroup.
    pub(crate) async fn launch(
        &self,
        argv: Vec<String>,
        env: Vec<(String, String)>,
        cwd: PathBuf,
        stdin_piped: bool,
        role: &ProcessRole,
    ) -> Result<(Launched, Option<PathBuf>), ExecError> {
        self.check_live()?;
        let enforced = self.host.cfg.isolation == IsolationMode::Enforced;
        let leaf = self.cgroup.as_ref().map(|cg| cg.join(self.leaf_name(role)));
        let mounts = enforced.then(|| MountSet {
            tree: self.dir.join("tree"),
            home: self.dir.join("home"),
            scratch: self.dir.join("scratch"),
            tmp: self.dir.join("tmp"),
            shm: self.dir.join("shm"),
        });
        let launched = self.host.launcher.launch(LaunchSpec { argv, env, cwd, stdin_piped, mounts, cgroup_leaf: leaf.clone() })?;
        self.procs.lock().insert(launched.pgid, ProcEntry { cgroup_leaf: leaf.clone() });
        let record = ProcessRecord {
            pgid: launched.pgid,
            pid: launched.pid,
            start_time: launched.start_time,
            boot_id: self.host.boot_id.clone(),
            run_id: self.run_id,
            env: self.env_id,
            role: role.clone(),
            cgroup: leaf.as_ref().map(|p| p.display().to_string()),
        };
        if let Err(err) = self.host.state.record_process(record).await {
            kill_group(launched.pgid, leaf.as_deref());
            self.procs.lock().remove(&launched.pgid);
            return Err(err.into());
        }
        Ok((launched, leaf))
    }

    /// Forgets a finished process group: kills leftovers, clears the state row, removes the leaf.
    pub(crate) async fn reap_group(&self, pgid: i32, leaf: Option<&Path>) {
        kill_group(pgid, leaf);
        self.procs.lock().remove(&pgid);
        if let Err(err) = self.host.state.clear_process(pgid).await {
            tracing::warn!(pgid, error = %err, "clear_process failed");
        }
        if let Some(leaf) = leaf {
            let _ = std::fs::remove_dir(leaf);
        }
    }

    /// TERM every live process group → grace → KILL (Linux: `cgroup.kill` on the env cgroup).
    /// Returns how many groups were signalled.
    pub(crate) async fn kill_all(&self, reason: KillReason, grace: Duration) -> u32 {
        self.kill.send_if_modified(|cur| {
            if cur.is_none() {
                *cur = Some(reason);
                true
            } else {
                false
            }
        });
        let groups: Vec<(i32, Option<PathBuf>)> = self.procs.lock().iter().map(|(pgid, e)| (*pgid, e.cgroup_leaf.clone())).collect();
        for (pgid, _) in &groups {
            sys::signal_group(*pgid, Signal::SIGTERM);
        }
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline && groups.iter().any(|(pgid, _)| sys::group_alive(*pgid)) {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        if let Some(cg) = &self.cgroup {
            let _ = std::fs::write(cg.join("cgroup.kill"), b"1");
        }
        for (pgid, leaf) in &groups {
            kill_group(*pgid, leaf.as_deref());
            if let Err(err) = self.host.state.clear_process(*pgid).await {
                tracing::warn!(pgid, error = %err, "clear_process failed");
            }
        }
        self.procs.lock().clear();
        u32::try_from(groups.len()).unwrap_or(u32::MAX)
    }

    pub(crate) fn kill_grace(&self) -> Duration {
        Duration::from_secs(u64::from(self.host.cfg.exec.kill_grace_s))
    }

    // -----------------------------------------------------------------------------------------
    // exec
    // -----------------------------------------------------------------------------------------

    /// `Env::exec` with an optional per-step env (`[[build]].env`, `[test].env`) and log target.
    pub(crate) async fn run(
        &self,
        spec: ExecSpec,
        step_env: Option<&BTreeMap<String, String>>,
        sink: Option<&dyn ExecSink>,
        log_override: Option<(LogSource, LogWriter)>,
    ) -> Result<ExecOutcome, ExecError> {
        self.check_live()?;
        check_extra_env(&spec.env)?;
        let max = max_timeout(&self.host.cfg.exec, &spec.role, log_override.is_some());
        if spec.timeout.is_zero() || spec.timeout > max {
            return Err(ExecError::Invalid(format!("timeout must be in 1..={}s", max.as_secs())));
        }
        let argv = spec.command.to_argv();
        if argv.is_empty() {
            return Err(ExecError::Invalid("empty argv".into()));
        }
        let cwd = self.resolve_cwd(&spec.cwd)?;
        let env = self.process_env(&spec.role, step_env, &spec.env)?;
        let (source, log) = match log_override {
            Some(pair) => pair,
            None => {
                let seq = self.exec_seq.fetch_add(1, Ordering::SeqCst) + 1;
                let source = LogSource::Exec(seq);
                let log = LogWriter::open(&self.log_path(&source))?;
                (source, log)
            }
        };
        let started = Instant::now();
        let (mut launched, leaf) = self.launch(argv, env, cwd, spec.stdin.is_some(), &spec.role).await?;
        let pgid = launched.pgid;
        let mut guard = GroupGuard { host: Arc::clone(&self.host), pgid, leaf: leaf.clone(), armed: true };

        let mut stdout = launched.child.stdout.take();
        let mut stderr = launched.child.stderr.take();
        if let (Some(input), Some(mut stdin)) = (spec.stdin, launched.child.stdin.take()) {
            tokio::spawn(async move {
                let _ = stdin.write_all(&input).await;
                let _ = stdin.shutdown().await;
            });
        }
        let mut out_cap = CaptureBuf::new(spec.capture);
        let mut err_cap = CaptureBuf::new(spec.capture);
        let mut obuf = vec![0u8; 32 * 1024];
        let mut ebuf = vec![0u8; 32 * 1024];
        let deadline = tokio::time::Instant::now() + spec.timeout;
        let far = tokio::time::Instant::now() + Duration::from_secs(365 * 24 * 3600);
        let mut kill_at: Option<tokio::time::Instant> = None;
        let mut drain_until: Option<tokio::time::Instant> = None;
        let mut status: Option<std::process::ExitStatus> = None;
        let mut forced: Option<Exit> = None;
        let mut kill_rx = self.kill.subscribe();
        let grace = self.kill_grace();

        loop {
            if stdout.is_none() && stderr.is_none() && status.is_some() {
                break;
            }
            tokio::select! {
                n = read_opt(&mut stdout, &mut obuf), if stdout.is_some() => match n {
                    Some(n) if n > 0 => {
                        let data = &obuf[..n];
                        if let Some(sink) = sink { sink.chunk(StdStream::Stdout, data); }
                        log.write(data);
                        out_cap.push(data);
                    }
                    _ => stdout = None,
                },
                n = read_opt(&mut stderr, &mut ebuf), if stderr.is_some() => match n {
                    Some(n) if n > 0 => {
                        let data = &ebuf[..n];
                        if let Some(sink) = sink { sink.chunk(StdStream::Stderr, data); }
                        log.write(data);
                        err_cap.push(data);
                    }
                    _ => stderr = None,
                },
                st = launched.child.wait(), if status.is_none() => {
                    status = Some(st.map_err(|e| ExecError::Io(format!("wait: {e}")))?);
                    // A destroy/quota kill whose signal won the race against `kill_rx` is still
                    // the cause (as in `HostChild::finish`).
                    if forced.is_none() {
                        forced = self.kill_reason().map(Exit::Killed);
                    }
                    // Background leftovers of the exec die with it, so the pipes close.
                    kill_group(pgid, leaf.as_deref());
                    drain_until = Some(tokio::time::Instant::now() + DRAIN_AFTER_EXIT);
                },
                () = tokio::time::sleep_until(deadline), if forced.is_none() && status.is_none() => {
                    forced = Some(Exit::TimedOut);
                    sys::signal_group(pgid, Signal::SIGTERM);
                    kill_at = Some(tokio::time::Instant::now() + grace);
                },
                () = tokio::time::sleep_until(kill_at.unwrap_or(far)), if kill_at.is_some() && status.is_none() => {
                    kill_group(pgid, leaf.as_deref());
                    kill_at = None;
                },
                changed = kill_rx.wait_for(Option::is_some), if forced.is_none() && status.is_none() => {
                    let reason = changed.ok().and_then(|r| *r).unwrap_or(KillReason::Cancelled);
                    forced = Some(Exit::Killed(reason));
                    sys::signal_group(pgid, Signal::SIGTERM);
                    kill_at = Some(tokio::time::Instant::now() + grace);
                },
                () = tokio::time::sleep_until(drain_until.unwrap_or(far)), if drain_until.is_some() => break,
            }
        }
        self.reap_group(pgid, leaf.as_deref()).await;
        guard.armed = false;
        let exit = forced.unwrap_or_else(|| exit_of(status));
        Ok(ExecOutcome { exit, stdout: out_cap.finish(), stderr: err_cap.finish(), duration: started.elapsed(), log: source })
    }

    // -----------------------------------------------------------------------------------------
    // spawn
    // -----------------------------------------------------------------------------------------

    pub(crate) async fn spawn_child(
        self: &Arc<Self>,
        argv: Vec<String>,
        cwd: &EnvPath,
        step_env: Option<&BTreeMap<String, String>>,
        extra: &[(String, String)],
        role: ProcessRole,
    ) -> Result<HostChild, ExecError> {
        self.check_live()?;
        if argv.is_empty() {
            return Err(ExecError::Invalid("empty argv".into()));
        }
        let cwd = self.resolve_cwd(cwd)?;
        let env = self.process_env(&role, step_env, extra)?;
        let (mut launched, leaf) = self.launch(argv, env, cwd, true, &role).await?;
        Ok(HostChild {
            stdin: launched.child.stdin.take(),
            stdout: launched.child.stdout.take(),
            stderr: launched.child.stderr.take(),
            pid: launched.pid,
            pgid: launched.pgid,
            child: launched.child,
            leaf,
            sandbox: Arc::clone(self),
            exit: None,
        })
    }
}

/// Exec timeout ceiling: `exec.max_timeout_s`, or the recipe ceiling (3600 s) for build, test,
/// and SCIP commands and for recipe-driven internal runs (migrate, seed, health commands).
pub(crate) fn max_timeout(cfg: &super::config::ExecConfig, role: &ProcessRole, internal: bool) -> Duration {
    let configured = Duration::from_secs(u64::from(cfg.max_timeout_s));
    if internal || matches!(role, ProcessRole::Build | ProcessRole::Test | ProcessRole::Scip) {
        configured.max(Duration::from_secs(3600))
    } else {
        configured
    }
}

async fn read_opt<R: AsyncRead + Unpin>(stream: &mut Option<R>, buf: &mut [u8]) -> Option<usize> {
    match stream {
        Some(s) => s.read(buf).await.ok(),
        None => None,
    }
}

pub(crate) fn exit_of(status: Option<std::process::ExitStatus>) -> Exit {
    use std::os::unix::process::ExitStatusExt;
    match status {
        Some(st) => match (st.code(), st.signal()) {
            (Some(code), _) => Exit::Code(code),
            (None, Some(sig)) => Exit::Signal(sig),
            (None, None) => Exit::Code(-1),
        },
        None => Exit::Killed(KillReason::Cancelled),
    }
}

/// Kills the group if the owning future is dropped (Run MCP cancel → exec future dropped).
struct GroupGuard {
    host: Arc<HostShared>,
    pgid: i32,
    leaf: Option<PathBuf>,
    armed: bool,
}

impl Drop for GroupGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        kill_group(self.pgid, self.leaf.as_deref());
        let host = Arc::clone(&self.host);
        let pgid = self.pgid;
        let leaf = self.leaf.take();
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move {
                let _ = host.state.clear_process(pgid).await;
                if let Some(leaf) = leaf {
                    let _ = std::fs::remove_dir(leaf);
                }
            });
        }
    }
}

/// First `head` bytes plus a ring of the last `tail` bytes after them.
pub(crate) struct CaptureBuf {
    limits: CaptureLimits,
    head: Vec<u8>,
    tail: VecDeque<u8>,
    total: u64,
}

impl CaptureBuf {
    pub(crate) fn new(limits: CaptureLimits) -> Self {
        Self { limits, head: Vec::with_capacity(limits.head.min(64 * 1024)), tail: VecDeque::new(), total: 0 }
    }

    pub(crate) fn push(&mut self, mut data: &[u8]) {
        self.total += data.len() as u64;
        let room = self.limits.head - self.head.len().min(self.limits.head);
        if room > 0 {
            let take = room.min(data.len());
            self.head.extend_from_slice(&data[..take]);
            data = &data[take..];
        }
        if self.limits.tail == 0 || data.is_empty() {
            return;
        }
        if data.len() >= self.limits.tail {
            self.tail.clear();
            self.tail.extend(&data[data.len() - self.limits.tail..]);
            return;
        }
        let overflow = (self.tail.len() + data.len()).saturating_sub(self.limits.tail);
        self.tail.drain(..overflow);
        self.tail.extend(data);
    }

    pub(crate) fn finish(self) -> Captured {
        Captured { head: self.head, tail: self.tail.into_iter().collect(), total_bytes: self.total }
    }
}

/// `EnvChild` for `Env::spawn`.
pub(crate) struct HostChild {
    child: tokio::process::Child,
    stdin: Option<tokio::process::ChildStdin>,
    stdout: Option<tokio::process::ChildStdout>,
    stderr: Option<tokio::process::ChildStderr>,
    pid: i32,
    pgid: i32,
    leaf: Option<PathBuf>,
    sandbox: Arc<Sandbox>,
    exit: Option<Exit>,
}

impl HostChild {
    async fn finish(&mut self, status: std::io::Result<std::process::ExitStatus>) -> Result<Exit, ExecError> {
        let status = status.map_err(|e| ExecError::Io(format!("wait: {e}")))?;
        let exit = match self.sandbox.kill_reason() {
            Some(reason) => Exit::Killed(reason),
            None => exit_of(Some(status)),
        };
        self.sandbox.reap_group(self.pgid, self.leaf.as_deref()).await;
        self.exit = Some(exit);
        Ok(exit)
    }

    /// Takes the child's pipes for an internal pump (services).
    pub(crate) fn take_pipes(&mut self) -> (Option<tokio::process::ChildStdout>, Option<tokio::process::ChildStderr>) {
        self.stdin = None;
        (self.stdout.take(), self.stderr.take())
    }

    pub(crate) fn pgid(&self) -> i32 {
        self.pgid
    }

    pub(crate) fn leaf(&self) -> Option<PathBuf> {
        self.leaf.clone()
    }
}

#[async_trait]
impl EnvChild for HostChild {
    fn pid(&self) -> u32 {
        self.pid as u32
    }

    fn take_stdin(&mut self) -> Option<Pin<Box<dyn AsyncWrite + Send>>> {
        self.stdin.take().map(|s| Box::pin(s) as Pin<Box<dyn AsyncWrite + Send>>)
    }

    fn take_stdout(&mut self) -> Option<Pin<Box<dyn AsyncRead + Send>>> {
        self.stdout.take().map(|s| Box::pin(s) as Pin<Box<dyn AsyncRead + Send>>)
    }

    fn take_stderr(&mut self) -> Option<Pin<Box<dyn AsyncRead + Send>>> {
        self.stderr.take().map(|s| Box::pin(s) as Pin<Box<dyn AsyncRead + Send>>)
    }

    async fn wait(&mut self) -> Result<Exit, ExecError> {
        if let Some(exit) = self.exit {
            return Ok(exit);
        }
        let status = self.child.wait().await;
        self.finish(status).await
    }

    async fn terminate(&mut self, grace: Duration) -> Result<Exit, ExecError> {
        if let Some(exit) = self.exit {
            return Ok(exit);
        }
        sys::signal_group(self.pgid, Signal::SIGTERM);
        let status = match tokio::time::timeout(grace, self.child.wait()).await {
            Ok(status) => status,
            Err(_) => {
                kill_group(self.pgid, self.leaf.as_deref());
                self.child.wait().await
            }
        };
        self.finish(status).await
    }

    fn interrupt(&self) -> Result<(), ExecError> {
        nix::sys::signal::kill(nix::unistd::Pid::from_raw(self.pid), Signal::SIGINT).map_err(|e| ExecError::Io(format!("SIGINT: {e}")))
    }
}

impl Drop for HostChild {
    fn drop(&mut self) {
        if self.exit.is_some() {
            return;
        }
        kill_group(self.pgid, self.leaf.as_deref());
        let sandbox = Arc::clone(&self.sandbox);
        let pgid = self.pgid;
        let leaf = self.leaf.take();
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            rt.spawn(async move { sandbox.reap_group(pgid, leaf.as_deref()).await });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_buf_matches_captured_new() {
        let limits = CaptureLimits { head: 5, tail: 4 };
        let data: Vec<u8> = (0u8..40).collect();
        for chunk in [1usize, 3, 7, 40] {
            for len in [0usize, 4, 5, 6, 9, 10, 40] {
                let mut buf = CaptureBuf::new(limits);
                for piece in data[..len].chunks(chunk) {
                    buf.push(piece);
                }
                assert_eq!(buf.finish(), Captured::new(&data[..len], limits), "len {len} chunk {chunk}");
            }
        }
    }
}
