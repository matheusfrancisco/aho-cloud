//! `HostEnv`: a fork of a Ready base (or an empty env) and every `Env` operation on it
//! (executor.md "Copy-on-write fork", "Destroy", "Database", "Baselines").

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use mage_core::domain::recipe::{DbProvider, RecipeSpec};
use parking_lot::Mutex;

use super::confine::AreaRoot;
use super::git::GitBaseline;
use super::logs::{self, LogWriter};
use super::postgres::DbCreds;
use super::proxy::{EgressPolicy, EgressProxy};
use super::sandbox::Sandbox;
use super::services::Services;
use super::{EgressMode, HostShared, cgroup, clone, ports, sys};
use crate::ports::executor::*;
use crate::ports::runner_state::{EnvResource, ForkRecord, ForkStatus, PortOwner};

/// Resource kinds recorded per fork (executor.md "Destroy").
pub(crate) mod kind {
    pub const DIR: &str = "dir";
    pub const CGROUP: &str = "cgroup";
    pub const PG_DB: &str = "pg_db";
    pub const PG_ROLE: &str = "pg_role";
    pub const COMPOSE: &str = "compose_project";
    pub const PORT: &str = "port";
    pub const PROXY: &str = "proxy";
}

pub(crate) struct HostEnv {
    sb: Arc<Sandbox>,
    shared: Arc<HostShared>,
    base: BaseRef,
    purpose: ForkPurpose,
    egress: EgressProfile,
    /// `None` for empty envs.
    git: Option<GitBaseline>,
    /// Base dir (logs of the prepare live there).
    base_dir: Option<PathBuf>,
    services: Services,
    proxy: Mutex<Option<EgressProxy>>,
    mcp_log: Mutex<Option<LogWriter>>,
    destroy_lock: tokio::sync::Mutex<bool>,
}

/// Egress policy of an env (Registries ∪ server origin, or the server origin only).
pub(crate) fn egress_policy(shared: &HostShared, profile: EgressProfile) -> EgressPolicy {
    let server = shared.cfg.server_host();
    let mut allow: Vec<String> = match profile {
        EgressProfile::Registries => shared.cfg.egress.registries.iter().map(|h| h.to_ascii_lowercase()).collect(),
        EgressProfile::ServerOnly => Vec::new(),
    };
    if let Some(host) = &server {
        allow.push(host.clone());
    }
    EgressPolicy { allow_hosts: allow, server_host: server, port: 443 }
}

/// Starts the per-env egress proxy on a leased port (egress mode `proxy`).
pub(crate) async fn start_proxy(sb: &Sandbox, profile: EgressProfile) -> Result<Option<EgressProxy>, ExecError> {
    if sb.host.cfg.egress.mode != EgressMode::Proxy {
        return Ok(None);
    }
    let lease = ports::lease(&sb.host, &sb.port_owner, &[None]).await?;
    let port = lease[0].port;
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port)))
        .await
        .map_err(|e| ExecError::Unavailable(format!("egress proxy bind {port}: {e}")))?;
    let proxy = EgressProxy::start(listener, egress_policy(&sb.host, profile), sb.log_path(&LogSource::Egress))
        .map_err(|e| ExecError::Unavailable(format!("egress proxy: {e}")))?;
    sb.vars.lock().proxy_port = Some(proxy.addr().port());
    Ok(Some(proxy))
}

/// Leases one port per declared recipe port name (missing ones only) into the sandbox vars.
pub(crate) async fn lease_recipe_ports(sb: &Sandbox, recipe: &RecipeSpec) -> Result<Vec<u16>, ExecError> {
    let missing: Vec<Option<PortName>> = {
        let vars = sb.vars.lock();
        recipe.ports.iter().filter(|p| vars.ports.get(p).is_none()).map(|p| PortName::new(p).map(Some)).collect::<Result<_, _>>()?
    };
    if missing.is_empty() {
        return Ok(Vec::new());
    }
    let leases = ports::lease(&sb.host, &sb.port_owner, &missing).await?;
    let mut vars = sb.vars.lock();
    let mut out = Vec::with_capacity(leases.len());
    for lease in leases {
        if let Some(name) = lease.name {
            vars.ports.0.insert(name, SocketAddr::from(([127, 0, 0, 1], lease.port)));
            out.push(lease.port);
        }
    }
    Ok(out)
}

/// Creates the per-sandbox directories a process context needs.
pub(crate) fn make_sandbox_dirs(sb: &Sandbox) -> Result<(), ExecError> {
    use std::os::unix::fs::PermissionsExt;
    let agent = sb.host.owners.agent;
    for sub in ["tree", "home", "tmp", "shm"] {
        let p = sb.dir.join(sub);
        std::fs::create_dir_all(&p)?;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(if sub == "tree" || sub == "home" { 0o755 } else { 0o700 }))?;
        if let Some((uid, gid)) = agent {
            std::os::unix::fs::lchown(&p, Some(uid), Some(gid))?;
        }
    }
    for sub in ["logs/svc", "logs/exec", "logs/build", "run", "state/volumes"] {
        std::fs::create_dir_all(sb.dir.join(sub))?;
    }
    std::fs::set_permissions(sb.dir.join("run"), std::fs::Permissions::from_mode(0o700))?;
    let scratch = sb.dir.join("scratch");
    std::fs::create_dir_all(&scratch)?;
    let root = AreaRoot::new(scratch, Area::Scratch, sb.host.owners);
    root.make_dir(&RelPath::root(), FileAccess::AgentCreateOnly)?;
    root.make_dir(&RelPath::new("launch")?, FileAccess::AgentReadOnly)?;
    root.make_dir(&RelPath::new("scip")?, FileAccess::AgentWritable)?;
    Ok(())
}

