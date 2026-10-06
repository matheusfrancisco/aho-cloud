//! `FakeExecutor` / `FakeEnv`: in-memory `Executor` and `Env` for use-case tests.
//!
//! Real semantics where the port defines them without processes: confined in-memory files for
//! both areas (symlinks resolved lexically, absolute targets refused), `read` / `write` / `list` /
//! `export` / `put_file` / `make_dir`, git-style unified diffs against in-memory baselines
//! (`diff_against_base`), and `apply_patch` / `check_patch` through `mage_core::domain::fix::Patch`.
//! Processes are scripted: `exec` runs handlers installed with [`FakeEnv::on_exec`]; `spawn`
//! hands out children queued with [`FakeEnv::push_child`]. Prepare reports steps but runs no
//! build commands (the Prepared baseline equals the source tree). Every call is recorded.

use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use mage_core::domain::RunId;
use mage_core::domain::fix::Patch;
use mage_core::domain::recipe::{DbProvider, RecipeSpec};
use mage_core::ports::{Clock, SystemClock};
use parking_lot::Mutex;
use sha2::{Digest, Sha256};

use tokio::io::{AsyncRead, AsyncWrite, DuplexStream};
use tokio::sync::watch;

use crate::ports::executor::*;
use crate::ports::runner_state::RunnerState;

/// Arch of the machine running the tests.
pub fn host_arch() -> Arch {
    if cfg!(target_arch = "aarch64") { Arch::Aarch64 } else { Arch::X86_64 }
}

/// An `ExecOutcome` with default capture limits and `log = Exec(0)` (the fake sets the seq).
pub fn outcome(exit: Exit, stdout: &[u8], stderr: &[u8]) -> ExecOutcome {
    ExecOutcome {
        exit,
        stdout: Captured::new(stdout, CaptureLimits::default()),
        stderr: Captured::new(stderr, CaptureLimits::default()),
        duration: Duration::from_millis(1),
        log: LogSource::Exec(0),
    }
}

type ExecHandler = dyn Fn(&ExecSpec) -> Option<Result<ExecOutcome, ExecError>> + Send + Sync;
type EnvHook = dyn Fn(&Arc<FakeEnv>) + Send + Sync;

// ---------------------------------------------------------------------------------------------
// FakeExecutor
// ---------------------------------------------------------------------------------------------

struct FakeBase {
    base: BaseRef,
    tree: Tree,
    recipe: Option<RecipeSpec>,
}

#[derive(Default)]
struct ExecInner {
    bundles: BTreeMap<String, Vec<(String, Vec<u8>)>>,
    bases: Vec<FakeBase>,
    envs: Vec<Arc<FakeEnv>>,
    forks: Vec<ForkOptions>,
    prepare_count: u32,
    fail_prepare: VecDeque<ExecError>,
    fail_fork: VecDeque<ExecError>,
    hooks: Vec<Arc<EnvHook>>,
}

pub struct FakeExecutor {
    inner: Mutex<ExecInner>,
    prepare_lock: tokio::sync::Mutex<()>,
    state: Option<Arc<dyn RunnerState>>,
    next_port: Arc<AtomicU32>,
    clock: Arc<dyn Clock>,
}

/// Maximum concurrent envs the fake reports in capabilities.
const FAKE_MAX_ENVS: u16 = 4;

impl FakeExecutor {
    /// Without a `RunnerState`, `reap(Periodic)` treats every run as inactive.
    pub fn new() -> Arc<Self> {
        Self::build(None)
    }

    /// `reap(Periodic)` keeps envs whose run is in `state.active_runs()`.
    pub fn with_state(state: Arc<dyn RunnerState>) -> Arc<Self> {
        Self::build(Some(state))
    }

    fn build(state: Option<Arc<dyn RunnerState>>) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(ExecInner::default()),
            prepare_lock: tokio::sync::Mutex::new(()),
            state,
            next_port: Arc::new(AtomicU32::new(40_000)),
            clock: Arc::new(SystemClock),
        })
    }

    /// Content served for `PrepareRequest.source_bundle.url == url`. Unknown URLs prepare an
    /// empty tree.
    pub fn add_bundle(&self, url: &str, files: Vec<(String, Vec<u8>)>) {
        self.inner.lock().bundles.insert(url.to_owned(), files);
    }

    pub fn fail_next_prepare(&self, err: ExecError) {
        self.inner.lock().fail_prepare.push_back(err);
    }

    pub fn fail_next_fork(&self, err: ExecError) {
        self.inner.lock().fail_fork.push_back(err);
    }

    /// Bases actually prepared (cache hits not counted).
    pub fn prepare_count(&self) -> u32 {
        self.inner.lock().prepare_count
    }

    /// Options of every `fork` and `empty_env` call that produced an env, in order.
    pub fn forks(&self) -> Vec<ForkOptions> {
        self.inner.lock().forks.clone()
    }

    pub fn envs(&self) -> Vec<Arc<FakeEnv>> {
        self.inner.lock().envs.clone()
    }

    /// Called for every env created afterwards (install exec handlers, queue children, ...).
    pub fn on_env(&self, hook: impl Fn(&Arc<FakeEnv>) + Send + Sync + 'static) {
        self.inner.lock().hooks.push(Arc::new(hook));
    }

    fn live_envs(&self) -> Vec<Arc<FakeEnv>> {
        self.inner.lock().envs.iter().filter(|e| !e.destroyed()).cloned().collect()
    }

    fn create_env(&self, base: BaseRef, opts: ForkOptions, tree: Tree, recipe: Option<RecipeSpec>) -> Result<Arc<FakeEnv>, ExecError> {
        if let Some(err) = self.inner.lock().fail_fork.pop_front() {
            return Err(err);
        }
        if self.live_envs().len() >= usize::from(FAKE_MAX_ENVS) {
            return Err(ExecError::Unavailable("max_envs reached".into()));
        }
        let id = EnvId::from_uuid(uuid::Uuid::now_v7());
        let env = Arc::new(FakeEnv::new(id, base, opts, tree, recipe, Arc::clone(&self.next_port)));
        let hooks = {
            let mut inner = self.inner.lock();
            inner.envs.push(Arc::clone(&env));
            inner.forks.push(opts);
            inner.hooks.clone()
        };
        for hook in hooks {
            hook(&env);
        }
        Ok(env)
    }
}

#[async_trait]
impl Executor for FakeExecutor {
    async fn capabilities(&self) -> Result<ExecutorCapabilities, ExecError> {
        let live = self.live_envs().len() as u16;
        let bases = self.inner.lock().bases.iter().map(|b| b.base.key).filter(|k| k.recipe_hash.is_some()).collect();
        Ok(ExecutorCapabilities {
            kind: ExecutorKind::Host,
            arch: host_arch(),
            bases,
            free_env_slots: FAKE_MAX_ENVS.saturating_sub(live),
            disk_free_bytes: 100 << 30,
            disk_pressure: false,
            fork_mode: ForkMode::Copy,
            harness_placement: HarnessPlacement::InEnv,
            isolation: IsolationMode::Dev,
        })
    }

    async fn prepare(&self, req: PrepareRequest, obs: &dyn PrepareObserver) -> Result<BaseRef, ExecError> {
        if let Some(err) = self.inner.lock().fail_prepare.pop_front() {
            return Err(err);
        }
        let _single_flight = self.prepare_lock.lock().await;
        let key = BaseKey {
            org_id: req.org_id,
            repo_id: req.repo_id,
            sha: req.sha,
            recipe_hash: req.recipe.as_ref().map(RecipeSpec::hash),
            arch: host_arch(),
            executor: ExecutorKind::Host,
        };
        if let Some(existing) = self.inner.lock().bases.iter().find(|b| b.base.key == key) {
            return Ok(existing.base.clone());
        }
        if let Some(recipe) = &req.recipe {
            let report = recipe.validate();
            if !report.is_valid() {
                let issues: Vec<String> = report.errors.iter().map(|i| format!("{}: {}", i.path, i.message)).collect();
                return Err(ExecError::Invalid(format!("recipe: {}", issues.join("; "))));
            }
        }
        obs.step("source", StepStatus::Started);
        let mut tree = Tree::new();
        let files = self.inner.lock().bundles.get(&req.source_bundle.url).cloned().unwrap_or_default();
        for (path, bytes) in files {
            let rel = RelPath::new(&path)?;
            put_node(&mut tree, &rel, Node::File(bytes), Area::Workspace)?;
        }
        obs.step("source", StepStatus::Ok { secs: 0 });
        if let Some(recipe) = &req.recipe {
            for step in &recipe.build {
                obs.step(&step.name, StepStatus::Started);
                obs.step(&step.name, StepStatus::Ok { secs: 0 });
            }
        }
        let kind = if req.recipe.is_some() { BaseKind::Prepared } else { BaseKind::SourceOnly };
        let base = BaseRef { id: key.id(), key, kind, ready_at: self.clock.now() };
        let mut inner = self.inner.lock();
        inner.prepare_count += 1;
        inner.bases.push(FakeBase { base: base.clone(), tree, recipe: req.recipe });
        Ok(base)
    }