impl HostEnv {
    pub(crate) fn sandbox(&self) -> &Arc<Sandbox> {
        &self.sb
    }

    fn new_sandbox(shared: &Arc<HostShared>, env_id: EnvId, run_id: mage_core::domain::RunId, cgroup: Option<PathBuf>) -> Sandbox {
        Sandbox::new(
            Arc::clone(shared),
            env_id.to_string(),
            Some(env_id),
            Some(run_id),
            shared.layout.fork(&env_id),
            cgroup,
            PortOwner::Env(env_id),
            format!("mage-{}", env_id.slug()),
        )
    }

    async fn add_resource(shared: &HostShared, env: EnvId, kind: &str, value: impl Into<String>) -> Result<(), ExecError> {
        shared.state.add_resource(env, EnvResource::new(kind, value)).await.map_err(ExecError::from)
    }

    /// Guards shared by `fork` and `empty_env`: `runner.max_envs`, and at most
    /// `runner.max_sessions_per_repo` Session forks per (org, repo).
    async fn admit(shared: &HostShared, base: Option<&BaseKey>, opts: &ForkOptions) -> Result<(), ExecError> {
        let forks = shared.state.forks().await?;
        if forks.len() >= usize::from(shared.cfg.max_envs) {
            return Err(ExecError::Unavailable(format!("runner.max_envs ({}) reached", shared.cfg.max_envs)));
        }
        if opts.purpose == ForkPurpose::Session {
            if let Some(key) = base {
                let sessions = forks
                    .iter()
                    .filter(|f| f.purpose == ForkPurpose::Session)
                    .filter(|f| f.base.is_some_and(|b| b.org_id == key.org_id && b.repo_id == key.repo_id))
                    .count();
                if sessions >= usize::from(shared.cfg.max_sessions_per_repo) {
                    return Err(ExecError::Conflict(format!("a session fork for repo {} is already running", key.repo_id)));
                }
            }
        }
        Ok(())
    }

    pub(crate) async fn fork(shared: &Arc<HostShared>, base: &BaseRef, opts: ForkOptions) -> Result<Arc<Self>, ExecError> {
        let rec = shared.state.base(&base.key).await?.ok_or_else(|| ExecError::NotFound(format!("base {}", base.id)))?;
        if rec.status != crate::ports::runner_state::BaseStatus::Ready {
            return Err(ExecError::Conflict(format!("base {} is not ready", base.id)));
        }
        Self::admit(shared, Some(&base.key), &opts).await?;
        let base_dir = PathBuf::from(&rec.dir);
        let recipe = read_recipe(&base_dir)?;
        let env_id = EnvId::from_uuid(shared.ids.uuid_v7());
        let dir = shared.layout.fork(&env_id);
        shared
            .state
            .begin_fork(ForkRecord {
                env: env_id,
                base: Some(base.key),
                run_id: opts.run_id,
                purpose: opts.purpose,
                dir: dir.display().to_string(),
                status: ForkStatus::Creating,
                created_at: shared.clock.now(),
                resources: vec![EnvResource::new(kind::DIR, dir.display().to_string())],
            })
            .await?;
        let base_ref = shared.base_ref(&rec);
        let result = Self::build_fork(shared, env_id, opts, base_ref, base_dir, recipe, rec.template_db.clone()).await;
        match result {
            Ok(env) => Ok(env),
            Err(err) => {
                // Undo whatever was created; the error that matters is the original one.
                if let Err(cleanup) = teardown_from_state(shared, env_id).await {
                    tracing::warn!(env = %env_id, error = %cleanup, "fork cleanup failed");
                }
                Err(err)
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn build_fork(
        shared: &Arc<HostShared>,
        env_id: EnvId,
        opts: ForkOptions,
        base: BaseRef,
        base_dir: PathBuf,
        recipe: Option<RecipeSpec>,
        template_db: Option<String>,
    ) -> Result<Arc<Self>, ExecError> {
        let dir = shared.layout.fork(&env_id);
        std::fs::create_dir_all(&dir)?;
        // 2. Clone tree/, home/, state/volumes/.
        let agent = shared.owners.agent;
        let mode = shared.fork_mode;
        let allow_copy = shared.cfg.fork.allow_copy_fallback;
        let (src, dst) = (base_dir.clone(), dir.clone());
        let used = tokio::task::spawn_blocking(move || -> Result<ForkMode, ExecError> {
            let tree = clone::clone_dir(&src.join("tree"), &dst.join("tree"), mode, allow_copy, agent)?;
            clone::clone_dir(&src.join("home"), &dst.join("home"), mode, allow_copy, agent)?;
            std::fs::create_dir_all(dst.join("state"))?;
            clone::clone_dir(&src.join("state").join("volumes"), &dst.join("state").join("volumes"), mode, allow_copy, None)?;
            Ok(tree)
        })
        .await
        .map_err(|e| ExecError::Io(format!("clone task: {e}")))??;
        if used == ForkMode::Copy && mode != ForkMode::Copy {
            tracing::warn!(env = %env_id, "fork used a full copy: filesystem lacks reflink");
        }
        // 5. cgroup.
        let cg = match &shared.cgroup_root {
            Some(root) => {
                let cg = cgroup::create(root, &format!("env-{}", env_id.slug()), &shared.cfg.cgroup)?;
                Self::add_resource(shared, env_id, kind::CGROUP, cg.display().to_string()).await?;
                Some(cg)
            }
            None => None,
        };
        let sb = Arc::new(Self::new_sandbox(shared, env_id, opts.run_id, cg));
        // 4. baseline.git (alternates), scratch/, tmp/, shm/, logs/, run/.
        make_sandbox_dirs(&sb)?;
        let git = GitBaseline::fork_from(&base_dir.join("baseline.git"), dir.join("baseline.git"), dir.join("tree"), dir.join("run"))?;
        // 6. Ports (named + proxy), 7. database, 8. proxy.
        if let Some(recipe) = &recipe {
            sb.vars.lock().recipe = Some(recipe.clone());
            for port in lease_recipe_ports(&sb, recipe).await? {
                Self::add_resource(shared, env_id, kind::PORT, port.to_string()).await?;
            }
            if let Some(db) = &recipe.database {
                match db.provider {
                    DbProvider::Runner => {
                        if let Some(template) = template_db.as_deref() {
                            let base_id = BaseId::parse(template.trim_start_matches("mb_"))?;
                            let creds = shared.pg()?.create_fork(&env_id, Some(&base_id), &[], None).await?;
                            Self::add_resource(shared, env_id, kind::PG_DB, creds.database.clone()).await?;
                            Self::add_resource(shared, env_id, kind::PG_ROLE, creds.user.clone()).await?;
                            sb.vars.lock().db = Some(creds);
                        }
                    }
                    DbProvider::Service => {
                        if let Some(url) = &db.url {
                            let rendered = sb.render(url)?;
                            sb.vars.lock().service_db_url = Some(rendered);
                        }
                    }
                }
            }
            if recipe.services.values().any(|s| s.compose.is_some()) {
                Self::add_resource(shared, env_id, kind::COMPOSE, sb.compose_name.clone()).await?;
            }
        }
        let proxy = start_proxy(&sb, opts.egress).await?;
        let proxy_port = sb.vars.lock().proxy_port;
        if let Some(port) = proxy_port {
            Self::add_resource(shared, env_id, kind::PROXY, port.to_string()).await?;
        }
        let env = Arc::new(Self {
            sb,
            shared: Arc::clone(shared),
            base,
            purpose: opts.purpose,
            egress: opts.egress,
            git: Some(git),
            base_dir: Some(base_dir),
            services: Services::default(),
            proxy: Mutex::new(proxy),
            mcp_log: Mutex::new(None),
            destroy_lock: tokio::sync::Mutex::new(false),
        });
        env.write_env_json()?;
        shared.state.set_fork_status(env_id, ForkStatus::Ready).await?;
        shared.envs.lock().insert(env_id, Arc::downgrade(&env));
        Ok(env)
    }

    pub(crate) async fn empty(shared: &Arc<HostShared>, opts: ForkOptions) -> Result<Arc<Self>, ExecError> {
        Self::admit(shared, None, &opts).await?;
        let env_id = EnvId::from_uuid(shared.ids.uuid_v7());
        let dir = shared.layout.fork(&env_id);
        shared
            .state
            .begin_fork(ForkRecord {
                env: env_id,
                base: None,
                run_id: opts.run_id,
                purpose: opts.purpose,
                dir: dir.display().to_string(),
                status: ForkStatus::Creating,
                created_at: shared.clock.now(),
                resources: vec![EnvResource::new(kind::DIR, dir.display().to_string())],
            })
            .await?;
        let result: Result<Arc<Self>, ExecError> = async {
            let cg = match &shared.cgroup_root {
                Some(root) => {
                    let cg = cgroup::create(root, &format!("env-{}", env_id.slug()), &shared.cfg.cgroup)?;
                    Self::add_resource(shared, env_id, kind::CGROUP, cg.display().to_string()).await?;
                    Some(cg)
                }
                None => None,
            };
            let sb = Arc::new(Self::new_sandbox(shared, env_id, opts.run_id, cg));
            make_sandbox_dirs(&sb)?;
            let proxy = start_proxy(&sb, EgressProfile::ServerOnly).await?;
            let proxy_port = sb.vars.lock().proxy_port;
            if let Some(port) = proxy_port {
                Self::add_resource(shared, env_id, kind::PROXY, port.to_string()).await?;
            }
            let env = Arc::new(Self {
                sb,
                shared: Arc::clone(shared),
                base: BaseRef::empty(shared.arch, ExecutorKind::Host, shared.clock.now()),
                purpose: opts.purpose,
                egress: EgressProfile::ServerOnly,
                git: None,
                base_dir: None,
                services: Services::default(),
                proxy: Mutex::new(proxy),
                mcp_log: Mutex::new(None),
                destroy_lock: tokio::sync::Mutex::new(false),
            });
            env.write_env_json()?;
            shared.state.set_fork_status(env_id, ForkStatus::Ready).await?;
            shared.envs.lock().insert(env_id, Arc::downgrade(&env));
            Ok(env)
        }
        .await;
        if result.is_err() {
            if let Err(cleanup) = teardown_from_state(shared, env_id).await {
                tracing::warn!(env = %env_id, error = %cleanup, "empty env cleanup failed");
            }
        }
        result
    }

    fn write_env_json(&self) -> Result<(), ExecError> {
        let vars = self.sb.vars.lock().clone();
        let doc = serde_json::json!({
            "env_id": self.sb.label,
            "base": if self.base.kind == BaseKind::Empty { None } else { Some(&self.base.key) },
            "run_id": self.sb.run_id.map(|r| r.to_string()),
            "purpose": self.purpose.as_str(),
            "egress": self.egress,
            "ports": vars.ports.0.iter().map(|(k, v)| (k.as_str().to_owned(), v.port())).collect::<BTreeMap<_, _>>(),
            "proxy_port": vars.proxy_port,
            "db": vars.db.as_ref().map(|d| d.database.clone()),
            "created_at": self.shared.clock.now().format(&time::format_description::well_known::Rfc3339).unwrap_or_default(),
        });
        let body = serde_json::to_vec_pretty(&doc).map_err(|e| ExecError::Io(e.to_string()))?;
        std::fs::write(self.sb.dir.join("env.json"), body)?;
        Ok(())
    }

    fn live(&self) -> Result<(), ExecError> {
        self.sb.check_live()
    }

    async fn blocking<T: Send + 'static>(&self, f: impl FnOnce() -> Result<T, ExecError> + Send + 'static) -> Result<T, ExecError> {
        self.live()?;
        tokio::task::spawn_blocking(f).await.map_err(|e| ExecError::Io(format!("blocking task: {e}")))?
    }

    fn recipe_required(&self) -> Result<RecipeSpec, ExecError> {
        self.sb.vars.lock().recipe.clone().ok_or_else(|| ExecError::Invalid("env has no recipe".into()))
    }

    fn git(&self) -> Result<&GitBaseline, ExecError> {
        self.git.as_ref().ok_or_else(|| ExecError::Unsupported("empty env has no baseline".into()))
    }

    /// Runs one recipe command (build step, migrate, seed) with observer reporting.
    async fn run_step(
        &self,
        step: &str,
        command: String,
        cwd: &str,
        timeout_s: u32,
        env: Option<&BTreeMap<String, String>>,
        role: ProcessRole,
        obs: &dyn PrepareObserver,
    ) -> Result<(), ExecError> {
        run_recipe_step(&self.sb, step, command, cwd, timeout_s, env, role, obs, &self.sb.log_path(&LogSource::Build(step.to_owned()))).await
    }

    async fn migrate_and_seed(&self, recipe: &RecipeSpec, seed: bool, obs: &dyn PrepareObserver) -> Result<bool, ExecError> {
        let Some(db) = &recipe.database else { return Ok(false) };
        let mut migrated = false;
        if let Some(migrate) = &db.migrate {
            let cmd = self.sb.render(migrate)?;
            self.run_step("migrate", cmd, &db.cwd, db.timeout_s, None, ProcessRole::Exec, obs).await?;
            migrated = true;
        }
        if seed {
            if let Some(s) = &db.seed {
                let cmd = self.sb.render(s)?;
                self.run_step("seed", cmd, &db.cwd, db.timeout_s, None, ProcessRole::Exec, obs).await?;
            }
        }
        Ok(migrated)
    }
}

/// Runs a recipe command as `role` in the workspace, streaming lines to `obs` and the step log.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_recipe_step(
    sb: &Sandbox,
    step: &str,
    command: String,
    cwd: &str,
    timeout_s: u32,
    env: Option<&BTreeMap<String, String>>,
    role: ProcessRole,
    obs: &dyn PrepareObserver,
    log_path: &Path,
) -> Result<(), ExecError> {
    struct LineSink<'a> {
        step: &'a str,
        obs: &'a dyn PrepareObserver,
        pending: Mutex<Vec<u8>>,
    }
    impl ExecSink for LineSink<'_> {
        fn chunk(&self, _stream: StdStream, data: &[u8]) {
            let mut pending = self.pending.lock();
            pending.extend_from_slice(data);
            while let Some(i) = pending.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = pending.drain(..=i).collect();
                self.obs.line(self.step, String::from_utf8_lossy(&line[..line.len() - 1]).trim_end_matches('\r'));
            }
            if pending.len() > 64 * 1024 {
                let line: Vec<u8> = std::mem::take(&mut *pending);
                self.obs.line(self.step, &String::from_utf8_lossy(&line));
            }
        }
    }
    obs.step(step, StepStatus::Started);
    let started = std::time::Instant::now();
    let mut spec = ExecSpec::shell(command, role);
    spec.cwd = EnvPath::ws(cwd)?;
    spec.timeout = std::time::Duration::from_secs(u64::from(timeout_s.max(1)));
    let log = LogWriter::open(log_path)?;
    let source = LogSource::Build(step.to_owned());
    let sink = LineSink { step, obs, pending: Mutex::new(Vec::new()) };
    let out = match sb.run(spec, env, Some(&sink), Some((source, log))).await {
        Ok(out) => out,
        Err(err) => {
            obs.step(step, StepStatus::Failed { exit: None, reason: err.to_string() });
            return Err(err);
        }
    };
    if out.exit.success() {
        obs.step(step, StepStatus::Ok { secs: u32::try_from(started.elapsed().as_secs()).unwrap_or(u32::MAX) });
        return Ok(());
    }
    let tail = log_tail(log_path, 4096);
    obs.step(step, StepStatus::Failed { exit: Some(out.exit), reason: format!("{}", out.exit) });
    Err(ExecError::PrepareFailed { step: step.to_owned(), exit: Some(out.exit), log_tail: tail })
}