    async fn fork(&self, base: &BaseRef, opts: ForkOptions) -> Result<Arc<dyn Env>, ExecError> {
        let found = {
            let inner = self.inner.lock();
            inner.bases.iter().find(|b| b.base.id == base.id).map(|b| (b.base.clone(), b.tree.clone(), b.recipe.clone()))
        };
        let (base, tree, recipe) = found.ok_or_else(|| ExecError::NotFound(format!("base {}", base.id)))?;
        let env = self.create_env(base, opts, tree, recipe)?;
        Ok(env)
    }

    async fn empty_env(&self, opts: ForkOptions) -> Result<Arc<dyn Env>, ExecError> {
        let base = BaseRef::empty(host_arch(), ExecutorKind::Host, self.clock.now());
        let env = self.create_env(base, opts, Tree::new(), None)?;
        Ok(env)
    }

    async fn base(&self, key: &BaseKey) -> Result<Option<BaseRef>, ExecError> {
        Ok(self.inner.lock().bases.iter().find(|b| &b.base.key == key).map(|b| b.base.clone()))
    }

    async fn evict(&self, key: &BaseKey) -> Result<u64, ExecError> {
        if self.live_envs().iter().any(|e| &e.base.key == key) {
            return Err(ExecError::Conflict(format!("base {} has live forks", key.id())));
        }
        let mut inner = self.inner.lock();
        let before = inner.bases.len();
        let mut freed = 0u64;
        inner.bases.retain(|b| {
            if &b.base.key == key {
                freed += tree_bytes(&b.tree);
                false
            } else {
                true
            }
        });
        if inner.bases.len() == before {
            return Err(ExecError::NotFound(format!("base {}", key.id())));
        }
        Ok(freed)
    }

    async fn reap(&self, scope: ReapScope) -> Result<ReapReport, ExecError> {
        let active: HashSet<RunId> = match (scope, &self.state) {
            (ReapScope::Periodic, Some(state)) => {
                state.active_runs().await.map_err(ExecError::from)?.into_iter().map(|r| r.run_id).collect()
            }
            _ => HashSet::new(),
        };
        let mut report = ReapReport::default();
        for env in self.live_envs() {
            if !active.contains(&env.opts.run_id) {
                env.destroy().await?;
                report.forks_destroyed += 1;
            }
        }
        Ok(report)
    }
}

// ---------------------------------------------------------------------------------------------
// In-memory tree
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Node {
    File(Vec<u8>),
    Dir,
    Symlink(String),
}

/// Keys are normalized relative paths (no root entry).
type Tree = BTreeMap<String, Node>;

fn tree_bytes(tree: &Tree) -> u64 {
    tree.values().map(|n| if let Node::File(b) = n { b.len() as u64 } else { 0 }).sum()
}

fn files_of(tree: &Tree) -> BTreeMap<&str, &[u8]> {
    tree.iter()
        .filter_map(|(k, n)| match n {
            Node::File(b) => Some((k.as_str(), b.as_slice())),
            _ => None,
        })
        .collect()
}

/// Resolves `rel` inside `tree`, following symlinks lexically (the last component only when
/// `follow_last`). Returns the resolved key ("" = area root).
fn resolve(tree: &Tree, area: Area, rel: &RelPath, follow_last: bool) -> Result<String, ExecError> {
    let escape = || ExecError::Confinement { area, path: rel.as_str().to_owned() };
    let mut pending: VecDeque<String> = rel.segments().map(str::to_owned).collect();
    let mut done: Vec<String> = Vec::new();
    let mut hops = 0;
    while let Some(seg) = pending.pop_front() {
        if seg == ".." {
            if done.pop().is_none() {
                return Err(escape());
            }
            continue;
        }
        if seg == "." || seg.is_empty() {
            continue;
        }
        done.push(seg);
        let key = done.join("/");
        let is_last = pending.is_empty();
        if let Some(Node::Symlink(target)) = tree.get(&key) {
            if is_last && !follow_last {
                break;
            }
            hops += 1;
            if hops > 40 {
                return Err(ExecError::Invalid(format!("too many symlinks in {rel}")));
            }
            if target.starts_with('/') {
                return Err(escape());
            }
            done.pop();
            for (i, part) in target.split('/').enumerate() {
                pending.insert(i, part.to_owned());
            }
        }
    }
    Ok(done.join("/"))
}

/// Creates missing ancestor directories of `key`; an ancestor that is a file → Conflict.
fn ensure_parents(tree: &mut Tree, key: &str) -> Result<(), ExecError> {
    let mut prefix = String::new();
    let parts: Vec<&str> = key.split('/').collect();
    for part in &parts[..parts.len().saturating_sub(1)] {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(part);
        match tree.get(&prefix) {
            None => {
                tree.insert(prefix.clone(), Node::Dir);
            }
            Some(Node::Dir) => {}
            Some(_) => return Err(ExecError::Conflict(format!("{prefix} is not a directory"))),
        }
    }
    Ok(())
}

fn put_node(tree: &mut Tree, rel: &RelPath, node: Node, area: Area) -> Result<String, ExecError> {
    let key = resolve(tree, area, rel, true)?;
    if key.is_empty() {
        return Err(ExecError::Conflict("area root is a directory".into()));
    }
    ensure_parents(tree, &key)?;
    tree.insert(key.clone(), node);
    Ok(key)
}

fn is_dir(tree: &Tree, key: &str) -> bool {
    key.is_empty() || matches!(tree.get(key), Some(Node::Dir))
}

// ---------------------------------------------------------------------------------------------
// FakeEnv
// ---------------------------------------------------------------------------------------------

/// A signal delivered to a fake child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FakeSignal {
    Interrupt,
    Terminate,
}

struct ChildShared {
    pid: u32,
    exit: watch::Sender<Option<Exit>>,
    signals: Mutex<Vec<FakeSignal>>,
}

/// Test side of a scripted child. `stdin` yields what the env process received; writes to
/// `stdout`/`stderr` appear on the child's streams (drop them for EOF).
pub struct FakeChildControl {
    pub stdin: DuplexStream,
    pub stdout: DuplexStream,
    pub stderr: DuplexStream,
    shared: Arc<ChildShared>,
}

impl FakeChildControl {
    /// Resolves the child's `wait`. Later calls are ignored.
    pub fn exit(&self, exit: Exit) {
        self.shared.exit.send_if_modified(|cur| {
            if cur.is_none() {
                *cur = Some(exit);
                true
            } else {
                false
            }
        });
    }

    pub fn signals(&self) -> Vec<FakeSignal> {
        self.shared.signals.lock().clone()
    }

    pub fn pid(&self) -> u32 {
        self.shared.pid
    }
}

struct PendingChild {
    stdin: DuplexStream,
    stdout: DuplexStream,
    stderr: DuplexStream,
    shared: Arc<ChildShared>,
}

struct FakeChild {
    stdin: Option<DuplexStream>,
    stdout: Option<DuplexStream>,
    stderr: Option<DuplexStream>,
    shared: Arc<ChildShared>,
}

#[async_trait]
impl EnvChild for FakeChild {
    fn pid(&self) -> u32 {
        self.shared.pid
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
        let mut rx = self.shared.exit.subscribe();
        let exit = rx.wait_for(Option::is_some).await.map_err(|_| ExecError::Io("fake child dropped".into()))?;
        Ok(exit.unwrap_or(Exit::Signal(9)))
    }

    async fn terminate(&mut self, grace: Duration) -> Result<Exit, ExecError> {
        self.shared.signals.lock().push(FakeSignal::Terminate);
        let mut rx = self.shared.exit.subscribe();
        if let Ok(Ok(exit)) = tokio::time::timeout(grace, rx.wait_for(Option::is_some)).await {
            return Ok(exit.unwrap_or(Exit::Signal(9)));
        }
        self.shared.exit.send_if_modified(|cur| {
            if cur.is_none() {
                *cur = Some(Exit::Signal(9));
                true
            } else {
                false
            }
        });
        Ok((*self.shared.exit.borrow()).unwrap_or(Exit::Signal(9)))
    }

    fn interrupt(&self) -> Result<(), ExecError> {
        self.shared.signals.lock().push(FakeSignal::Interrupt);
        Ok(())
    }
}

struct EnvState {
    destroyed: bool,
    ws: Tree,
    scratch: Tree,
    source: Option<Tree>,
    prepared: Option<Tree>,
    recipe: Option<RecipeSpec>,
    ports: PortMap,
    db: bool,
    services: BTreeMap<String, ServiceStatus>,
    failing: BTreeMap<String, String>,
    execs: Vec<ExecSpec>,
    spawns: Vec<SpawnSpec>,
    put_files: Vec<(EnvPath, FileAccess)>,
    made_dirs: Vec<(EnvPath, FileAccess)>,
    writes: Vec<EnvPath>,
    diff_queue: VecDeque<Result<PatchOutput, ExecError>>,
    apply_queue: VecDeque<Result<ApplyOutcome, ExecError>>,
    check_queue: VecDeque<Result<ApplyOutcome, ExecError>>,
    db_queue: VecDeque<Result<DbRows, ExecError>>,
    applied: Vec<(Vec<u8>, ApplyMode)>,
    logs: BTreeMap<String, Vec<String>>,
    built: Vec<String>,
    db_setups: u32,
    db_resets: u32,
    exec_seq: u64,
    live_children: Vec<Arc<ChildShared>>,
    pending_children: VecDeque<PendingChild>,
}

pub struct FakeEnv {
    id: EnvId,
    base: BaseRef,
    opts: ForkOptions,
    root: String,
    scratch_root: String,
    isolation: Mutex<IsolationMode>,
    placement: Mutex<HarnessPlacement>,
    state: Mutex<EnvState>,
    handlers: Mutex<Vec<Arc<ExecHandler>>>,
    next_port: Arc<AtomicU32>,
    next_pid: AtomicU32,
}

/// Maximum exec timeout the fake accepts (default `exec.max_timeout_s`), and for build, test, and
/// SCIP commands (recipe `timeout_s` ceiling), as `exec_host` does.
pub const FAKE_MAX_TIMEOUT: Duration = Duration::from_secs(900);
pub const FAKE_MAX_RECIPE_TIMEOUT: Duration = Duration::from_secs(3600);

impl FakeEnv {
    /// A standalone empty env (Dev, InEnv, root `/fake/ws`, scratch `/fake/scratch`).
    pub fn empty() -> Arc<Self> {
        let base = BaseRef::empty(host_arch(), ExecutorKind::Host, SystemClock.now());
        let opts = ForkOptions {
            run_id: RunId::from_uuid(uuid::Uuid::now_v7()),
            purpose: ForkPurpose::Triage,
            egress: EgressProfile::ServerOnly,
        };
        Arc::new(Self::new(EnvId::from_uuid(uuid::Uuid::now_v7()), base, opts, Tree::new(), None, Arc::new(AtomicU32::new(45_000))))
    }

    fn new(id: EnvId, base: BaseRef, opts: ForkOptions, tree: Tree, recipe: Option<RecipeSpec>, next_port: Arc<AtomicU32>) -> Self {
        let baseline = (base.kind != BaseKind::Empty).then(|| tree.clone());
        let mut scratch = Tree::new();
        scratch.insert("launch".into(), Node::Dir);
        let env = Self {
            id,
            base,
            opts,
            root: "/fake/ws".into(),
            scratch_root: "/fake/scratch".into(),
            isolation: Mutex::new(IsolationMode::Dev),
            placement: Mutex::new(HarnessPlacement::InEnv),
            state: Mutex::new(EnvState {
                destroyed: false,
                ws: tree,
                scratch,
                source: baseline.clone(),
                prepared: baseline,
                recipe: None,
                ports: PortMap::default(),
                db: false,
                services: BTreeMap::new(),
                failing: BTreeMap::new(),
                execs: Vec::new(),
                spawns: Vec::new(),
                put_files: Vec::new(),
                made_dirs: Vec::new(),
                writes: Vec::new(),
                diff_queue: VecDeque::new(),
                apply_queue: VecDeque::new(),
                check_queue: VecDeque::new(),
                db_queue: VecDeque::new(),
                applied: Vec::new(),
                logs: BTreeMap::new(),
                built: Vec::new(),
                db_setups: 0,
                db_resets: 0,
                exec_seq: 0,
                live_children: Vec::new(),
                pending_children: VecDeque::new(),
            }),
            handlers: Mutex::new(Vec::new()),
            next_port,
            next_pid: AtomicU32::new(50_000),
        };
        if let Some(recipe) = recipe {
            env.install_recipe(recipe);
        }
        env
    }

    fn install_recipe(&self, recipe: RecipeSpec) {
        let mut st = self.state.lock();
        let mut ports = BTreeMap::new();
        for name in &recipe.ports {
            if let Ok(port_name) = PortName::new(name) {
                let existing = st.ports.0.get(&port_name).copied();
                let addr = existing.unwrap_or_else(|| {
                    let port = self.next_port.fetch_add(1, Ordering::SeqCst) as u16;
                    std::net::SocketAddr::from(([127, 0, 0, 1], port))
                });
                ports.insert(port_name, addr);
            }
        }
        st.ports = PortMap(ports);
        st.db = recipe.database.as_ref().is_some_and(|d| d.provider == DbProvider::Runner);
        st.recipe = Some(recipe);
    }