/// Last `max` bytes of a log file, lossy UTF-8.
pub(crate) fn log_tail(path: &Path, max: usize) -> String {
    let bytes = std::fs::read(path).unwrap_or_default();
    String::from_utf8_lossy(&bytes[bytes.len().saturating_sub(max)..]).into_owned()
}

/// `recipe.toml` of a base dir (absent for SourceOnly bases).
pub(crate) fn read_recipe(base_dir: &Path) -> Result<Option<RecipeSpec>, ExecError> {
    match std::fs::read_to_string(base_dir.join("recipe.toml")) {
        Ok(text) => RecipeSpec::parse(&text).map(Some).map_err(|e| ExecError::Io(format!("base recipe: {e}"))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Destroy steps 3–9 driven by the fork's recorded resources (each removes its row on success,
/// so a crash mid-destroy resumes where it stopped). Processes must already be dead.
pub(crate) async fn teardown_from_state(shared: &HostShared, env: EnvId) -> Result<(), ExecError> {
    let Some(fork) = shared.state.forks().await?.into_iter().find(|f| f.env == env) else { return Ok(()) };
    let _ = shared.state.set_fork_status(env, ForkStatus::Destroying).await;
    let mut resources = fork.resources.clone();
    // Fixed order: compose, database, proxy, ports, dir, cgroup.
    let rank = |k: &str| match k {
        kind::COMPOSE => 0,
        kind::PG_DB => 1,
        kind::PG_ROLE => 2,
        kind::PROXY => 3,
        kind::PORT => 4,
        kind::DIR => 5,
        kind::CGROUP => 6,
        _ => 7,
    };
    resources.sort_by_key(|r| rank(&r.kind));
    let mut first_error: Option<ExecError> = None;
    for r in &resources {
        let result: Result<(), ExecError> = match r.kind.as_str() {
            kind::COMPOSE => shared.compose.down(&r.value).await.or_else(|e| match e {
                // No docker on this machine: nothing can be running.
                ExecError::Unavailable(m) if m == "docker" => Ok(()),
                e => Err(e),
            }),
            kind::PG_DB => match &shared.pg {
                Some(pg) => pg.drop_database_named(&r.value).await,
                None => Ok(()),
            },
            kind::PG_ROLE => match &shared.pg {
                Some(pg) => pg.drop_role_named(&r.value).await,
                None => Ok(()),
            },
            kind::PROXY | kind::PORT => shared.state.release_ports(&PortOwner::Env(env)).await.map_err(ExecError::from),
            kind::DIR => {
                let path = PathBuf::from(&r.value);
                sys::move_to_trash(&path, &shared.layout.trash()).map(|moved| {
                    if let Some(moved) = moved {
                        sys::remove_in_background(moved);
                    }
                })
            }
            kind::CGROUP => cgroup::destroy(Path::new(&r.value)),
            _ => Ok(()),
        };
        match result {
            Ok(()) => {
                let _ = shared.state.remove_resource(env, r).await;
            }
            Err(err) => {
                tracing::warn!(env = %env, kind = %r.kind, error = %err, "destroy step failed");
                first_error.get_or_insert(err);
            }
        }
    }
    shared.state.release_ports(&PortOwner::Env(env)).await?;
    if let Some(err) = first_error {
        return Err(err);
    }
    shared.state.remove_fork(env).await?;
    Ok(())
}

impl HostEnv {
    /// Destroy (executor.md): Destroying → kill everything → compose down → DB → proxy → ports →
    /// trash the dir → cgroup → remove_fork. Idempotent.
    pub(crate) async fn destroy_inner(&self) -> Result<(), ExecError> {
        let mut done = self.destroy_lock.lock().await;
        if *done {
            return Ok(());
        }
        let env_id = self.id();
        let _ = self.shared.state.set_fork_status(env_id, ForkStatus::Destroying).await;
        self.services.stop_all(&self.sb, None).await;
        self.sb.kill_all(KillReason::Destroyed, self.sb.kill_grace()).await;
        let proxy = self.proxy.lock().take();
        if let Some(proxy) = proxy {
            proxy.shutdown().await;
        }
        *self.mcp_log.lock() = None;
        teardown_from_state(&self.shared, env_id).await?;
        self.shared.envs.lock().remove(&env_id);
        *done = true;
        Ok(())
    }
}

#[async_trait]
impl Env for HostEnv {
    fn id(&self) -> EnvId {
        self.sb.env_id.unwrap_or_else(|| EnvId::from_uuid(uuid::Uuid::nil()))
    }

    fn base(&self) -> &BaseRef {
        &self.base
    }

    fn root(&self) -> &str {
        &self.sb.visible_ws
    }

    fn scratch_root(&self) -> &str {
        &self.sb.visible_scratch
    }

    fn ports(&self) -> PortMap {
        self.sb.vars.lock().ports.clone()
    }

    fn db_url(&self) -> Option<String> {
        let vars = self.sb.vars.lock();
        vars.db.as_ref().map(DbCreds::url).or_else(|| vars.service_db_url.clone())
    }

    fn recipe(&self) -> Option<RecipeSpec> {
        self.sb.vars.lock().recipe.clone()
    }

    fn render(&self, template: &str) -> Result<String, ExecError> {
        self.live()?;
        self.sb.render(template)
    }

    fn harness_placement(&self) -> HarnessPlacement {
        HarnessPlacement::InEnv
    }

    fn isolation(&self) -> IsolationMode {
        self.shared.cfg.isolation
    }

    async fn exec(&self, spec: ExecSpec, sink: Option<&dyn ExecSink>) -> Result<ExecOutcome, ExecError> {
        self.sb.run(spec, None, sink, None).await
    }

    async fn spawn(&self, spec: SpawnSpec) -> Result<Box<dyn EnvChild>, ExecError> {
        check_extra_env(&spec.env)?;
        let child = self.sb.spawn_child(spec.argv, &spec.cwd, None, &spec.env, spec.role).await?;
        Ok(Box::new(child))
    }

    async fn read(&self, path: &EnvPath, range: ReadRange) -> Result<FileRead, ExecError> {
        let root = self.sb.area(path.area).clone();
        let rel = path.rel.clone();
        self.blocking(move || root.read(&rel, range)).await
    }

    async fn write(&self, path: &EnvPath, bytes: &[u8], mode: WriteMode) -> Result<WriteReport, ExecError> {
        let root = self.sb.area(path.area).clone();
        let rel = path.rel.clone();
        let bytes = bytes.to_vec();
        self.blocking(move || root.write(&rel, &bytes, mode)).await
    }

    async fn list(&self, path: &EnvPath, opts: ListOpts) -> Result<Listing, ExecError> {
        let root = self.sb.area(path.area).clone();
        let rel = path.rel.clone();
        self.blocking(move || root.list(&rel, &opts)).await
    }

    async fn export(&self, path: &EnvPath, dest: &Path) -> Result<ExportReport, ExecError> {
        let root = self.sb.area(path.area).clone();
        let rel = path.rel.clone();
        let dest = dest.to_path_buf();
        self.blocking(move || root.export(&rel, &dest)).await
    }

    async fn put_file(&self, path: &EnvPath, bytes: &[u8], access: FileAccess) -> Result<WriteReport, ExecError> {
        let root = self.sb.area(path.area).clone();
        let rel = path.rel.clone();
        let bytes = bytes.to_vec();
        self.blocking(move || root.put_file(&rel, &bytes, access)).await
    }

    async fn make_dir(&self, path: &EnvPath, access: FileAccess) -> Result<(), ExecError> {
        let root = self.sb.area(path.area).clone();
        let rel = path.rel.clone();
        self.blocking(move || root.make_dir(&rel, access)).await
    }

    async fn set_recipe(&self, spec: RecipeSpec) -> Result<(), ExecError> {
        self.live()?;
        if self.base.kind != BaseKind::SourceOnly {
            return Err(ExecError::Conflict("set_recipe needs a SourceOnly env".into()));
        }
        let report = spec.validate();
        if !report.is_valid() {
            let issues: Vec<String> = report.errors.iter().map(|i| format!("{}: {}", i.path, i.message)).collect();
            return Err(ExecError::Invalid(format!("recipe: {}", issues.join("; "))));
        }
        let old = self.sb.vars.lock().recipe.clone();
        if old.as_ref() == Some(&spec) {
            return Ok(());
        }
        if let Some(old) = &old {
            self.services.stop_changed(&self.sb, old, &spec).await;
        }
        self.sb.vars.lock().recipe = Some(spec.clone());
        for port in lease_recipe_ports(&self.sb, &spec).await? {
            Self::add_resource(&self.shared, self.id(), kind::PORT, port.to_string()).await?;
        }
        {
            let mut vars = self.sb.vars.lock();
            let declared: Vec<String> = spec.ports.clone();
            vars.ports.0.retain(|name, _| declared.iter().any(|d| d == name.as_str()));
        }
        if spec.services.values().any(|s| s.compose.is_some()) {
            Self::add_resource(&self.shared, self.id(), kind::COMPOSE, self.sb.compose_name.clone()).await?;
        }
        let service_url = match spec.database.as_ref() {
            Some(db) if db.provider == DbProvider::Service => db.url.as_deref().map(|u| self.sb.render(u)).transpose()?,
            _ => None,
        };
        self.sb.vars.lock().service_db_url = service_url;
        self.write_env_json()
    }

    async fn build(&self, obs: &dyn PrepareObserver) -> Result<(), ExecError> {
        self.live()?;
        let recipe = self.recipe_required()?;
        check_toolchains(&self.shared, &recipe)?;
        for step in &recipe.build {
            let cmd = self.sb.render_build(&step.run)?;
            self.run_step(&step.name, cmd, &step.cwd, step.timeout_s, Some(&step.env), ProcessRole::Build, obs).await?;
        }
        Ok(())
    }

    async fn rebuild(&self, scope: RebuildScope, obs: &dyn PrepareObserver) -> Result<RebuildReport, ExecError> {
        self.live()?;
        let recipe = self.recipe_required()?;
        let (run, skipped) = rebuild_plan(&recipe, &scope);
        for step in &run {
            let cmd = self.sb.render_build(&step.run)?;
            self.run_step(&step.name, cmd, &step.cwd, step.timeout_s, Some(&step.env), ProcessRole::Build, obs).await?;
        }
        let has_db = self.sb.vars.lock().db.is_some() || self.sb.vars.lock().service_db_url.is_some();
        let migrated = if has_db { self.migrate_and_seed(&recipe, false, obs).await? } else { false };
        Ok(RebuildReport {
            steps_run: run.iter().map(|s| s.name.clone()).collect(),
            steps_skipped: skipped.iter().map(|s| s.name.clone()).collect(),
            migrated,
        })
    }

    async fn setup_db(&self) -> Result<(), ExecError> {
        self.live()?;
        let recipe = self.recipe_required()?;
        let db = recipe.database.clone().ok_or_else(|| ExecError::Invalid("recipe has no [database]".into()))?;
        let obs = NullObserver;
        match db.provider {
            DbProvider::Runner => {
                if self.sb.vars.lock().db.is_none() {
                    let env_id = self.id();
                    let creds = self.shared.pg()?.create_fork(&env_id, None, &db.extensions, None).await?;
                    Self::add_resource(&self.shared, env_id, kind::PG_DB, creds.database.clone()).await?;
                    Self::add_resource(&self.shared, env_id, kind::PG_ROLE, creds.user.clone()).await?;
                    self.sb.vars.lock().db = Some(creds);
                }
            }
            DbProvider::Service => {
                let service = db.service.clone().ok_or_else(|| ExecError::Invalid("database.service missing".into()))?;
                let report = self.services.start(&self.sb, ServiceSelector::Named(vec![service.clone()])).await?;
                if !report.all_healthy {
                    return Err(ExecError::ServiceUnhealthy {
                        service: service.clone(),
                        reason: "database service did not become healthy".into(),
                        log_tail: log_tail(&self.sb.log_path(&LogSource::Service(service)), 4096),
                    });
                }
            }
        }
        self.migrate_and_seed(&recipe, true, &obs).await?;
        self.write_env_json()
    }

    async fn reset_db(&self) -> Result<(), ExecError> {
        self.live()?;
        let recipe = self.recipe_required()?;
        let provider = recipe.database.as_ref().map(|d| d.provider);
        if provider != Some(DbProvider::Runner) {
            return Err(ExecError::Unsupported("reset_db needs provider = \"runner\"".into()));
        }
        let rec = self.shared.state.base(&self.base.key).await?;
        let template = rec.and_then(|r| r.template_db).ok_or_else(|| ExecError::Unsupported("reset_db needs a Prepared env".into()))?;
        let base_id = BaseId::parse(template.trim_start_matches("mb_"))?;
        let password = self.sb.vars.lock().db.as_ref().map(|d| d.password.clone());
        self.services.stop(&self.sb, ServiceSelector::All).await?;
        let pg = self.shared.pg()?;
        pg.drop_fork_db(&self.id()).await?;
        let creds = pg.create_fork(&self.id(), Some(&base_id), &[], password.as_deref()).await?;
        self.sb.vars.lock().db = Some(creds);
        let report = self.services.start(&self.sb, ServiceSelector::All).await?;
        if !report.all_healthy {
            let bad = report.services.iter().find(|s| s.state != ServiceState::Healthy);
            if let Some(bad) = bad {
                return Err(ExecError::ServiceUnhealthy {
                    service: bad.name.clone(),
                    reason: format!("{:?}", bad.state),
                    log_tail: log_tail(&self.sb.log_path(&LogSource::Service(bad.name.clone())), 4096),
                });
            }
        }
        Ok(())
    }

    async fn start_services(&self, which: ServiceSelector) -> Result<ServicesReport, ExecError> {
        self.services.start(&self.sb, which).await
    }

    async fn health(&self) -> Result<HealthReport, ExecError> {
        self.services.health(&self.sb).await
    }

    async fn stop_services(&self, which: ServiceSelector) -> Result<(), ExecError> {
        self.services.stop(&self.sb, which).await
    }

    async fn logs(&self, q: LogQuery) -> Result<LogSlice, ExecError> {
        self.live()?;
        let mut path = self.sb.log_path(&q.source);
        if !path.exists() {
            // Prepare and build logs of the base (a fork's own build writes to the fork).
            if let (Some(base_dir), LogSource::Prepare | LogSource::Build(_)) = (&self.base_dir, &q.source) {
                let rel = path.strip_prefix(&self.sb.dir).map(Path::to_path_buf).unwrap_or_default();
                path = base_dir.join(rel);
            }
        }
        tokio::task::spawn_blocking(move || logs::query(&path, &q)).await.map_err(|e| ExecError::Io(e.to_string()))?
    }

    async fn db_query(&self, q: DbQuery) -> Result<DbRows, ExecError> {
        self.live()?;
        let creds = {
            let vars = self.sb.vars.lock();
            match (&vars.db, &vars.service_db_url) {
                (Some(creds), _) => creds.clone(),
                (None, Some(url)) => creds_from_url(url)?,
                (None, None) => return Err(ExecError::Unsupported("env has no database".into())),
            }
        };
        self.shared.pg()?.query(&creds, &q).await
    }

    async fn record_mcp(&self, line: &[u8]) -> Result<(), ExecError> {
        self.live()?;
        let writer = {
            let mut slot = self.mcp_log.lock();
            match slot.as_ref() {
                Some(w) => w.clone(),
                None => {
                    let w = LogWriter::open(&self.sb.log_path(&LogSource::Mcp))?;
                    *slot = Some(w.clone());
                    w
                }
            }
        };
        let mut buf = Vec::with_capacity(line.len() + 1);
        buf.extend(line.iter().map(|&b| if b == b'\n' { b' ' } else { b }));
        buf.push(b'\n');
        writer.write(&buf);
        Ok(())
    }

    async fn diff_against_base(&self, opts: &DiffOptions) -> Result<PatchOutput, ExecError> {
        self.live()?;
        self.git()?.diff(opts.against, &opts.exclude).await
    }

    async fn apply_patch(&self, patch: &[u8], mode: ApplyMode) -> Result<ApplyOutcome, ExecError> {
        self.live()?;
        let outcome = self.git()?.apply(patch, mode).await?;
        if let (ApplyOutcome::Applied { files, .. }, Some((uid, gid))) = (&outcome, self.shared.owners.agent) {
            let tree = self.sb.dir.join("tree");
            for file in files {
                let path = tree.join(file.as_str());
                if std::fs::symlink_metadata(&path).is_ok() {
                    std::os::unix::fs::lchown(&path, Some(uid), Some(gid))?;
                }
            }
        }
        Ok(outcome)
    }

    async fn check_patch(&self, patch: &[u8], baseline: Baseline) -> Result<ApplyOutcome, ExecError> {
        self.live()?;
        self.git()?.check(patch, baseline).await
    }

    async fn destroy(&self) -> Result<(), ExecError> {
        self.destroy_inner().await
    }
}

/// `provider = "service"` Postgres URL → credentials for `db_query`.
fn creds_from_url(raw: &str) -> Result<DbCreds, ExecError> {
    let url = url::Url::parse(raw).map_err(|e| ExecError::Invalid(format!("database.url: {e}")))?;
    if !matches!(url.scheme(), "postgres" | "postgresql") {
        return Err(ExecError::Unsupported("db_query supports Postgres only".into()));
    }
    let decode = |s: &str| url::form_urlencoded::parse(format!("x={s}").as_bytes()).next().map(|(_, v)| v.into_owned()).unwrap_or_default();
    Ok(DbCreds {
        host: url.host_str().unwrap_or("127.0.0.1").to_owned(),
        port: url.port().unwrap_or(5432),
        user: decode(url.username()),
        password: url.password().map(decode).unwrap_or_default(),
        database: url.path().trim_start_matches('/').to_owned(),
    })
}

/// Recipe `[toolchain]` entries must exist in `[runner.toolchains]`.
pub(crate) fn check_toolchains(shared: &HostShared, recipe: &RecipeSpec) -> Result<(), ExecError> {
    for (tool, version) in &recipe.toolchain {
        let dir = shared.cfg.toolchains.get(tool).and_then(|v| v.get(version));
        match dir {
            Some(dir) if dir.is_dir() => {}
            Some(dir) => return Err(ExecError::Unavailable(format!("toolchain {tool} {version}: {} missing", dir.display()))),
            None => return Err(ExecError::Unavailable(format!("toolchain {tool} {version} is not configured on this runner"))),
        }
    }
    Ok(())
}