    fn live(&self) -> Result<parking_lot::MutexGuard<'_, EnvState>, ExecError> {
        let st = self.state.lock();
        if st.destroyed { Err(ExecError::Destroyed) } else { Ok(st) }
    }

    pub fn set_isolation(&self, mode: IsolationMode) {
        *self.isolation.lock() = mode;
    }

    pub fn set_harness_placement(&self, placement: HarnessPlacement) {
        *self.placement.lock() = placement;
    }

    pub fn fork_options(&self) -> ForkOptions {
        self.opts
    }

    /// Installs an exec handler; handlers are tried newest first, the first `Some` wins.
    pub fn on_exec(&self, handler: impl Fn(&ExecSpec) -> Option<Result<ExecOutcome, ExecError>> + Send + Sync + 'static) {
        self.handlers.lock().push(Arc::new(handler));
    }

    /// Queues a child for the next `spawn` (FIFO).
    pub fn push_child(&self) -> FakeChildControl {
        let (child_in, ctrl_in) = tokio::io::duplex(64 * 1024);
        let (child_out, ctrl_out) = tokio::io::duplex(64 * 1024);
        let (child_err, ctrl_err) = tokio::io::duplex(64 * 1024);
        let shared = Arc::new(ChildShared {
            pid: self.next_pid.fetch_add(1, Ordering::SeqCst),
            exit: watch::channel(None).0,
            signals: Mutex::new(Vec::new()),
        });
        self.state.lock().pending_children.push_back(PendingChild {
            stdin: child_in,
            stdout: child_out,
            stderr: child_err,
            shared: Arc::clone(&shared),
        });
        FakeChildControl { stdin: ctrl_in, stdout: ctrl_out, stderr: ctrl_err, shared }
    }

    pub fn destroyed(&self) -> bool {
        self.state.lock().destroyed
    }

    pub fn execs(&self) -> Vec<ExecSpec> {
        self.state.lock().execs.clone()
    }

    pub fn spawns(&self) -> Vec<SpawnSpec> {
        self.state.lock().spawns.clone()
    }

    pub fn put_files(&self) -> Vec<(EnvPath, FileAccess)> {
        self.state.lock().put_files.clone()
    }

    pub fn made_dirs(&self) -> Vec<(EnvPath, FileAccess)> {
        self.state.lock().made_dirs.clone()
    }

    /// Paths written with `Env::write`.
    pub fn writes(&self) -> Vec<EnvPath> {
        self.state.lock().writes.clone()
    }

    /// Patches passed to `apply_patch`, with their mode.
    pub fn applied(&self) -> Vec<(Vec<u8>, ApplyMode)> {
        self.state.lock().applied.clone()
    }

    /// Lines appended with `record_mcp`.
    pub fn mcp_lines(&self) -> Vec<String> {
        self.state.lock().logs.get(&LogSource::Mcp.to_string()).cloned().unwrap_or_default()
    }

    /// Build steps run by `build` / `rebuild`, in order.
    pub fn built_steps(&self) -> Vec<String> {
        self.state.lock().built.clone()
    }

    pub fn db_setups(&self) -> u32 {
        self.state.lock().db_setups
    }

    pub fn db_resets(&self) -> u32 {
        self.state.lock().db_resets
    }

    /// `start_services` reports `name` as `Unhealthy { reason }`.
    pub fn fail_service(&self, name: &str, reason: &str) {
        self.state.lock().failing.insert(name.to_owned(), reason.to_owned());
    }

    pub fn script_diff(&self, result: Result<PatchOutput, ExecError>) {
        self.state.lock().diff_queue.push_back(result);
    }

    pub fn script_apply(&self, result: Result<ApplyOutcome, ExecError>) {
        self.state.lock().apply_queue.push_back(result);
    }

    pub fn script_check(&self, result: Result<ApplyOutcome, ExecError>) {
        self.state.lock().check_queue.push_back(result);
    }

    pub fn script_db(&self, result: Result<DbRows, ExecError>) {
        self.state.lock().db_queue.push_back(result);
    }

    /// Appends a line to a log source (e.g. service output for `logs`).
    pub fn push_log(&self, source: &LogSource, line: &str) {
        self.state.lock().logs.entry(source.to_string()).or_default().push(line.to_owned());
    }

    /// Sets a file directly (no confinement checks beyond path validity; parents created).
    pub fn set_file(&self, path: &EnvPath, bytes: &[u8]) {
        let mut st = self.state.lock();
        let tree = if path.area == Area::Workspace { &mut st.ws } else { &mut st.scratch };
        let key = path.rel.as_str().to_owned();
        if ensure_parents(tree, &key).is_ok() {
            tree.insert(key, Node::File(bytes.to_vec()));
        }
    }

    /// Current file bytes (symlinks followed within the area).
    pub fn file(&self, path: &EnvPath) -> Option<Vec<u8>> {
        let st = self.state.lock();
        let tree = if path.area == Area::Workspace { &st.ws } else { &st.scratch };
        let key = resolve(tree, path.area, &path.rel, true).ok()?;
        match tree.get(&key) {
            Some(Node::File(bytes)) => Some(bytes.clone()),
            _ => None,
        }
    }

    /// Creates a symlink at `path` pointing to `target` (as `ln -s target path` would).
    pub fn add_symlink(&self, path: &EnvPath, target: &str) {
        let mut st = self.state.lock();
        let tree = if path.area == Area::Workspace { &mut st.ws } else { &mut st.scratch };
        let key = path.rel.as_str().to_owned();
        if ensure_parents(tree, &key).is_ok() {
            tree.insert(key, Node::Symlink(target.to_owned()));
        }
    }

    fn render_with(&self, st: &EnvState, template: &str) -> Result<String, ExecError> {
        let slug = self.id.slug();
        render_template(template, |name| match name {
            "ws" => Some(self.root.clone()),
            "scratch" => Some(self.scratch_root.clone()),
            "db.url" if st.db => Some(format!("postgres://mf_{slug}:fake@127.0.0.1:5432/mf_{slug}")),
            "db.host" if st.db => Some("127.0.0.1".into()),
            "db.port" if st.db => Some("5432".into()),
            "db.name" | "db.user" if st.db => Some(format!("mf_{slug}")),
            "db.password" if st.db => Some("fake".into()),
            _ => {
                if let Some(port) = name.strip_prefix("port.") {
                    return st.ports.get(port).map(|a| a.port().to_string());
                }
                let service = name.strip_prefix("service.")?.strip_suffix(".url")?;
                let recipe = st.recipe.as_ref()?;
                let first = recipe.services.get(service)?.ports.iter().min()?;
                st.ports.get(first).map(|a| format!("http://127.0.0.1:{}", a.port()))
            }
        })
    }

    fn baseline<'a>(st: &'a EnvState, which: Baseline) -> Result<&'a Tree, ExecError> {
        let tree = match which {
            Baseline::Source => st.source.as_ref(),
            Baseline::Prepared => st.prepared.as_ref(),
        };
        tree.ok_or_else(|| ExecError::Unsupported("env has no baseline (empty env)".into()))
    }

    fn selected_services(st: &EnvState, which: &ServiceSelector) -> Result<Vec<String>, ExecError> {
        let Some(recipe) = &st.recipe else {
            return match which {
                ServiceSelector::All => Ok(Vec::new()),
                ServiceSelector::Named(names) if names.is_empty() => Ok(Vec::new()),
                ServiceSelector::Named(names) => Err(ExecError::NotFound(format!("service {}", names[0]))),
            };
        };
        match which {
            ServiceSelector::All => Ok(recipe.services.keys().cloned().collect()),
            ServiceSelector::Named(names) => {
                for name in names {
                    if !recipe.services.contains_key(name) {
                        return Err(ExecError::NotFound(format!("service {name}")));
                    }
                }
                Ok(names.clone())
            }
        }
    }
}

#[async_trait]
impl Env for FakeEnv {
    fn id(&self) -> EnvId {
        self.id
    }

    fn base(&self) -> &BaseRef {
        &self.base
    }

    fn root(&self) -> &str {
        &self.root
    }

    fn scratch_root(&self) -> &str {
        &self.scratch_root
    }

    fn ports(&self) -> PortMap {
        self.state.lock().ports.clone()
    }

    fn db_url(&self) -> Option<String> {
        let st = self.state.lock();
        st.db.then(|| {
            let slug = self.id.slug();
            format!("postgres://mf_{slug}:fake@127.0.0.1:5432/mf_{slug}")
        })
    }

    fn recipe(&self) -> Option<RecipeSpec> {
        self.state.lock().recipe.clone()
    }

    fn render(&self, template: &str) -> Result<String, ExecError> {
        let st = self.live()?;
        self.render_with(&st, template)
    }

    fn harness_placement(&self) -> HarnessPlacement {
        *self.placement.lock()
    }

    fn isolation(&self) -> IsolationMode {
        *self.isolation.lock()
    }

    async fn exec(&self, spec: ExecSpec, sink: Option<&dyn ExecSink>) -> Result<ExecOutcome, ExecError> {
        let seq = {
            let mut st = self.live()?;
            check_extra_env(&spec.env)?;
            let max = if matches!(spec.role, ProcessRole::Build | ProcessRole::Test | ProcessRole::Scip) { FAKE_MAX_RECIPE_TIMEOUT } else { FAKE_MAX_TIMEOUT };
            if spec.timeout.is_zero() || spec.timeout > max {
                return Err(ExecError::Invalid(format!("timeout must be in 1..={}s", max.as_secs())));
            }
            if let Command::Argv(argv) = &spec.command {
                if argv.is_empty() {
                    return Err(ExecError::Invalid("empty argv".into()));
                }
            }
            let tree = if spec.cwd.area == Area::Workspace { &st.ws } else { &st.scratch };
            let cwd = resolve(tree, spec.cwd.area, &spec.cwd.rel, true)?;
            if !is_dir(tree, &cwd) {
                return Err(ExecError::NotFound(format!("cwd {}", spec.cwd)));
            }
            st.execs.push(spec.clone());
            st.exec_seq += 1;
            st.exec_seq
        };
        let handlers: Vec<Arc<ExecHandler>> = self.handlers.lock().iter().rev().cloned().collect();
        let result = handlers.iter().find_map(|h| h(&spec)).unwrap_or_else(|| {
            let argv = spec.command.to_argv().join(" ");
            Ok(outcome(Exit::Code(127), b"", format!("fake: no exec handler for {argv}\n").as_bytes()))
        });
        let mut out = result?;
        out.log = LogSource::Exec(seq);
        if let Some(sink) = sink {
            for (stream, cap) in [(StdStream::Stdout, &out.stdout), (StdStream::Stderr, &out.stderr)] {
                sink.chunk(stream, &cap.head);
                sink.chunk(stream, &cap.tail);
            }
        }
        let mut st = self.state.lock();
        let log = st.logs.entry(LogSource::Exec(seq).to_string()).or_default();
        for cap in [&out.stdout, &out.stderr] {
            let text = String::from_utf8_lossy(&[cap.head.as_slice(), cap.tail.as_slice()].concat()).into_owned();
            log.extend(text.lines().map(str::to_owned));
        }
        Ok(out)
    }

    async fn spawn(&self, spec: SpawnSpec) -> Result<Box<dyn EnvChild>, ExecError> {
        let mut st = self.live()?;
        check_extra_env(&spec.env)?;
        if spec.argv.is_empty() {
            return Err(ExecError::Invalid("empty argv".into()));
        }
        let tree = if spec.cwd.area == Area::Workspace { &st.ws } else { &st.scratch };
        let cwd = resolve(tree, spec.cwd.area, &spec.cwd.rel, true)?;
        if !is_dir(tree, &cwd) {
            return Err(ExecError::NotFound(format!("cwd {}", spec.cwd)));
        }
        st.spawns.push(spec);
        let pending = st.pending_children.pop_front().ok_or_else(|| ExecError::Unavailable("fake: no child queued (push_child)".into()))?;
        st.live_children.push(Arc::clone(&pending.shared));
        Ok(Box::new(FakeChild {
            stdin: Some(pending.stdin),
            stdout: Some(pending.stdout),
            stderr: Some(pending.stderr),
            shared: pending.shared,
        }))
    }

    async fn read(&self, path: &EnvPath, range: ReadRange) -> Result<FileRead, ExecError> {
        let st = self.live()?;
        let tree = if path.area == Area::Workspace { &st.ws } else { &st.scratch };
        let key = resolve(tree, path.area, &path.rel, true)?;
        match tree.get(&key) {
            Some(Node::File(bytes)) => Ok(FileRead::of(bytes, range)),
            Some(_) => Err(ExecError::Invalid(format!("{path} is not a regular file"))),
            None if key.is_empty() => Err(ExecError::Invalid(format!("{path} is not a regular file"))),
            None => Err(ExecError::NotFound(path.to_string())),
        }
    }

    async fn write(&self, path: &EnvPath, bytes: &[u8], mode: WriteMode) -> Result<WriteReport, ExecError> {
        let mut st = self.live()?;
        let tree = if path.area == Area::Workspace { &mut st.ws } else { &mut st.scratch };
        let key = resolve(tree, path.area, &path.rel, true)?;
        if key.is_empty() {
            return Err(ExecError::Invalid("cannot write the area root".into()));
        }
        let existing = tree.get(&key).cloned();
        let (content, created) = match (existing, mode) {
            (Some(Node::Dir), _) => return Err(ExecError::Invalid(format!("{path} is a directory"))),
            (Some(_), WriteMode::Create) => return Err(ExecError::Conflict(format!("{path} exists"))),
            (Some(Node::File(mut old)), WriteMode::Append) => {
                old.extend_from_slice(bytes);
                (old, false)
            }
            (Some(_), _) => (bytes.to_vec(), false),
            (None, _) => (bytes.to_vec(), true),
        };
        ensure_parents(tree, &key)?;
        tree.insert(key, Node::File(content));
        st.writes.push(path.clone());
        Ok(WriteReport { bytes: bytes.len() as u64, created })
    }

    async fn list(&self, path: &EnvPath, opts: ListOpts) -> Result<Listing, ExecError> {
        if !(1..=4).contains(&opts.depth) {
            return Err(ExecError::Invalid("depth must be 1..=4".into()));
        }
        let st = self.live()?;
        let tree = if path.area == Area::Workspace { &st.ws } else { &st.scratch };
        let key = resolve(tree, path.area, &path.rel, true)?;
        match tree.get(&key) {
            Some(Node::Dir) => {}
            None if key.is_empty() => {}
            Some(_) => return Err(ExecError::Invalid("not a directory".into())),
            None => return Err(ExecError::NotFound(path.to_string())),
        }
        let prefix = if key.is_empty() { String::new() } else { format!("{key}/") };
        let mut entries = Vec::new();
        let mut truncated = false;
        for (k, node) in tree.range(prefix.clone()..) {
            if !k.starts_with(&prefix) {
                break;
            }
            let rest = &k[prefix.len()..];
            let depth = rest.split('/').count();
            if depth > usize::from(opts.depth) {
                continue;
            }
            if !opts.hidden && rest.split('/').any(|seg| seg.starts_with('.')) {
                continue;
            }
            if opts.glob.as_ref().is_some_and(|g| !g.matches(k)) {
                continue;
            }
            if entries.len() >= opts.max_entries as usize {
                truncated = true;
                break;
            }
            let (kind, bytes) = match node {
                Node::File(b) => (EntryKind::File, b.len() as u64),
                Node::Dir => (EntryKind::Dir, 0),
                Node::Symlink(t) => (EntryKind::Symlink, t.len() as u64),
            };
            entries.push(DirEntry { path: RelPath::new(k)?, kind, bytes });
        }
        Ok(Listing { entries, truncated })
    }

    async fn export(&self, path: &EnvPath, dest: &Path) -> Result<ExportReport, ExecError> {
        let bytes = {
            let st = self.live()?;
            let tree = if path.area == Area::Workspace { &st.ws } else { &st.scratch };
            let key = resolve(tree, path.area, &path.rel, true)?;
            match tree.get(&key) {
                Some(Node::File(bytes)) => bytes.clone(),
                Some(_) => return Err(ExecError::Invalid(format!("{path} is not a regular file"))),
                None => return Err(ExecError::NotFound(path.to_string())),
            }
        };
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::write(dest, &bytes).await?;
        Ok(ExportReport { bytes: bytes.len() as u64, sha256: Sha256::digest(&bytes).into() })
    }

    async fn put_file(&self, path: &EnvPath, bytes: &[u8], access: FileAccess) -> Result<WriteReport, ExecError> {
        if access == FileAccess::AgentCreateOnly {
            return Err(ExecError::Invalid("AgentCreateOnly applies to directories only".into()));
        }
        let mut st = self.live()?;
        let tree = if path.area == Area::Workspace { &mut st.ws } else { &mut st.scratch };
        let key = resolve(tree, path.area, &path.rel, false)?;
        if key.is_empty() || matches!(tree.get(&key), Some(Node::Dir)) {
            return Err(ExecError::Conflict(format!("{path} is a directory")));
        }
        let created = !tree.contains_key(&key);
        ensure_parents(tree, &key)?;
        tree.insert(key, Node::File(bytes.to_vec()));
        st.put_files.push((path.clone(), access));
        Ok(WriteReport { bytes: bytes.len() as u64, created })
    }

    async fn make_dir(&self, path: &EnvPath, access: FileAccess) -> Result<(), ExecError> {
        let mut st = self.live()?;
        let tree = if path.area == Area::Workspace { &mut st.ws } else { &mut st.scratch };
        let key = resolve(tree, path.area, &path.rel, false)?;
        if !key.is_empty() {
            match tree.get(&key) {
                Some(Node::Dir) => {}
                Some(_) => return Err(ExecError::Conflict(format!("{path} exists and is not a directory"))),
                None => {
                    ensure_parents(tree, &key)?;
                    tree.insert(key, Node::Dir);
                }
            }
        }
        st.made_dirs.push((path.clone(), access));
        Ok(())
    }

    async fn set_recipe(&self, spec: RecipeSpec) -> Result<(), ExecError> {
        {
            let st = self.live()?;
            if self.base.kind != BaseKind::SourceOnly {
                return Err(ExecError::Conflict("set_recipe needs a SourceOnly env".into()));
            }
            if st.recipe.as_ref() == Some(&spec) {
                return Ok(());
            }
        }
        let report = spec.validate();
        if !report.is_valid() {
            let issues: Vec<String> = report.errors.iter().map(|i| format!("{}: {}", i.path, i.message)).collect();
            return Err(ExecError::Invalid(format!("recipe: {}", issues.join("; "))));
        }
        self.install_recipe(spec);
        Ok(())
    }

    async fn build(&self, obs: &dyn PrepareObserver) -> Result<(), ExecError> {
        let mut st = self.live()?;
        let recipe = st.recipe.clone().ok_or_else(|| ExecError::Invalid("env has no recipe".into()))?;
        for step in &recipe.build {
            obs.step(&step.name, StepStatus::Started);
            st.built.push(step.name.clone());
            obs.step(&step.name, StepStatus::Ok { secs: 0 });
        }
        Ok(())
    }

    async fn rebuild(&self, scope: RebuildScope, obs: &dyn PrepareObserver) -> Result<RebuildReport, ExecError> {
        let mut st = self.live()?;
        let recipe = st.recipe.clone().ok_or_else(|| ExecError::Invalid("env has no recipe".into()))?;
        let (run, skipped) = rebuild_plan(&recipe, &scope);
        for step in &run {
            obs.step(&step.name, StepStatus::Started);
            st.built.push(step.name.clone());
            obs.step(&step.name, StepStatus::Ok { secs: 0 });
        }
        Ok(RebuildReport {
            steps_run: run.iter().map(|s| s.name.clone()).collect(),
            steps_skipped: skipped.iter().map(|s| s.name.clone()).collect(),
            migrated: recipe.database.as_ref().is_some_and(|d| d.migrate.is_some()),
        })
    }

    async fn setup_db(&self) -> Result<(), ExecError> {
        let mut st = self.live()?;
        if st.recipe.as_ref().and_then(|r| r.database.as_ref()).is_none() {
            return Err(ExecError::Invalid("recipe has no [database]".into()));
        }
        st.db_setups += 1;
        Ok(())
    }

    async fn reset_db(&self) -> Result<(), ExecError> {
        let mut st = self.live()?;
        if !st.db {
            return Err(ExecError::Unsupported("env has no runner database".into()));
        }
        st.db_resets += 1;
        Ok(())
    }

    async fn start_services(&self, which: ServiceSelector) -> Result<ServicesReport, ExecError> {
        let mut st = self.live()?;
        let names = Self::selected_services(&st, &which)?;
        let now = SystemClock.now();
        let recipe = st.recipe.clone();
        for name in &names {
            let state = match st.failing.get(name) {
                Some(reason) => ServiceState::Unhealthy { reason: reason.clone() },
                None => ServiceState::Healthy,
            };
            let ports = recipe
                .as_ref()
                .and_then(|r| r.services.get(name))
                .map(|s| s.ports.iter().filter_map(|p| Some((PortName::new(p).ok()?, st.ports.get(p)?))).collect())
                .unwrap_or_default();
            st.services.insert(name.clone(), ServiceStatus { name: name.clone(), state, ports, since: now, last_check: None });
        }
        let services: Vec<ServiceStatus> = names.iter().filter_map(|n| st.services.get(n).cloned()).collect();
        let all_healthy = services.iter().all(|s| s.state == ServiceState::Healthy);
        Ok(ServicesReport { services, all_healthy })
    }

    async fn health(&self) -> Result<HealthReport, ExecError> {
        let st = self.live()?;
        let now = SystemClock.now();
        let checks: Vec<HealthCheckResult> = st
            .services
            .values()
            .filter(|s| s.state != ServiceState::Stopped)
            .map(|s| HealthCheckResult {
                service: s.name.clone(),
                ok: s.state == ServiceState::Healthy,
                detail: format!("{:?}", s.state),
                at: now,
            })
            .collect();
        let all_ok = checks.iter().all(|c| c.ok);
        Ok(HealthReport { checks, all_ok })
    }

    async fn stop_services(&self, which: ServiceSelector) -> Result<(), ExecError> {
        let mut st = self.live()?;
        let names = Self::selected_services(&st, &which)?;
        for name in names {
            if let Some(s) = st.services.get_mut(&name) {
                s.state = ServiceState::Stopped;
            }
        }
        Ok(())
    }

    async fn logs(&self, q: LogQuery) -> Result<LogSlice, ExecError> {
        let st = self.live()?;
        let lines = st.logs.get(&q.source.to_string()).cloned().unwrap_or_default();
        let re = q
            .grep
            .as_deref()
            .map(|g| regex::RegexBuilder::new(g).size_limit(1 << 20).build())
            .transpose()
            .map_err(|e| ExecError::Invalid(format!("grep: {e}")))?;
        let mut offset = 0u64;
        let mut selected = Vec::new();
        for line in &lines {
            let start = offset;
            offset += line.len() as u64 + 1;
            if q.since.is_some_and(|c| start < c.0) {
                continue;
            }
            if re.as_ref().is_none_or(|re| re.is_match(line)) {
                selected.push(line.clone());
            }
        }
        let matched = u32::try_from(selected.len()).unwrap_or(u32::MAX);
        let keep = (q.tail as usize).min(selected.len());
        let truncated = keep < selected.len();
        let lines = selected.split_off(selected.len() - keep);
        Ok(LogSlice { lines, cursor: LogCursor(offset), truncated, matched })
    }

    async fn db_query(&self, q: DbQuery) -> Result<DbRows, ExecError> {
        let mut st = self.live()?;
        if !st.db {
            return Err(ExecError::Unsupported("env has no database".into()));
        }
        if q.sql.trim().is_empty() {
            return Err(ExecError::Invalid("empty sql".into()));
        }
        match st.db_queue.pop_front() {
            Some(result) => result,
            None => Ok(DbRows { columns: Vec::new(), rows: Vec::new(), row_count: 0, truncated: false }),
        }
    }

    async fn record_mcp(&self, line: &[u8]) -> Result<(), ExecError> {
        let mut st = self.live()?;
        st.logs.entry(LogSource::Mcp.to_string()).or_default().push(String::from_utf8_lossy(line).into_owned());
        Ok(())
    }

    async fn diff_against_base(&self, opts: &DiffOptions) -> Result<PatchOutput, ExecError> {
        let mut st = self.live()?;
        if let Some(scripted) = st.diff_queue.pop_front() {
            return scripted;
        }
        let base = Self::baseline(&st, opts.against)?;
        let out = memdiff::diff(&files_of(base), &files_of(&st.ws), &opts.exclude)?;
        Ok(out)
    }

    async fn apply_patch(&self, patch: &[u8], mode: ApplyMode) -> Result<ApplyOutcome, ExecError> {
        let mut st = self.live()?;
        st.applied.push((patch.to_vec(), mode));
        if let Some(scripted) = st.apply_queue.pop_front() {
            return scripted;
        }
        let parsed = match Patch::parse(patch) {
            Ok(p) => p,
            Err(e) => return Ok(memdiff::rejected(RejectReason::Malformed, e.to_string(), Vec::new())),
        };
        if mode == ApplyMode::Exact && files_of(Self::baseline(&st, Baseline::Prepared)?) != files_of(&st.ws) {
            return Ok(memdiff::rejected(RejectReason::BaseMismatch, "work tree differs from the prepared baseline".into(), Vec::new()));
        }
        let outcome = memdiff::apply(&parsed, &mut st.ws, mode, true)?;
        Ok(outcome)
    }

    async fn check_patch(&self, patch: &[u8], baseline: Baseline) -> Result<ApplyOutcome, ExecError> {
        let mut st = self.live()?;
        if let Some(scripted) = st.check_queue.pop_front() {
            return scripted;
        }
        let parsed = match Patch::parse(patch) {
            Ok(p) => p,
            Err(e) => return Ok(memdiff::rejected(RejectReason::Malformed, e.to_string(), Vec::new())),
        };
        let mut tree = Self::baseline(&st, baseline)?.clone();
        memdiff::apply(&parsed, &mut tree, ApplyMode::Exact, false)
    }

    async fn destroy(&self) -> Result<(), ExecError> {
        let mut st = self.state.lock();
        if st.destroyed {
            return Ok(());
        }
        st.destroyed = true;
        for child in st.live_children.drain(..) {
            child.exit.send_if_modified(|cur| {
                if cur.is_none() {
                    *cur = Some(Exit::Killed(KillReason::Destroyed));
                    true
                } else {
                    false
                }
            });
        }
        st.services.clear();
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// In-memory git-style diff
// ---------------------------------------------------------------------------------------------

mod memdiff {
    use super::*;

    const CONTEXT: usize = 3;
    /// Above this many cells the LCS table is skipped and the file becomes one replacement hunk.
    const LCS_MAX_CELLS: usize = 4_000_000;
    const EMPTY_BLOB: &str = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391";
    const ZERO_OID: &str = "0000000000000000000000000000000000000000";

    pub(super) fn rejected(reason: RejectReason, detail: String, paths: Vec<RelPath>) -> ApplyOutcome {
        ApplyOutcome::Rejected { reason, detail, paths }
    }

    fn oid(bytes: &[u8]) -> String {
        if bytes.is_empty() {
            return EMPTY_BLOB.to_owned();
        }
        let digest = Sha256::digest(bytes);
        hex::encode(&digest[..20])
    }

    fn is_binary(bytes: &[u8]) -> bool {
        bytes[..bytes.len().min(8000)].contains(&0)
    }

    pub(super) fn diff(base: &BTreeMap<&str, &[u8]>, cur: &BTreeMap<&str, &[u8]>, exclude: &[PathGlob]) -> Result<PatchOutput, ExecError> {
        let paths: BTreeSet<&str> = base.keys().chain(cur.keys()).copied().collect();
        let mut out = PatchOutput { patch: Vec::new(), files: Vec::new(), excluded: Vec::new(), binary: Vec::new() };
        for path in paths {
            let (old, new) = (base.get(path).copied(), cur.get(path).copied());
            if old == new {
                continue;
            }
            let rel = RelPath::new(path)?;
            if exclude.iter().any(|g| g.matches(path)) {
                out.excluded.push(rel);
                continue;
            }
            let change = match (old, new) {
                (None, Some(_)) => FileChange::Added,
                (Some(_), None) => FileChange::Deleted,
                _ => FileChange::Modified,
            };
            let binary = old.is_some_and(is_binary) || new.is_some_and(is_binary);
            out.files.push(PatchFile { path: rel.clone(), change, binary });
            if binary {
                out.binary.push(rel);
                continue;
            }
            write_file_patch(&mut out.patch, path, old, new);
        }
        if out.patch.len() > PATCH_MAX_BYTES {
            return Err(ExecError::Invalid("patch too large".into()));
        }
        Ok(out)
    }

    fn write_file_patch(out: &mut Vec<u8>, path: &str, old: Option<&[u8]>, new: Option<&[u8]>) {
        out.extend_from_slice(format!("diff --git a/{path} b/{path}\n").as_bytes());
        match (old, new) {
            (None, Some(new)) => {
                out.extend_from_slice(format!("new file mode 100644\nindex {ZERO_OID}..{}\n", oid(new)).as_bytes());
                if !new.is_empty() {
                    out.extend_from_slice(format!("--- /dev/null\n+++ b/{path}\n").as_bytes());
                    write_hunks(out, b"", new);
                }
            }
            (Some(old), None) => {
                out.extend_from_slice(format!("deleted file mode 100644\nindex {}..{ZERO_OID}\n", oid(old)).as_bytes());
                if !old.is_empty() {
                    out.extend_from_slice(format!("--- a/{path}\n+++ /dev/null\n").as_bytes());
                    write_hunks(out, old, b"");
                }
            }
            (Some(old), Some(new)) => {
                out.extend_from_slice(format!("index {}..{} 100644\n--- a/{path}\n+++ b/{path}\n", oid(old), oid(new)).as_bytes());
                write_hunks(out, old, new);
            }
            (None, None) => {}
        }
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Op {
        Equal(usize, usize),
        Del(usize),
        Ins(usize),
    }

    fn edit_script(a: &[&[u8]], b: &[&[u8]]) -> Vec<Op> {
        let (n, m) = (a.len(), b.len());
        if n.saturating_mul(m) > LCS_MAX_CELLS {
            return (0..n).map(Op::Del).chain((0..m).map(Op::Ins)).collect();
        }
        // lcs[i][j] = LCS length of a[i..] and b[j..].
        let width = m + 1;
        let mut lcs = vec![0u32; (n + 1) * width];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                lcs[i * width + j] =
                    if a[i] == b[j] { lcs[(i + 1) * width + j + 1] + 1 } else { lcs[(i + 1) * width + j].max(lcs[i * width + j + 1]) };
            }
        }
        let (mut i, mut j) = (0, 0);
        let mut ops = Vec::with_capacity(n + m);
        while i < n || j < m {
            if i < n && j < m && a[i] == b[j] {
                ops.push(Op::Equal(i, j));
                i += 1;
                j += 1;
            } else if j < m && (i == n || lcs[i * width + j + 1] >= lcs[(i + 1) * width + j]) {
                ops.push(Op::Ins(j));
                j += 1;
            } else {
                ops.push(Op::Del(i));
                i += 1;
            }
        }
        // Deletions before insertions within a change run, as git prints them.
        let mut k = 0;
        while k < ops.len() {
            if matches!(ops[k], Op::Equal(..)) {
                k += 1;
                continue;
            }
            let start = k;
            while k < ops.len() && !matches!(ops[k], Op::Equal(..)) {
                k += 1;
            }
            ops[start..k].sort_by_key(|op| match op {
                Op::Del(i) => (0, *i),
                Op::Ins(j) => (1, *j),
                Op::Equal(..) => (2, 0),
            });
        }
        ops
    }

    fn push_line(out: &mut Vec<u8>, prefix: u8, line: &[u8]) {
        out.push(prefix);
        out.extend_from_slice(line);
        if line.last() != Some(&b'\n') {
            out.extend_from_slice(b"\n\\ No newline at end of file\n");
        }
    }

    fn write_hunks(out: &mut Vec<u8>, old: &[u8], new: &[u8]) {
        let a: Vec<&[u8]> = old.split_inclusive(|&b| b == b'\n').collect();
        let b: Vec<&[u8]> = new.split_inclusive(|&b| b == b'\n').collect();
        let ops = edit_script(&a, &b);
        let changes: Vec<usize> = ops.iter().enumerate().filter(|(_, op)| !matches!(op, Op::Equal(..))).map(|(i, _)| i).collect();
        let mut groups: Vec<(usize, usize)> = Vec::new();
        for &c in &changes {
            match groups.last_mut() {
                Some((_, end)) if c <= *end + 2 * CONTEXT + 1 => *end = c,
                _ => groups.push((c, c)),
            }
        }
        for (first, last) in groups {
            let lo = first.saturating_sub(CONTEXT);
            let hi = (last + CONTEXT + 1).min(ops.len());
            let slice = &ops[lo..hi];
            // Old/new line positions where the hunk starts (0-based counts of lines before it).
            let (mut old_before, mut new_before) = (0usize, 0usize);
            for op in &ops[..lo] {
                match op {
                    Op::Equal(..) => {
                        old_before += 1;
                        new_before += 1;
                    }
                    Op::Del(_) => old_before += 1,
                    Op::Ins(_) => new_before += 1,
                }
            }
            let old_count = slice.iter().filter(|op| !matches!(op, Op::Ins(_))).count();
            let new_count = slice.iter().filter(|op| !matches!(op, Op::Del(_))).count();
            let range = |before: usize, count: usize| match count {
                0 => format!("{before},0"),
                1 => format!("{}", before + 1),
                n => format!("{},{n}", before + 1),
            };
            out.extend_from_slice(format!("@@ -{} +{} @@\n", range(old_before, old_count), range(new_before, new_count)).as_bytes());
            for op in slice {
                match *op {
                    Op::Equal(i, _) => push_line(out, b' ', a[i]),
                    Op::Del(i) => push_line(out, b'-', a[i]),
                    Op::Ins(j) => push_line(out, b'+', b[j]),
                }
            }
        }
    }

    /// Applies every file of `patch` to `tree` atomically (all or nothing); `write` = commit.
    pub(super) fn apply(patch: &Patch, tree: &mut Tree, mode: ApplyMode, write: bool) -> Result<ApplyOutcome, ExecError> {
        let mut results = Vec::new();
        for file in patch.files() {
            let path = file.path().as_str();
            let base = match tree.get(path) {
                Some(Node::File(bytes)) => Some(bytes.as_slice()),
                Some(_) => {
                    return Ok(rejected(RejectReason::DoesNotApply, format!("{path}: not a regular file"), vec![RelPath::new(path)?]));
                }
                None => None,
            };
            match file.apply(base, mode) {
                Ok(post) => results.push((path.to_owned(), post)),
                Err(e) => return Ok(rejected(RejectReason::DoesNotApply, e.to_string(), vec![RelPath::new(path)?])),
            }
        }
        let mut files = Vec::with_capacity(results.len());
        for (path, post) in results {
            files.push(RelPath::new(&path)?);
            if write {
                match post {
                    Some(bytes) => {
                        ensure_parents(tree, &path)?;
                        tree.insert(path, Node::File(bytes));
                    }
                    None => {
                        tree.remove(&path);
                    }
                }
            }
        }
        Ok(ApplyOutcome::Applied { files, patch_sha256: patch.sha256() })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn map<'a>(pairs: &[(&'a str, &'a [u8])]) -> BTreeMap<&'a str, &'a [u8]> {
            pairs.iter().copied().collect()
        }

        #[test]
        fn diff_round_trips_through_core_patch() {
            let old = b"a\nb\nc\nd\ne\nf\ng\nh\ni\nj\n".as_slice();
            let new = b"a\nB\nc\nd\ne\nf\ng\nh\ni\nj\nk".as_slice();
            let base = map(&[("x.txt", old), ("gone.txt", b"bye\n"), ("empty", b"")]);
            let cur = map(&[("x.txt", new), ("added.txt", b"hi\n"), ("empty2", b"")]);
            let out = diff(&base, &cur, &[]).unwrap();
            let patch = Patch::parse(&out.patch).unwrap();
            assert_eq!(patch.files().len(), 5);
            let mut tree: Tree = base.iter().map(|(k, v)| ((*k).to_owned(), Node::File(v.to_vec()))).collect();
            let ApplyOutcome::Applied { .. } = apply(&patch, &mut tree, ApplyMode::Exact, true).unwrap() else { panic!("rejected") };
            let after: BTreeMap<&str, &[u8]> = files_of(&tree);
            assert_eq!(after, cur);
        }

        #[test]
        fn separate_hunks_and_binary_and_excludes() {
            let old: Vec<u8> = (0..40).flat_map(|i| format!("{i}\n").into_bytes()).collect();
            let mut new = old.clone();
            new.splice(0..2, b"X\n".iter().copied());
            new.extend_from_slice(b"Y\n");
            let base = map(&[("f", &old), ("bin", b"\0a"), (".mage/x", b"1")]);
            let cur = map(&[("f", &new), ("bin", b"\0b"), (".mage/x", b"2")]);
            let out = diff(&base, &cur, &[PathGlob::new(".mage/**").unwrap()]).unwrap();
            let text = String::from_utf8(out.patch.clone()).unwrap();
            assert_eq!(text.matches("@@ ").count(), 2, "{text}");
            assert_eq!(out.binary, vec![RelPath::new("bin").unwrap()]);
            assert!(!text.contains("bin"));
            assert_eq!(out.excluded, vec![RelPath::new(".mage/x").unwrap()]);
            assert!(Patch::parse(&out.patch).is_ok());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::contract::executor::{self as contract, ExecutorFixture};

    struct FakeFixture {
        exec: Arc<FakeExecutor>,
    }

    #[async_trait]
    impl ExecutorFixture for FakeFixture {
        fn executor(&self) -> Arc<dyn Executor> {
            self.exec.clone()
        }

        async fn bundle(&self, files: &[(&str, &[u8])]) -> BundleSource {
            let mut hasher = Sha256::new();
            for (path, bytes) in files {
                hasher.update(path.as_bytes());
                hasher.update([0]);
                hasher.update(bytes);
            }
            let sha: [u8; 32] = hasher.finalize().into();
            let url = format!("fake://{}", hex::encode(sha));
            self.exec.add_bundle(&url, files.iter().map(|(p, b)| ((*p).to_owned(), b.to_vec())).collect());
            BundleSource { url, sha256: sha, bytes: files.iter().map(|(_, b)| b.len() as u64).sum() }
        }

        async fn symlink(&self, env: &dyn Env, path: &EnvPath, target: &str) {
            let fake = self.exec.envs().into_iter().find(|e| e.id() == env.id()).expect("env of this executor");
            fake.add_symlink(path, target);
        }

        fn processes(&self) -> bool {
            false
        }

        fn database(&self) -> bool {
            false
        }
    }

    #[tokio::test]
    async fn fake_executor_contract() {
        contract::run_core(&FakeFixture { exec: FakeExecutor::new() }).await;
    }

    #[tokio::test]
    async fn scripted_children_and_execs() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let env = FakeEnv::empty();
        env.on_exec(|spec| matches!(&spec.command, Command::Shell(c) if c == "npm test").then(|| Ok(outcome(Exit::Code(1), b"1 failing", b""))));
        let out = env.exec(ExecSpec::shell("npm test", ProcessRole::Test), None).await.unwrap();
        assert_eq!((out.exit, out.stdout.head.as_slice(), out.log.clone()), (Exit::Code(1), &b"1 failing"[..], LogSource::Exec(1)));
        assert_eq!(env.exec(ExecSpec::shell("other", ProcessRole::Exec), None).await.unwrap().exit, Exit::Code(127));

        let mut ctl = env.push_child();
        let mut child = env
            .spawn(SpawnSpec { argv: vec!["omp".into()], cwd: EnvPath::scratch("launch").unwrap(), env: vec![], role: ProcessRole::Harness })
            .await
            .unwrap();
        let mut stdin = child.take_stdin().unwrap();
        stdin.write_all(b"hello").await.unwrap();
        let mut got = [0u8; 5];
        ctl.stdin.read_exact(&mut got).await.unwrap();
        assert_eq!(&got, b"hello");
        ctl.stdout.write_all(b"event\n").await.unwrap();
        let mut out = child.take_stdout().unwrap();
        let mut line = [0u8; 6];
        out.read_exact(&mut line).await.unwrap();
        assert_eq!(&line, b"event\n");
        child.interrupt().unwrap();
        assert_eq!(child.terminate(Duration::from_millis(10)).await.unwrap(), Exit::Signal(9));
        assert_eq!(ctl.signals(), vec![FakeSignal::Interrupt, FakeSignal::Terminate]);
        assert!(matches!(env.spawn(SpawnSpec { argv: vec!["x".into()], cwd: EnvPath::scratch(".").unwrap(), env: vec![], role: ProcessRole::Harness }).await.err(), Some(ExecError::Unavailable(_))));

        let ctl2 = env.push_child();
        let mut waiting = env
            .spawn(SpawnSpec { argv: vec!["claude".into()], cwd: EnvPath::scratch("launch").unwrap(), env: vec![], role: ProcessRole::Harness })
            .await
            .unwrap();
        env.destroy().await.unwrap();
        assert_eq!(waiting.wait().await.unwrap(), Exit::Killed(KillReason::Destroyed));
        drop(ctl2);
    }
}
