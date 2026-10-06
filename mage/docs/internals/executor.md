# Executor, Run MCP, Playwright (`mage-runner`)

## Purpose

This doc covers everything on the runner that touches customer code:

- **Executor / Env**: prepare a base for `(repo, sha, recipe)`, fork it into disposable
  environments, run processes, files, services, the test database, and patches in a fork, then
  destroy it.
- **Recipe** `.mage/env.toml`: how a repo is built, booted, and tested. One file drives v1 (host)
  and v2 (aho).
- **Run MCP**: the per-run MCP server through which the harness acts on its fork and submits
  results.
- **BrowserRunner**: Playwright repro scripts with video and trace.
- **RunnerState**: local SQLite bookkeeping that makes crashes recoverable.
- **Verify runs**: how the Boot, Repro, and Fix gates execute on the runner.
- **v1 host isolation rules** and the outline of the v2 aho adapter.

The server side of runs, gates, and the runner ↔ server protocol is in [runs.md](runs.md). Harness
processes are in [harness.md](harness.md). Fix policy (exclusions, gate checks, PRs) is in
[fix.md](fix.md). Triage runs use only `empty_env` and `submit_triage` ([triage.md](triage.md)).

### Where the code lives

| Module | Contents |
|---|---|
| `mage_core::domain::recipe` | `RecipeSpec`, parse, `validate`, canonical form, `RecipeHash` (Additions) |
| `mage_core::wire` | `RunAssignment`, `VerifySpec`, `PreparedBaseKey`, capability DTOs (owned by [runs.md](runs.md)) |
| `mage_runner::ports::executor` | `Executor`, `Env`, `EnvChild`, and their types |
| `mage_runner::ports::browser` | `BrowserRunner` |
| `mage_runner::ports::runner_state` | `RunnerState` |
| `mage_runner::ports::run_mcp` | `RunMcpHost` (Additions) |
| `mage_runner::domain::observe` | pure observation extraction for `FailureSignature` comparison |
| `mage_runner::app::runner` | use cases: `session`, `verify`, `tools` (`RunTools`), `scip`, `recover`, `disk` |
| `mage_runner::adapters::driven::exec_host` | v1 `Executor` (this doc's main adapter) |
| `mage_runner::adapters::driven::exec_vm` | v2 `Executor` on `shared/vmm` (Firecracker; Docker backend in dev) |
| `mage_runner::adapters::driven::exec_ahocloud` | v2 `Executor` on aho-cloud's runtime-agent API (feature `ahocloud`) |
| `mage_runner::adapters::driven::playwright` | `BrowserRunner` |
| `mage_runner::adapters::driven::runner_state` | `RunnerState` on SQLite |
| `mage_runner::adapters::driving::mcp` | Run MCP (rmcp, streamable HTTP), implements `RunMcpHost` |
| `mage_runner::testing` | fakes: `FakeExecutor`/`FakeEnv`, `FakeBrowserRunner`, `MemRunnerState`, `FakeRunMcpHost` |

Dependency direction follows the README: `adapters → app → ports/domain`. `app::runner` never
imports `crate::adapters`. Inside `adapters::driven`, modules never import one another: the
harness adapters launch processes through `Env::spawn`, and `playwright` runs through `Env::exec`.
Neither touches `exec_host` internals.

External crates used by these modules (all in `mage-runner`):

- `exec_host`: `tokio` (process, fs), `nix` (setsid, unshare, mount, setres*id, prctl, killpg,
  rlimits), `reflink-copy`, `cap-std`, `reqwest`, `zstd`, `tar`, `sha2`, `sqlx` (postgres, for
  the runner-local Postgres), `hyper` (egress proxy), `regex`.
- `runner_state`: `sqlx` (sqlite) or `rusqlite` on a blocking pool.
- `mcp`: `rmcp` (streamable HTTP server), `axum`, `tower`, `schemars`, `jsonschema` (for
  `submit_triage`), `subtle`.
- `playwright`: `serde_json`.

Git, docker, Playwright, and SCIP indexers are external binaries run as subprocesses.

```mermaid
flowchart LR
  subgraph Runner["mage start runner (OS user mage)"]
    LL[driving::lease_loop]
    APP[app::runner<br/>session / verify / scip / recover]
    RT[app::runner::tools<br/>RunTools]
    MCP[driving::mcp<br/>Run MCP per run, loopback]
    EH[driven::exec_host<br/>Executor / Env]
    PW[driven::playwright<br/>BrowserRunner]
    RS[(driven::runner_state<br/>SQLite)]
    LK[driven::link<br/>ServerLink]
    PX[egress proxy<br/>listener per env]
  end
  subgraph EnvBox["Env: OS user mage-agent, mount ns, cgroup"]
    H[harness omp / claude]
    S[services]
    X[exec, build, test, node+playwright, scip]
  end
  PG[(runner-local Postgres<br/>template DBs)]
  DK[docker compose]
  SV[server /runner/v1]
  GCS[(GCS via signed URLs)]
  LL --> APP
  APP --> EH & LK & RS & MCP
  MCP --> RT --> EH & PW & LK
  PW --> EH
  EH --> EnvBox & PG & DK & RS & PX
  H -- bearer --> MCP
  X -- HTTPS_PROXY --> PX -- allowlist --> NET[(registries, server origin)]
  LK --> SV & GCS
```

## Ports defined here

All ports live in `mage_runner::ports`, use `#[async_trait]`, are `Send + Sync`, and are held as
`Arc<dyn Port>`. The runner crate depends on tokio, so the port signatures use tokio I/O traits.
IDs (`OrgId`, `RepoId`, `RunId`, `CommitSha`, `RecipeHash`, `ArtifactKind`, `FailureSignature`,
`RunKind`) come from `mage_core::domain`.

### Shared types (`ports::executor`)

```rust
pub struct EnvId(Uuid);                 // runner-local, via IdGen; Display "env_<base32>"
pub struct BaseId(String);              // hex(sha256(canonical BaseKey bytes))[..20]
pub enum ExecutorKind { Host, Aho }
pub enum Arch { X86_64, Aarch64 }

pub struct BaseKey {
    pub org_id: OrgId, pub repo_id: RepoId, pub sha: CommitSha,
    pub recipe_hash: Option<RecipeHash>,   // None = SourceOnly base
    pub arch: Arch, pub executor: ExecutorKind,
}
pub enum BaseKind { Empty, SourceOnly, Prepared }
pub struct BaseRef { pub key: BaseKey, pub id: BaseId, pub kind: BaseKind, pub ready_at: OffsetDateTime }

pub struct BundleSource { pub url: String /* signed GET */, pub sha256: [u8; 32], pub bytes: u64 }
pub struct PrepareRequest {
    pub org_id: OrgId, pub repo_id: RepoId, pub sha: CommitSha,
    pub recipe: Option<RecipeSpec>,       // None → SourceOnly (Learn without a validated recipe)
    pub source_bundle: BundleSource,
}
pub struct ForkOptions { pub run_id: RunId, pub purpose: ForkPurpose, pub egress: EgressProfile }
pub enum ForkPurpose { Session, Verify, Scip, Triage }
pub enum EgressProfile { Registries, ServerOnly }   // registries ∪ server origin, or server origin only

/// Validated relative path: '/'-separated, no leading '/', no '..' segment, no NUL, no '\',
/// no empty segments, ≤ 4096 bytes, leading "./" stripped. "." = area root.
pub struct RelPath(String);
pub enum Area { Workspace, Scratch }
pub struct EnvPath { pub area: Area, pub rel: RelPath }
impl EnvPath { pub fn ws(rel: &str) -> Result<Self, ExecError>; pub fn scratch(rel: &str) -> Result<Self, ExecError>; }

pub struct PortName(String);            // ^[a-z][a-z0-9_]{0,31}$
pub struct PortMap(pub BTreeMap<PortName, SocketAddr>);   // addresses reachable from the runner process
pub struct PathGlob(String);            // git pathspec glob syntax (`**`, `*`, `?`)

pub enum ProcessRole { Harness, Service(String), Exec, Build, Test, Playwright, Scip, Precheck }
```

### `Executor`

```rust
#[async_trait]
pub trait Executor: Send + Sync {
    async fn capabilities(&self) -> Result<ExecutorCapabilities, ExecError>;

    /// Idempotent and single-flight per BaseKey. Returns the Ready base, preparing it if absent.
    async fn prepare(&self, req: PrepareRequest, obs: &dyn PrepareObserver) -> Result<BaseRef, ExecError>;

    /// Fresh fork of a Ready base. Never forks a fork.
    async fn fork(&self, base: &BaseRef, opts: ForkOptions) -> Result<Arc<dyn Env>, ExecError>;

    /// Env with an empty workspace, no recipe, DB, or service ports. Same isolation as a fork.
    async fn empty_env(&self, opts: ForkOptions) -> Result<Arc<dyn Env>, ExecError>;

    async fn base(&self, key: &BaseKey) -> Result<Option<BaseRef>, ExecError>;
    /// Conflict if live forks reference the base.
    async fn evict(&self, key: &BaseKey) -> Result<u64 /* bytes freed */, ExecError>;
    async fn reap(&self, scope: ReapScope) -> Result<ReapReport, ExecError>;
}

pub struct ExecutorCapabilities {
    pub kind: ExecutorKind, pub arch: Arch,
    pub bases: Vec<BaseKey>,              // Ready + Prepared only (wire: PreparedBaseKey)
    pub free_env_slots: u16, pub disk_free_bytes: u64, pub disk_pressure: bool,
    pub fork_mode: ForkMode, pub harness_placement: HarnessPlacement,
    pub isolation: IsolationMode,
}
pub enum ForkMode { Clonefile, Reflink, Copy, Overlay }
pub enum HarnessPlacement { InEnv /* v1 host */, Host /* v2 aho: harness outside the VM */ }
pub enum IsolationMode { Enforced, Dev }
pub enum ReapScope { Boot, Periodic }
pub struct ReapReport {
    pub forks_destroyed: u32, pub processes_killed: u32, pub databases_dropped: u32,
    pub compose_projects_removed: u32, pub dirs_removed: u32, pub bases_evicted: u32, pub bytes_freed: u64,
}

pub trait PrepareObserver: Send + Sync {
    fn step(&self, step: &str, status: StepStatus);   // StepStatus = Started | Ok { secs: u32 } | Failed { exit: Option<Exit>, reason: String }
    fn line(&self, step: &str, line: &str);           // non-blocking; the receiver may drop lines
}
```

### `Env`

```rust
#[async_trait]
pub trait Env: Send + Sync {
    fn id(&self) -> EnvId;
    fn base(&self) -> &BaseRef;
    fn root(&self) -> &str;              // absolute workspace path as env processes see it
    fn scratch_root(&self) -> &str;      // absolute scratch path as env processes see it
    fn ports(&self) -> PortMap;          // current named-port leases (Learn may add ports)
    fn db_url(&self) -> Option<String>;  // URL env processes use (local, fake credentials)
    fn recipe(&self) -> Option<RecipeSpec>;
    fn render(&self, template: &str) -> Result<String, ExecError>;   // ${port.x}, ${db.url}, ...

    // processes
    async fn exec(&self, spec: ExecSpec, sink: Option<&dyn ExecSink>) -> Result<ExecOutcome, ExecError>;
    async fn spawn(&self, spec: SpawnSpec) -> Result<Box<dyn EnvChild>, ExecError>;

    // files, confined to the area root
    async fn read(&self, path: &EnvPath, range: ReadRange) -> Result<FileRead, ExecError>;
    async fn write(&self, path: &EnvPath, bytes: &[u8], mode: WriteMode) -> Result<WriteReport, ExecError>;
    async fn list(&self, path: &EnvPath, opts: ListOpts) -> Result<Listing, ExecError>;
    async fn export(&self, path: &EnvPath, dest: &Path) -> Result<ExportReport, ExecError>;
    /// Runner-owned helper files (harness config with per-run tokens) with explicit agent access.
    async fn put_file(&self, path: &EnvPath, bytes: &[u8], access: FileAccess) -> Result<WriteReport, ExecError>;
    async fn make_dir(&self, path: &EnvPath, access: FileAccess) -> Result<(), ExecError>;

    // recipe lifecycle
    async fn set_recipe(&self, spec: RecipeSpec) -> Result<(), ExecError>;   // SourceOnly-based envs only
    async fn build(&self, obs: &dyn PrepareObserver) -> Result<(), ExecError>;
    async fn rebuild(&self, scope: RebuildScope, obs: &dyn PrepareObserver) -> Result<RebuildReport, ExecError>;
    async fn setup_db(&self) -> Result<(), ExecError>;   // SourceOnly envs: create DB, extensions, migrate, seed
    async fn reset_db(&self) -> Result<(), ExecError>;   // Prepared envs: re-clone from the base template
    async fn start_services(&self, which: ServiceSelector) -> Result<ServicesReport, ExecError>;
    async fn health(&self) -> Result<HealthReport, ExecError>;
    async fn stop_services(&self, which: ServiceSelector) -> Result<(), ExecError>;
    async fn logs(&self, q: LogQuery) -> Result<LogSlice, ExecError>;
    async fn db_query(&self, q: DbQuery) -> Result<DbRows, ExecError>;

    // patches
    async fn diff_against_base(&self, opts: &DiffOptions) -> Result<PatchOutput, ExecError>;
    async fn apply_patch(&self, patch: &[u8], mode: ApplyMode) -> Result<ApplyOutcome, ExecError>;
    async fn check_patch(&self, patch: &[u8], baseline: Baseline) -> Result<ApplyOutcome, ExecError>;

    /// Idempotent. Afterwards every other method returns ExecError::Destroyed.
    async fn destroy(&self) -> Result<(), ExecError>;
}
```

Process types:

```rust
pub struct ExecSpec {
    pub command: Command,                // Shell(String) runs `/bin/sh -c`; Argv(Vec<String>) runs directly
    pub cwd: EnvPath,                    // default EnvPath::ws(".")
    pub env: Vec<(String, String)>,      // added on top of the env base set; reserved names → Invalid
    pub stdin: Option<Vec<u8>>,          // None = /dev/null
    pub timeout: Duration,               // required, ≤ config exec.max_timeout_s
    pub capture: CaptureLimits,          // per stream: head bytes + tail bytes kept in memory
    pub role: ProcessRole,
}
pub enum Command { Shell(String), Argv(Vec<String>) }
pub struct CaptureLimits { pub head: usize, pub tail: usize }       // default 8 KiB + 24 KiB
pub struct ExecOutcome { pub exit: Exit, pub stdout: Captured, pub stderr: Captured, pub duration: Duration, pub log: LogSource }
pub enum Exit { Code(i32), Signal(i32), TimedOut, Killed(KillReason) }
pub enum KillReason { Destroyed, Quota, Cancelled }
pub struct Captured { pub head: Vec<u8>, pub tail: Vec<u8>, pub total_bytes: u64 }   // tail empty when total ≤ head
pub enum StdStream { Stdout, Stderr }
pub trait ExecSink: Send + Sync { fn chunk(&self, stream: StdStream, data: &[u8]); }  // must not block

pub struct SpawnSpec { pub argv: Vec<String>, pub cwd: EnvPath, pub env: Vec<(String, String)>, pub role: ProcessRole }
#[async_trait]
pub trait EnvChild: Send {
    fn pid(&self) -> u32;
    fn take_stdin(&mut self) -> Option<Pin<Box<dyn AsyncWrite + Send>>>;
    fn take_stdout(&mut self) -> Option<Pin<Box<dyn AsyncRead + Send>>>;
    fn take_stderr(&mut self) -> Option<Pin<Box<dyn AsyncRead + Send>>>;
    async fn wait(&mut self) -> Result<Exit, ExecError>;
    async fn terminate(&mut self, grace: Duration) -> Result<Exit, ExecError>;  // TERM → grace → KILL
    fn interrupt(&self) -> Result<(), ExecError>;                               // SIGINT to the child pid only
}
```

File types:

```rust
pub enum ReadRange { Whole { max_bytes: u64 }, Lines { start: u32 /* 1-based */, count: u32, max_bytes: u64 }, Bytes { offset: u64, len: u64 } }
pub struct FileRead { pub bytes: Vec<u8>, pub total_bytes: u64, pub total_lines: Option<u32>, pub truncated: bool, pub binary: bool, pub sha256: [u8; 32] /* whole file */ }
pub enum WriteMode { Create /* Conflict if exists */, Overwrite, Append }
pub struct WriteReport { pub bytes: u64, pub created: bool }
pub struct ListOpts { pub depth: u8 /* 1..=4 */, pub glob: Option<PathGlob>, pub hidden: bool, pub respect_gitignore: bool, pub max_entries: u32 }
pub struct Listing { pub entries: Vec<DirEntry>, pub truncated: bool }
pub struct DirEntry { pub path: RelPath, pub kind: EntryKind /* File | Dir | Symlink */, pub bytes: u64 }
pub struct ExportReport { pub bytes: u64, pub sha256: [u8; 32] }
/// Enforced-mode meaning (dev mode: same modes, no boundary since everything runs as one user).
pub enum FileAccess {
    AgentReadOnly,    // owner mage, group mage-agent; file 0440, dir 0550: agent reads, cannot modify or replace
    AgentWritable,    // owner mage-agent; file 0600, dir 0700
    AgentCreateOnly,  // dir only: owner mage, group mage-agent, 1770 (sticky): agent creates its own files, cannot touch runner-owned ones
}
```

Service, log, and DB types:

```rust
pub enum ServiceSelector { All, Named(Vec<String>) }
pub struct ServicesReport { pub services: Vec<ServiceStatus>, pub all_healthy: bool }
pub struct ServiceStatus { pub name: String, pub state: ServiceState, pub ports: Vec<(PortName, SocketAddr)>, pub since: OffsetDateTime, pub last_check: Option<HealthCheckResult> }
pub enum ServiceState { Stopped, Starting, Healthy, Unhealthy { reason: String }, Exited { exit: Exit } }
pub struct HealthReport { pub checks: Vec<HealthCheckResult>, pub all_ok: bool }
pub struct HealthCheckResult { pub service: String, pub ok: bool, pub detail: String, pub at: OffsetDateTime }
pub enum RebuildScope { All, Changed(Vec<RelPath>) }
pub struct RebuildReport { pub steps_run: Vec<String>, pub steps_skipped: Vec<String>, pub migrated: bool }

pub enum LogSource { Prepare, Build(String), Service(String), Exec(u64), Mcp }
pub struct LogQuery { pub source: LogSource, pub tail: u32, pub grep: Option<String>, pub since: Option<LogCursor> }
pub struct LogCursor(pub u64);           // logical byte offset; survives rotation
pub struct LogSlice { pub lines: Vec<String>, pub cursor: LogCursor, pub truncated: bool, pub matched: u32 }

pub struct DbQuery { pub sql: String, pub params: Vec<DbValue>, pub write: bool, pub max_rows: u32, pub timeout: Duration }
pub enum DbValue { Null, Bool(bool), Int(i64), Float(f64), Text(String) }   // other types rendered as Text
pub struct DbColumn { pub name: String, pub type_name: String }
pub struct DbRows { pub columns: Vec<DbColumn>, pub rows: Vec<Vec<DbValue>>, pub row_count: u64, pub truncated: bool }
```

Patch types (shared with [fix.md](fix.md), which owns the exclusion policy):

```rust
pub enum Baseline { Source /* tree at sha */, Prepared /* after build + migrate */ }
pub struct DiffOptions { pub against: Baseline, pub exclude: Vec<PathGlob> }
pub struct PatchOutput { pub patch: Vec<u8>, pub files: Vec<PatchFile>, pub excluded: Vec<RelPath>, pub binary: Vec<RelPath> }
pub struct PatchFile { pub path: RelPath, pub change: FileChange, pub binary: bool }
pub enum FileChange { Added, Modified, Deleted, ModeChanged }
pub enum ApplyMode { Exact, AllowOffset }
pub enum ApplyOutcome {
    Applied { files: Vec<RelPath>, patch_sha256: [u8; 32] },
    Rejected { reason: RejectReason, detail: String, paths: Vec<RelPath> },
}
pub enum RejectReason { BaseMismatch, DoesNotApply, Malformed }
```

Error enum (one for `Executor` and `Env`):

```rust
#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error("not found: {0}")] NotFound(String),
    #[error("conflict: {0}")] Conflict(String),
    #[error("invalid: {0}")] Invalid(String),
    #[error("unavailable: {0}")] Unavailable(String),          // toolchain, Postgres, docker, disk
    #[error("path escapes {area:?}: {path}")] Confinement { area: Area, path: String },
    #[error("prepare step {step} failed")] PrepareFailed { step: String, exit: Option<Exit>, log_tail: String },
    #[error("service {service} unhealthy: {reason}")] ServiceUnhealthy { service: String, reason: String, log_tail: String },
    #[error("bundle rejected: {0}")] BundleRejected(String),     // sha256 mismatch, unsafe tar entry, size cap
    #[error("signed URL expired: {0}")] SourceUrlExpired(String), // caller refreshes via ServerLink and retries once
    #[error("quota exceeded: {0}")] QuotaExceeded(String),
    #[error("env destroyed")] Destroyed,
    #[error("unsupported: {0}")] Unsupported(String),
    #[error("io: {0}")] Io(String),
}
```

### `BrowserRunner` (`ports::browser`)

```rust
#[async_trait]
pub trait BrowserRunner: Send + Sync {
    async fn run(&self, env: &dyn Env, script: &BrowserScript, opts: &RecordOpts) -> Result<BrowserOutcome, BrowserError>;
}
pub struct BrowserScript { pub source: ScriptSource, pub grep: Option<String> /* test title filter */ }
pub enum ScriptSource { Workspace(RelPath), Inline { file_name: String, bytes: Vec<u8> } }
pub struct RecordOpts {
    pub video: bool, pub trace: bool, pub screenshots: ScreenshotMode, pub har: bool,
    pub timeout: Duration,               // whole invocation (globalTimeout)
    pub test_timeout: Duration,          // per test
    pub outbox: PathBuf,                 // runner-local dir; exported artifacts land here
    pub label: String,                   // artifact name prefix, e.g. "explore-3", "verify-r1"
}
pub enum ScreenshotMode { Off, OnFailure, On }
pub struct BrowserOutcome {
    pub passed: bool, pub status: BrowserStatus,
    pub error_text: Option<String>,      // first failure, ANSI stripped, ≤ 4 KiB
    pub failures: Vec<TestFailure>,
    pub video: Option<LocalArtifact>, pub trace: Option<LocalArtifact>,
    pub screenshots: Vec<LocalArtifact>, pub har: Option<LocalArtifact>, pub report: Option<LocalArtifact>,
    pub http_statuses: Vec<u16>,         // HAR statuses ≥ 400 plus `mage.http_status` annotations
    pub stdout_tail: String, pub duration: Duration,
}
pub enum BrowserStatus { Passed, Failed, TimedOut, Interrupted, NoTests, Crashed { exit: Exit } }
pub struct TestFailure { pub title: String, pub message: String, pub location: Option<String> }
pub struct LocalArtifact { pub kind: ArtifactKind, pub name: String, pub path: PathBuf, pub bytes: u64, pub sha256: [u8; 32], pub content_type: String }

#[derive(Debug, thiserror::Error)]
pub enum BrowserError {
    #[error("not found: {0}")] NotFound(String),     // script missing
    #[error("conflict: {0}")] Conflict(String),
    #[error("invalid: {0}")] Invalid(String),        // not a Playwright script kind
    #[error("unavailable: {0}")] Unavailable(String), // toolchain or browsers missing
    #[error(transparent)] Env(#[from] ExecError),
}
```

### `RunnerState` (`ports::runner_state`)

```rust
#[async_trait]
pub trait RunnerState: Send + Sync {
    // runs
    async fn begin_run(&self, run: ActiveRun) -> Result<(), RunnerStateError>;      // Conflict if present
    async fn set_phase(&self, run: RunId, phase: RunPhase) -> Result<(), RunnerStateError>;
    async fn attach_env(&self, run: RunId, env: EnvId) -> Result<(), RunnerStateError>;
    async fn end_run(&self, run: RunId) -> Result<(), RunnerStateError>;
    async fn active_runs(&self) -> Result<Vec<ActiveRun>, RunnerStateError>;

    // processes (pgid + start time guard against pid reuse)
    async fn record_process(&self, p: ProcessRecord) -> Result<(), RunnerStateError>;
    async fn clear_process(&self, pgid: i32) -> Result<(), RunnerStateError>;
    async fn processes(&self) -> Result<Vec<ProcessRecord>, RunnerStateError>;

    // bases
    async fn claim_base(&self, key: &BaseKey, staging_dir: &str) -> Result<BaseClaim, RunnerStateError>;
    async fn base_ready(&self, key: &BaseKey, meta: BaseMeta) -> Result<(), RunnerStateError>;
    async fn base_abandon(&self, key: &BaseKey) -> Result<(), RunnerStateError>;
    async fn base(&self, key: &BaseKey) -> Result<Option<BaseRecord>, RunnerStateError>;
    async fn bases(&self) -> Result<Vec<BaseRecord>, RunnerStateError>;
    async fn touch_base(&self, key: &BaseKey, at: OffsetDateTime) -> Result<(), RunnerStateError>;
    async fn remove_base(&self, key: &BaseKey) -> Result<(), RunnerStateError>;    // Conflict if live forks

    // forks and their adapter-owned resources
    async fn begin_fork(&self, f: ForkRecord) -> Result<(), RunnerStateError>;
    async fn set_fork_status(&self, env: EnvId, s: ForkStatus) -> Result<(), RunnerStateError>;
    async fn add_resource(&self, env: EnvId, r: EnvResource) -> Result<(), RunnerStateError>;
    async fn remove_resource(&self, env: EnvId, r: &EnvResource) -> Result<(), RunnerStateError>;
    async fn forks(&self) -> Result<Vec<ForkRecord>, RunnerStateError>;
    async fn remove_fork(&self, env: EnvId) -> Result<(), RunnerStateError>;

    // ports
    async fn lease_ports(&self, owner: &PortOwner, names: &[Option<PortName>], pool: PortRange) -> Result<Vec<PortLease>, RunnerStateError>;
    async fn quarantine_port(&self, port: u16, until: OffsetDateTime) -> Result<(), RunnerStateError>;
    async fn release_ports(&self, owner: &PortOwner) -> Result<(), RunnerStateError>;
    async fn port_leases(&self) -> Result<Vec<PortLease>, RunnerStateError>;

    // recovery
    async fn recovery_snapshot(&self) -> Result<RecoverySnapshot, RunnerStateError>;
}

pub struct ActiveRun { pub run_id: RunId, pub org_id: OrgId, pub kind: RunKind, pub attempt: u16, pub phase: RunPhase, pub env: Option<EnvId>, pub started_at: OffsetDateTime }
/// Local phase. Heartbeats send the wire `RunnerPhase` (runs.md): `Leased` sends nothing, the rest map 1:1.
pub enum RunPhase { Leased, Preparing, Running, AwaitingGate, Finishing /* Learn SCIP post-step + teardown */ }
pub struct ProcessRecord {
    pub pgid: i32, pub pid: i32,
    pub start_time: u64,                 // Linux /proc/<pid>/stat field 22; macOS proc start µs
    pub boot_id: String,                 // Linux /proc/sys/kernel/random/boot_id; macOS kern.boottime
    pub run_id: Option<RunId>, pub env: Option<EnvId>, pub role: ProcessRole, pub cgroup: Option<String>,
}
pub enum BaseClaim { Owned, Ready(BaseRecord), InProgress }
pub enum BaseStatus { Preparing, Ready }
pub struct BaseRecord { pub key: BaseKey, pub id: BaseId, pub kind: BaseKind, pub status: BaseStatus, pub dir: String, pub bytes: u64, pub created_at: OffsetDateTime, pub last_used_at: OffsetDateTime, pub live_forks: u32 }
pub struct BaseMeta { pub kind: BaseKind, pub dir: String, pub bytes: u64, pub fork_mode: ForkMode, pub template_db: Option<String> }
pub struct ForkRecord { pub env: EnvId, pub base: Option<BaseKey> /* None for empty_env */, pub run_id: RunId, pub purpose: ForkPurpose, pub dir: String, pub status: ForkStatus, pub created_at: OffsetDateTime, pub resources: Vec<EnvResource> }
pub enum ForkStatus { Creating, Ready, Destroying }
pub struct EnvResource { pub kind: String, pub value: String }   // adapter-defined; see host kinds below
pub enum PortOwner { Env(EnvId), Base(BaseId), RunMcp(RunId) }
pub struct PortRange { pub start: u16, pub end: u16 }
pub struct PortLease { pub port: u16, pub owner: PortOwner, pub name: Option<PortName>, pub at: OffsetDateTime }
pub struct RecoverySnapshot { pub runs: Vec<ActiveRun>, pub processes: Vec<ProcessRecord>, pub forks: Vec<ForkRecord>, pub preparing_bases: Vec<BaseRecord>, pub ports: Vec<PortLease> }

#[derive(Debug, thiserror::Error)]
pub enum RunnerStateError { NotFound, Conflict(String), Invalid(String), Unavailable(String) }
```

`lease_ports` is atomic: one SQLite transaction picks the lowest free, non-quarantined ports in
`pool` and inserts them. Callers bind-test each port. A port that fails the bind test goes to
`quarantine_port(port, now + 10 min)`, and the caller leases again.

SQLite schema (`<data_dir>/state.db`, WAL, `PRAGMA foreign_keys = ON`):

```sql
CREATE TABLE runs      (run_id TEXT PRIMARY KEY, org_id TEXT, kind TEXT, attempt INT, phase TEXT, env_id TEXT, started_at TEXT);
CREATE TABLE processes (pgid INT PRIMARY KEY, pid INT, start_time INT, boot_id TEXT, run_id TEXT, env_id TEXT, role TEXT, cgroup TEXT);
CREATE TABLE bases     (base_id TEXT PRIMARY KEY, key_json TEXT UNIQUE, kind TEXT, status TEXT, dir TEXT, bytes INT,
                        fork_mode TEXT, template_db TEXT, created_at TEXT, last_used_at TEXT);
CREATE TABLE forks     (env_id TEXT PRIMARY KEY, base_id TEXT REFERENCES bases(base_id), run_id TEXT, purpose TEXT,
                        dir TEXT, status TEXT, created_at TEXT);
CREATE TABLE resources (env_id TEXT REFERENCES forks(env_id) ON DELETE CASCADE, kind TEXT, value TEXT, PRIMARY KEY (env_id, kind, value));
CREATE TABLE ports     (port INT PRIMARY KEY, owner_kind TEXT, owner_id TEXT, name TEXT, at TEXT);
CREATE TABLE port_quarantine (port INT PRIMARY KEY, until TEXT);
```

`live_forks` is `SELECT count(*) FROM forks WHERE base_id = ?`. `remove_base` fails with `Conflict`
while that count is above 0.

### `RunMcpHost` (`ports::run_mcp`, Additions)

```rust
#[async_trait]
pub trait RunMcpHost: Send + Sync {
    async fn start(&self, tools: Arc<RunTools>) -> Result<Box<dyn RunMcpHandle>, RunMcpError>;
}
#[async_trait]
pub trait RunMcpHandle: Send + Sync {
    fn endpoint(&self) -> &RunMcpEndpoint;
    async fn shutdown(&self);            // graceful: stop accepting, cancel in-flight calls (kills their execs)
}
pub struct RunMcpEndpoint { pub url: String /* http://127.0.0.1:<port>/mcp */, pub bearer: String }
pub enum RunMcpError { Unavailable(String), Invalid(String), NotFound, Conflict(String) }
```

`RunTools` is the `app::runner::tools` use case: a concrete struct holding the run context (run id,
kind, attempt, `Arc<dyn Env>` or none, `Arc<dyn BrowserRunner>`, `Arc<dyn ServerLink>`, tool
set, gate state). The rmcp adapter only maps MCP calls to `RunTools` methods.

### Ports used

| Port | Defined in | Used for |
|---|---|---|
| `ServerLink` | `mage_runner::ports` ([runs.md](runs.md) owns signatures) | heartbeat with `phase`, batched events, command long-poll, artifact signed-upload URL + PUT, `POST /result`, `POST /finish`. Every call carries `attempt`; `409 lease_lost` → abort the run locally |
| `Harness` / `HarnessSession` | `mage_runner::ports` ([harness.md](harness.md)) | session runs; adapters launch through `Env::spawn` |
| `Clock`, `IdGen` | `mage_core::ports` | timestamps, `EnvId`, nonces |

## Recipe `.mage/env.toml`

The recipe lives in the repo at `.mage/env.toml`, or arrives as a Learn proposal. It is parsed
into `mage_core::domain::recipe::RecipeSpec` (serde with `deny_unknown_fields`). Parsing,
validation, canonical form, and hash are pure functions in `mage-core`, used by both the server
(Learn results, Boot gate, `run_recipe`) and the runner (`submit_recipe`, `services`, prepare).
design.md mentions `.mage/env.yaml`; TOML is the decided format.

### Schema

Top level:

| Key | Type | Req | Default | Notes |
|---|---|---|---|---|
| `version` | int | yes | — | `1` |
| `description` | string | no | — | ≤ 2 KiB; shown to agents and in the UI |
| `ports` | array of port names | no | `[]` | every named port the app listens on |
| `[toolchain]` | table name → version string | no | `{}` | e.g. `node = "20"`; matched against runner config `[runner.toolchains]` |
| `[env]` | table NAME → string | no | `{}` | applied to build, services, exec, test; fake values only |
| `[[build]]` | array of tables | no | `[]` | ordered build steps |
| `[database]` | table | no | — | test database |
| `[services.<name>]` | tables | no | `{}` | long-running processes or compose services |
| `[test]` | table | no | — | project test command (required by the Fix gate) |
| `[browser]` | table | no | — | Playwright base URL and test login |
| `[fakes.<name>]` | tables | no | `{}` | documents which service fakes which external dependency |
| `[[scip]]` | array of tables | no | runner defaults per detected language | SCIP indexer commands (Learn post-gate) |
| `[fix]` | table | no | — | owned by [fix.md](fix.md): `lint`, `lint_mode`, `generated`, `regression_dir` |

`[[build]]`:

| Key | Type | Req | Default | Notes |
|---|---|---|---|---|
| `name` | string | yes | — | `^[a-z][a-z0-9_-]{0,31}$`, unique |
| `run` | string | yes | — | shell command (`/bin/sh -c`) |
| `cwd` | rel path | no | `"."` | workspace-relative |
| `timeout_s` | int | no | `900` | `1..=3600` |
| `env` | table | no | `{}` | step-only additions |
| `inputs` | array of globs | no | `[]` | `Env::rebuild(Changed)` re-runs the step only if a changed path matches; empty = always re-run |

`[database]`:

| Key | Type | Req | Default | Notes |
|---|---|---|---|---|
| `provider` | `"runner"` \| `"service"` | no | `"runner"` | `runner` = runner-local Postgres with template fork; `service` = DB runs as a recipe service, data cloned with the fork |
| `engine` | `"postgres"` | no | `"postgres"` | v1 supports only Postgres for `provider = "runner"` |
| `version` | string | no | — | major version expected; mismatch with the runner's Postgres → warning |
| `name` | string | no | `"app"` | logical name; the physical name is per fork |
| `extensions` | array | no | `[]` | created by the runner admin before `migrate` |
| `migrate` | string | no | — | must be idempotent: it re-runs after a patch |
| `seed` | string | no | — | runs once in prepare; never re-run on a cloned DB |
| `cwd` | rel path | no | `"."` | |
| `timeout_s` | int | no | `600` | per command |
| `service` | string | if `service` | — | name of the service hosting the DB |
| `url` | template | if `service` | — | loopback URL, e.g. `postgres://app:app@127.0.0.1:${port.db}/app` |

`[services.<name>]` (name `^[a-z][a-z0-9_-]{0,31}$`, at most 16):

| Key | Type | Req | Default | Notes |
|---|---|---|---|---|
| `run` | string | one of | — | native process, shell command |
| `compose` | `{ file, service, ports = { <port> = <container_port> } }` | one of | — | docker compose service; `file` workspace-relative |
| `cwd` | rel path | no | `"."` | native only |
| `env` | table | no | `{}` | |
| `ports` | array of port names | no | `[]` | ports this service owns; every declared port has exactly one owner |
| `depends_on` | array of service names | no | `[]` | started and healthy first; acyclic |
| `stop_timeout_s` | int | no | `10` | TERM → wait → KILL |
| `[services.<name>.health]` | table | yes | — | exactly one of `http`, `tcp`, `command` |
| `health.http` | template URL | — | — | GET; loopback only |
| `health.expect_status` | int or array | no | `[200]` | for `http` |
| `health.tcp` | template `host:port` | — | — | connect succeeds |
| `health.command` | string | — | — | exit 0, run as `exec` with 10 s timeout |
| `health.interval_ms` | int | no | `1000` | |
| `health.timeout_ms` | int | no | `2000` | per probe |
| `health.start_period_s` | int | no | `120` | max time to first success |
| `health.retries` | int | no | `3` | consecutive failures after healthy → `Unhealthy` |

`[test]`: `run` (req), `cwd` (`"."`), `timeout_s` (`1800`), `env`.

`[browser]`: `base_url` (template; must reference a declared port), `[browser.login]` with `url`
(path or template URL), `username`, `password` (fake). Exposed to scripts as `MAGE_BASE_URL`,
`MAGE_LOGIN_URL`, `MAGE_LOGIN_USER`, `MAGE_LOGIN_PASSWORD`.

`[fakes.<name>]`: `service` (req, an existing service), `replaces` (hostname of the real
dependency, informational), `notes`. Fakes are services; this table explains them to agents and to
the fix brief.

`[[scip]]`: `language` (req), `run` (req; must contain `${scip.out}`), `cwd` (`"."`), `timeout_s`
(`1800`). The project root is always the repo root ([knowledge.md](knowledge.md) drops documents
whose paths are not repo-relative).

### Template variables

`${...}` is expanded by `Env::render`. `$${` is a literal `${`. Unknown variables fail validation.

| Variable | Value | Allowed in |
|---|---|---|
| `${ws}` | workspace root as env processes see it (`/mage/ws` enforced, real path dev) | everywhere (warning in `build`) |
| `${scratch}` | scratch root as env processes see it | everywhere |
| `${port.<name>}` | leased host port | services, health, `database.url`, `migrate`/`seed`, `test`, `browser`, `[env]` values |
| `${service.<name>.url}` | `http://127.0.0.1:<first port of service>` | same as `${port.*}` |
| `${db.url}`, `${db.host}`, `${db.port}`, `${db.name}`, `${db.user}`, `${db.password}` | this env's DB (base DB during prepare) | services, `migrate`/`seed`, `test`, `[env]` values |
| `${scip.out}` | output file path under scratch | `[[scip]].run` only |

Build steps never see ports or DB values. Prepare-time ports differ from fork ports, so anything
baked into build output would point at the wrong port.

### Example

```toml
version = 1
description = "Rails-free example: Node API + React web + Postgres + Redis"
ports = ["api", "web", "redis", "stripe"]

[toolchain]
node = "20"

[env]
NODE_ENV = "development"
DATABASE_URL = "${db.url}"
REDIS_URL = "redis://127.0.0.1:${port.redis}"
STRIPE_API_BASE = "${service.stripe-mock.url}"
STRIPE_SECRET_KEY = "sk_test_mage_fake_000000"
SESSION_SECRET = "mage-fake-session-secret"

[[build]]
name = "deps"
run = "npm ci"
inputs = ["package.json", "package-lock.json"]
timeout_s = 1200

[[build]]
name = "compile"
run = "npm run build"

[database]
provider = "runner"
version = "16"
extensions = ["pgcrypto"]
migrate = "npm run db:migrate"
seed = "npm run db:seed"

[services.redis]
compose = { file = "docker-compose.yml", service = "redis", ports = { redis = 6379 } }
ports = ["redis"]
[services.redis.health]
tcp = "127.0.0.1:${port.redis}"

[services.stripe-mock]
compose = { file = "docker-compose.yml", service = "stripe-mock", ports = { stripe = 12111 } }
ports = ["stripe"]
[services.stripe-mock.health]
tcp = "127.0.0.1:${port.stripe}"

[services.api]
run = "npm run start:api"
ports = ["api"]
depends_on = ["redis", "stripe-mock"]
env = { PORT = "${port.api}" }
[services.api.health]
http = "http://127.0.0.1:${port.api}/healthz"

[services.web]
run = "npm run start:web"
ports = ["web"]
depends_on = ["api"]
env = { PORT = "${port.web}", API_URL = "${service.api.url}" }
[services.web.health]
http = "http://127.0.0.1:${port.web}/"
start_period_s = 180

[test]
run = "npm test -- --runInBand"
timeout_s = 1800

[browser]
base_url = "http://127.0.0.1:${port.web}"
[browser.login]
url = "/login"
username = "support@example.test"
password = "mage-fake-password"

[fakes.stripe]
service = "stripe-mock"
replaces = "api.stripe.com"

[[scip]]
language = "typescript"
run = "scip-typescript index --output ${scip.out}"

[fix]
lint = "npm run lint"
lint_mode = "advisory"
generated = ["src/generated/**"]
```

### Validation rules

`RecipeSpec::validate(&self) -> RecipeReport { errors: Vec<RecipeIssue>, warnings: Vec<RecipeIssue> }`
is pure. `RecipeIssue { path: String /* e.g. "services.api.env.PORT" */, code: IssueCode, message: String }`.
Any error blocks `submit_recipe`, the Boot gate, and prepare. Warnings are returned to the agent
and stored with the recipe.

| Code | Rule |
|---|---|
| `Version` | `version == 1` |
| `TooLarge` | TOML ≤ 64 KiB; ≤ 32 build steps, 16 services, 32 ports, 64 env vars per table, 8 scip entries |
| `BadName` | port, service, build, and scip names match their regex; env keys match `^[A-Z_][A-Z0-9_]{0,127}$` |
| `PathNotRelative` | every path field (`cwd`, `compose.file`, `inputs`, `[fix]` paths) is relative, has no `..` segment, no `~`, no `\`, no NUL, and is not under `.git/` |
| `AbsoluteHostPath` | in `run`, `health.command`, and env values, any token (split on whitespace, `=`, `:`, quotes) starting with `/` must be under `/bin/`, `/usr/bin/`, or equal `/dev/null`. Use `${ws}` / `${scratch}`. URL fields are exempt for their path part |
| `HardcodedPort` | no `127.0.0.1:<digits>`, `localhost:<digits>`, `0.0.0.0:<digits>`, `[::1]:<digits>` in `run`, env values, `health`, `browser`, `database.url`. Use `${port.<name>}`. Compose container ports are exempt |
| `UnknownPort` | every `${port.x}` is declared in `ports` |
| `PortOwnership` | every declared port is owned by exactly one service; no service owns an undeclared port |
| `UnknownVariable` | only the variables in the table above, and only where allowed (e.g. no `${port.*}` in `[[build]]`) |
| `ServiceShape` | exactly one of `run` / `compose`; `health` present with exactly one probe; `compose.ports` keys ⊆ service `ports` |
| `DependsOn` | `depends_on` names exist; graph is acyclic |
| `ReservedEnv` | env keys are not `PATH`, `HOME`, `USER`, `LOGNAME`, `SHELL`, `TMPDIR`, `LD_*`, `DYLD_*`, `*_PROXY`/`*_proxy` (any case), `MAGE_*`, `ANTHROPIC_*`, `CLAUDE_*`, `PI_*`, `GOOGLE_APPLICATION_CREDENTIALS`, `AWS_*` |
| `LiveSecret` | no value anywhere matches a live-credential pattern: `sk_live_`, `rk_live_`, `AKIA[0-9A-Z]{16}`, `ghp_`, `gho_`, `ghs_`, `github_pat_`, `xox[abpr]-`, `sk-ant-`, `AIza[0-9A-Za-z_-]{35}`, `-----BEGIN [A-Z ]*PRIVATE KEY-----`, `"private_key":` |
| `NotFakeSecret` | env keys matching `(?i)(secret|token|password|passwd|api_?key|private_?key|credential|dsn)$` or `(?i)_KEY$` must have a value that is a template reference or contains (case-insensitive) `fake`, `test`, `mage`, `dummy`, `example`, `changeme`, or `localhost` |
| `ExternalUrl` (warning) | URL-shaped values whose host is not loopback or a template reference. The egress firewall will block them; use a fake |
| `Database` | `provider = "runner"` ⇒ `engine = "postgres"`, `service`/`url` absent; `provider = "service"` ⇒ `service` exists and `url` is loopback; extension names `^[a-z0-9_]+$` |
| `BrowserBaseUrl` | `browser.base_url` references a declared port |
| `ScipOut` | each `[[scip]].run` contains `${scip.out}` |
| `Timeouts` | every `timeout_s` in `1..=3600`; sum of build timeouts ≤ runner `prepare.max_total_s` (checked by the runner, not the domain) |
| `NoTest` (warning) | `[test]` missing: the Fix gate refuses runs for this recipe |
| `UnknownKey` | serde `deny_unknown_fields` on every table |

The host adapter adds checks of its own during prepare and returns them as
`ExecError::Unavailable`: requested toolchains exist in runner config, docker is present when
compose is used, and the Postgres major matches when `database.version` is set.

### Canonical form and `RecipeHash`

`RecipeHash = SHA-256(canonical_toml(spec))`, computed by `RecipeSpec::hash()` in `mage-core`
(`sha2` is allowed there). `Recipe.body` stores the original text, comments included, for display.
The canonical emitter is hand-written, so the hash does not depend on the `toml` crate's
formatting:

1. Parse to `RecipeSpec`; fill every default explicitly (`cwd = "."`, `timeout_s = 900`, ...).
2. Normalize paths: strip a leading `./`, collapse `//`, drop a trailing `/`.
3. Treat order-insensitive arrays as sets (sort and dedup): `ports`, `depends_on`, `extensions`,
   `inputs`, `services.*.ports`, `[fix].generated`, `health.expect_status`. Order-sensitive arrays
   keep their order: `[[build]]`, `[[scip]]`.
4. Emit tables in a fixed order (schema order above). Within maps (`env`, `services`, `fakes`,
   `toolchain`, `compose.ports`), sort keys bytewise.
5. Emit every string as a TOML basic string with minimal escaping (`\"`, `\\`, `\n`, `\t`,
   `\uXXXX` for other control characters). Integers in decimal. No comments, LF line endings, one
   trailing newline.
6. Hash the UTF-8 bytes.

A golden test pins the hash of a fixture recipe. Changing the emitter changes every hash, which
invalidates prepared bases (they are only a cache) and requires a migration that recomputes
`RecipeHash` for stored recipes.

## Use cases (`app::runner`)

### UC1 Prepare a base (`Executor::prepare`)

Callers: session runs (Learn with `recipe: None`, Diagnose/Fix with the Validated recipe), Verify
runs, and the Learn post-gate SCIP step.

1. Build `BaseKey` (arch and executor from the adapter). `RunnerState::claim_base(key, staging)`.
   - `Ready(rec)` → `touch_base`, return.
   - `InProgress` → wait on the in-process notifier for that key (prepare is single-flight inside
     one daemon), then re-check.
   - `Owned` → continue.
2. Disk check (UC9). Evict LRU bases if needed.
3. Download the SourceBundle with the signed URL into the bundle cache, verify sha256, extract
   into `staging/tree` (adapter: "Source bundle").
4. Baseline commit `refs/mage/source`.
5. `recipe: None` → mark Ready as `SourceOnly`, return.
6. Run `[[build]]` steps in order as `ProcessRole::Build` (registry egress). Report each step
   through `PrepareObserver::step` and stream lines to `<base>/logs/build/<step>.log`. A non-zero
   exit → `PrepareFailed { step, exit, log_tail }`.
7. Database (`provider = "runner"`): create the base DB and role, extensions, then `migrate` and
   `seed` with `${db.*}` = base DB. Prepare leases temporary ports for services under
   `PortOwner::Base`.
8. Start all services and wait for health (boot verification). Unhealthy →
   `ServiceUnhealthy { service, reason, log_tail }`.
9. Stop services cleanly. Commit baseline `refs/mage/prepared`. Convert the base DB to a template.
   Release the prepare ports.
10. Measure bytes. Rename staging to the final dir. `base_ready(key, meta)`. Return `BaseRef`.

Any failure: kill the prepare cgroup, drop the base DB, move staging to trash, `base_abandon`.

### UC2 Session run (Learn, Diagnose, Fix)

```mermaid
sequenceDiagram
  participant L as lease_loop
  participant A as app::runner::session
  participant X as Executor / Env
  participant M as Run MCP
  participant H as HarnessSession
  participant S as ServerLink
  L->>A: RunAssignment (attempt)
  A->>A: RunnerState.begin_run(Leased)
  A->>S: heartbeat phase=Preparing
  A->>X: prepare(req) → BaseRef
  A->>X: fork(base, Session) → Env
  A->>X: start_services(All) (Diagnose/Fix)
  A->>M: RunMcpHost.start(RunTools{kind, env})
  A->>H: start via Env::spawn (MCP: knowledge + run)
  A->>S: heartbeat phase=Running
  H->>M: tool calls (exec, read, playwright_run, ...)
  H->>M: submit_* → validate, upload, POST /result
  M-->>H: "submitted, gate running, end your turn"
  A->>A: set_phase(AwaitingGate)
  S-->>A: GateFeedback (long-poll /commands)
  alt Failed and attempts_left > 0
    A->>H: steer(feedback) → new turn
  else Passed or no attempts left
    A->>A: Learn+Passed: UC5 SCIP
    A->>A: UC7 teardown
  end
```

Steps:

1. `begin_run(ActiveRun { phase: Leased })`. Start the heartbeat task (extends the lease; carries
   `phase`).
2. `set_phase(Preparing)`. Choose the base:
   - Learn: `prepare(recipe: None)` → SourceOnly base.
   - Diagnose / Fix: `prepare(recipe: Some(validated))` → Prepared base.
3. `fork(base, ForkOptions { purpose: Session, egress: Registries })`, then `attach_env`.
4. Diagnose / Fix: `start_services(All)`. Unhealthy → teardown and
   `finish(Failed { stage: Prepare, reason: "base_does_not_boot", retryable: false })`. Either the
   recipe was validated on another sha or the runner drifted; [runs.md](runs.md) decides whether
   the recipe becomes `Broken`.
5. Fix only: write the pinned repro script (downloaded by signed URL, sha256 checked; on
   `SourceUrlExpired` refresh and retry once) to `.mage/repro/<file>`. [fix.md](fix.md)'s
   exclusion globs keep it out of the patch.
6. Lease a port (`PortOwner::RunMcp`), `RunMcpHost::start(RunTools { kind, env, ... })`, and get
   the `RunMcpEndpoint`.
7. Start the harness ([harness.md](harness.md)) through `Env::spawn` with
   `cwd = EnvPath::scratch("launch")`, the Knowledge MCP URL plus `knowledge_token`, the Run MCP
   endpoint, and `gateway_token`. `set_phase(Running)`. Pump harness events plus Run MCP events to
   `ServerLink` in batches.
8. Consume commands: `Steer(text)` → `HarnessSession::steer`; `Abort` → UC7 with `Cancelled`;
   `GateFeedback` → step 9.
9. After a `submit_*` the session idles in `AwaitingGate`.
   - `Failed { attempts_left > 0 }` → `set_phase(Running)`, `steer(render(feedback))`.
   - `Failed { attempts_left = 0 }` → UC7.
   - `Passed` → Learn: UC5, then UC7. Others: UC7.
   - If `POST /result` says no gate follows (Triage accepted; Diagnose verdict other than
     `Reproduced`), go to UC7 at once.
10. A harness session that exits before submitting → finish with the harness error
    ([harness.md](harness.md)).

Triage runs skip steps 2–5. They use `Executor::empty_env(ForkOptions { purpose: Triage, egress: ServerOnly })`
and the Run MCP with tool set `[submit_triage]` ([triage.md](triage.md)).

Local guard: at most `runner.max_sessions_per_repo` (default 1) Session forks per `(org, repo)` at
once. Verify and Scip forks are exempt. The server applies the same rule when leasing; this is
defense in depth.

### UC3 Tool call (`RunTools`)

1. The rmcp adapter authenticates the bearer and checks that the tool is in
   `ToolSet::for_kind(kind)` (otherwise MCP "unknown tool").
2. Deserialize arguments (schemars-derived schema; `submit_triage` uses the checked-in JSON
   Schema).
3. `RunTools::<tool>`: parse paths into `EnvPath`, apply limits, call `Env` / `BrowserRunner` /
   `ServerLink`.
4. Shape the output: per-field truncation, then the total cap (Run MCP section).
5. Emit `RunEventKind::ToolCall { name, args_preview }` before and
   `ToolResult { name, ok, preview }` after (previews ≤ 4 KiB). The harness adapter does not
   re-emit events for `mcp__run__*` tools ([harness.md](harness.md)).
6. Append one JSON line per call to `<fork>/logs/mcp.jsonl` (`LogSource::Mcp`).

### UC4 Submit and gate loop

`submit_*` validates locally, uploads what the server needs, and calls `POST /runs/{id}/result`
(with `attempt` and `Idempotency-Key: {run_id}:{submission_seq}`). It then returns at once with the
server's `ack.message` ([runs.md](runs.md)), for example:

> submitted (gate: Boot|Repro|Fix, attempt n). The gate runs in a fresh environment. End your turn now.

If the ack status is `AwaitingGate`, `RunTools` moves to `AwaitingGate`. Further `submit_*` calls
return a tool error "gate in progress" until `GateFeedback` arrives.
Other tools stay usable. `RunTools` reports each submit outcome (ack status, gate pending or not)
to the session task over a channel, and the session task drives steer and teardown. Details per
tool are in the Run MCP section.

### UC5 Learn post-gate SCIP

Runs after `GateFeedback { kind: Boot, outcome: Passed }`, with `set_phase(Finishing)`:

1. `prepare(recipe: Some(proposed))`. This normally hits the base the Boot Verify run just built
   on this runner ([runs.md](runs.md) prefers the same runner). If not, it prepares one here.
2. `fork(base, ForkOptions { purpose: Scip, egress: Registries })`.
3. Choose indexers: the recipe's `[[scip]]` entries, or else runner defaults for the languages
   present (config `[runner.scip.defaults]`, e.g. `typescript = "scip-typescript index --output ${scip.out}"`,
   `python = "scip-python index . --output ${scip.out}"` [unverified flags]). Binaries live under
   `/opt/mage/scip/bin`, which is prepended to `PATH` for `ProcessRole::Scip`.
4. For each language: `exec(ExecSpec::shell(env.render(run with ${scip.out} = <scratch>/scip/<lang>.scip)), cwd = repo root, timeout)`.
   - Non-zero exit → `Notice` event and skip that language. SCIP failure never fails the Learn run.
5. `export` each output to the outbox. Upload as `ArtifactKind::ScipIndex`, name
   `scip/<language>.scip`, cap 1 GiB. The server enqueues `index.ingest_scip` per artifact
   ([knowledge.md](knowledge.md)).
6. Destroy the Scip fork, then UC7.

### UC6 Verify run

See [Verify runs](#verify-runs-boot-repro-fix-testbaseline) below.

### UC7 Teardown

Order matters, and every step is idempotent:

1. `set_phase(Finishing)` (no-op if already there).
2. `RunMcpHandle::shutdown`.
3. Close the harness gracefully, then `terminate(grace = 10 s)` ([harness.md](harness.md)).
4. Collect logs: `export` service logs, `prepare.log`, and `mcp.jsonl` into one `logs.tar.zst`
   (cap 20 MiB, newest bytes kept). Upload as `ArtifactKind::Log`, name `env-logs.tar.zst`.
5. `env.destroy()`. Release the `RunMcp` port.
6. `ServerLink::finish(run, attempt, report)` with the finish outcome from [runs.md](runs.md):
   `Done` normally, or `Failed { stage, reason, retryable }`, where a prepare failure maps to
   `FailStage::Prepare`, a fork failure to `Fork`, a Run MCP start failure to `RunMcp`, a harness
   exit to `Harness`, a Verify infra failure to `Verify`, and an upload failure to `Upload`. Then
   `end_run`.

On `409 lease_lost` from any call (or the local lease deadline passing): self-fence. Skip uploads,
run steps 2, 3, and 5, `end_run`, then send `finish(Abandoned)` best effort (a 409 is ignored).

### UC8 Recovery on boot (`app::runner::recover`)

Runs before the lease loop starts. Harness sessions do not survive a daemon restart, so every
fork is orphaned.

1. `snap = RunnerState::recovery_snapshot()`.
2. Report the runs in `snap.runs` as held runs when registering ([runs.md](runs.md)). The server
   applies `LeaseLost` to them, and the runner `end_run`s each one.
3. `Executor::reap(ReapScope::Boot)`, which destroys every recorded fork and its resources, kills
   recorded processes (only if `boot_id` matches and `start_time` matches), deletes Preparing
   bases, releases all non-base ports, and runs the orphan scan (adapter section).
4. Report capabilities with the next lease call.

### UC9 Disk and periodic reaper (`app::runner::disk`)

Every 5 minutes, and before every prepare and fork:

1. `Executor::reap(ReapScope::Periodic)`: destroy forks whose `run_id` is not in `active_runs`,
   kill recorded processes whose env is gone, release ports whose owner is gone, empty `trash/`.
2. Free space (`statvfs` on the data dir) < `disk.min_free_gib` + estimate (the last base size for
   this repo, else 5 GiB) → evict Ready bases with `live_forks = 0` in LRU order (`last_used_at`)
   until enough is free. Also evict bases idle for more than `disk.base_idle_days`, and over-count
   bases (`disk.bases_max`) or over-size bases (`disk.bases_max_gib`).
3. Still short → `capabilities.disk_pressure = true`, so the server stops assigning new runs here,
   and the pending prepare fails with `Unavailable("disk")`.
4. Per-fork usage: every 60 s, allocated blocks of each fork dir (`st_blocks × 512`, walked by a
   background task). Over `disk.fork_max_gib` → the env enters `QuotaExceeded`: running execs are
   killed (`KillReason::Quota`), new ops return `QuotaExceeded`, and the run finishes `Errored`.

## Host adapter (`adapters::driven::exec_host`)

### v1 isolation rules

There are two modes. `isolation = "enforced"` is required on the GCP runner VM and is the only mode
allowed when `runner.environment = "production"`. `isolation = "dev"` is for the Mac. The daemon
refuses to start if the configured mode's checks fail.

| Rule | Enforced (Linux runner VM) | Dev (macOS) |
|---|---|---|
| Separate machine | Runner VM never hosts the server, indexer, Cloud SQL, or any secret | Everything on one Mac; never for non-hoop repos |
| Trusted repos only | Server leases host-executor runs only for repos of orgs flagged `execution = host` (internal) | same |
| OS users | Daemon runs as `mage`. Every repo-controlled process (build, services, exec, test, Playwright, SCIP, harness) runs as `mage-agent` (no login shell, no sudo, not in `docker` group) | Current user |
| Privileges | `mage` has ambient `CAP_SETUID CAP_SETGID CAP_KILL CAP_SYS_ADMIN CAP_CHOWN CAP_FOWNER CAP_DAC_OVERRIDE` via systemd, `Delegate=yes` for cgroups, member of `docker` | none |
| Filesystem view | Per-process mount namespace: workspace bound at `/mage/ws`, home `/mage/home`, scratch `/mage/scratch`, private `/tmp` `/var/tmp` `/dev/shm`; tmpfs over the data dir and `/etc/mage` hides other envs, bases, runner state and config; `PR_SET_NO_NEW_PRIVS` | Real paths; nothing hidden |
| Environment | Cleared, then the allowlist below | Cleared, then the allowlist below; `ProcessRole::Harness` keeps the real `HOME` only if `runner.dev.harness_real_home = true` (harness logins, e.g. keychain) |
| Process control | One cgroup v2 per env (`memory.max`, `pids.max`, `cpu.weight`); `cgroup.kill` on destroy; process group per spawn | Process group per spawn only |
| Egress | VPC deny-all except allowlisted destinations [unverified: GCP FQDN firewall objects]; host nftables: `meta skuid mage-agent` may reach only `lo`; outbound via the per-env loopback proxy with hostname allowlist; no DNS for `mage-agent` | Not enforced; startup warning |
| Containers | Compose model validated (no privileged, host namespaces, devices, caps, host binds outside the fork); started by the daemon, never by `mage-agent`; `DOCKER-USER` chain drops egress from compose bridges [unverified exact rules] | Docker Desktop, same validation |
| Secrets on disk | Runner token and local Postgres admin credentials only, mode 0600 owner `mage`, hidden from env processes by the mount namespace. Per-run `knowledge_token` / `gateway_token` live only in the harness's per-env config under scratch | same files, not hidden |
| Data | Fake-keys env only (`NotFakeSecret`, `LiveSecret`); the runner-local Postgres holds only seed data | same |
| Concurrency | ≤ 1 Session fork per repo (server + local guard), `runner.max_envs` total | same |

Known v1 gaps, accepted for trusted repos and closed by v2:

- Same kernel.
- All envs share the `mage-agent` uid, so loopback ports of one env are reachable from another.
  Postgres per-fork credentials and the Run MCP bearer still separate those two surfaces.
- In dev mode, nothing is hidden.

### Runner config (executor keys)

These keys belong to the `[runner]` section of `mage_runner::Config`. Values shown are the
defaults; the dev profile differs where noted.

```toml
[runner]
environment = "production"         # production | dev
isolation = "enforced"             # enforced | dev (dev refused when environment = production)
data_dir = "/var/lib/mage"         # dev: ~/.mage/runner
max_envs = 4                       # concurrent forks of any purpose
max_sessions_per_repo = 1

[runner.agent]
user = "mage-agent"                # enforced only
path = ["/opt/mage/bin", "/usr/local/bin", "/usr/bin", "/bin"]
env_passthrough = []               # names only; reserved names refused at load

[runner.dev]
harness_real_home = false

[runner.toolchains]                # recipe [toolchain] name → version → bin dir
node = { "20" = "/opt/mage/toolchains/node-20/bin", "22" = "/opt/mage/toolchains/node-22/bin" }
python = { "3.12" = "/opt/mage/toolchains/python-3.12/bin" }

[runner.ports]
range = [20000, 29999]

[runner.exec]
default_timeout_s = 120
max_timeout_s = 900
kill_grace_s = 5

[runner.prepare]
max_total_s = 5400                 # sum of build step timeouts
max_bundle_unpacked_gib = 8

[runner.fork]
allow_copy_fallback = false        # dev: true

[runner.cgroup]                    # enforced only
memory_max = "8G"
pids_max = 4096
cpu_weight = 100

[runner.postgres]
url = "postgres://mage_runner@127.0.0.1:5432/postgres"   # password from MAGE_RUNNER_POSTGRES_PASSWORD
strategy = "auto"                  # auto | wal_log | file_copy

[runner.egress]
mode = "proxy"                     # proxy | off (dev)
registries = ["registry.npmjs.org", "pypi.org", "files.pythonhosted.org", "proxy.golang.org",
              "sum.golang.org", "index.crates.io", "static.crates.io", "repo.maven.apache.org", "rubygems.org"]
# the server origin is always added from [runner].server_url

[runner.disk]
min_free_gib = 20
bases_max = 20
bases_max_gib = 200
base_idle_days = 14
fork_max_gib = 20

[runner.playwright]
root = "/opt/mage/playwright"
node = "/opt/mage/toolchains/node-20/bin/node"

[runner.scip.defaults]
typescript = "scip-typescript index --output ${scip.out}"

[runner.mcp]
max_response_bytes = 65536
max_calls = 2000
max_explore_uploads = 20

[runner.verify]
script_timeout_s = 300             # Shell scripts; Playwright uses RecordOpts
```

### Directory layout

```
<data_dir>/                          owner mage, 0750 (e.g. /var/lib/mage, dev ~/.mage/runner)
  state.db                           RunnerState (0600)
  bundles/<sha256>.tar.zst           SourceBundle cache (evicted with their last base)
  bases/<base_id>/                   Ready base, never mutated after rename
    base.json                        BaseKey, kind, recipe hash, bytes, fork_mode, created_at
    recipe.toml                      canonical recipe
    tree/                            workspace content (built)
    home/                            HOME during build (package caches)
    state/volumes/<volume>/          compose named-volume data (bind-mounted)
    baseline.git/                    bare-style git dir: refs/mage/source, refs/mage/prepared
    logs/prepare.log, logs/build/<step>.log
  bases/<base_id>.staging-<nonce>/   prepare in progress
  forks/<env_id>/
    env.json                         EnvId, BaseKey, run, ports, db names, created_at
    tree/ home/ state/volumes/       cloned from the base
    tmp/ shm/                        private /tmp and /dev/shm sources
    scratch/                         launch/, harness/, pw/, repro/, scip/ (agent-visible, never in diffs)
    baseline.git/                    own index + refs; objects/info/alternates → base objects
    logs/svc/<name>.log, logs/exec/<seq>.log, logs/mcp.jsonl
    run/                             runner-private (0700 mage): compose override, proxy state
  runs/<run_id>/outbox/              exported artifacts awaiting upload (0700 mage)
  trash/<nonce>/                     renamed-away dirs deleted in the background
/mage/{ws,home,scratch}              empty mount points (root, 0755), enforced mode only
/opt/mage/                           toolchains, playwright, scip binaries (root, read-only)
```

Ownership in enforced mode: `tree/`, `home/`, `tmp/`, and `shm/` are owned by `mage-agent`.
`scratch/` is `mage:mage-agent 1770` (sticky, `AgentCreateOnly`). The agent can create entries
there but cannot rename or replace runner-owned ones such as `scratch/harness/` (made with
`make_dir(AgentReadOnly)`), so harness configs holding per-run tokens cannot be swapped
mid-session. Everything else is owned by `mage`. Files the daemon creates for the agent with
`write` are `fchown`ed to `mage-agent` right after creation. `put_file`/`make_dir` apply the
requested `FileAccess`.

### Source bundle download and extract

1. Cache hit `bundles/<sha256>.tar.zst` → skip the download.
2. Otherwise stream a GET to the signed URL (`reqwest`, no proxy, 3 retries with exponential
   backoff, 10 min total) into `<sha256>.part`. Hash while writing. Size must equal `bytes` and the
   hash must equal `sha256`, else delete it and return `BundleRejected`. Then rename. An HTTP 400
   or 403 whose body says the signature expired → `SourceUrlExpired`. The session/verify use case
   then calls `ServerLink` to refresh download URLs ([runs.md](runs.md)) and retries `prepare`
   once. The signed URL is never logged.
3. Extract with `zstd` + `tar` crates in a blocking task into `staging/tree`, entry by entry:
   - reject absolute paths and any `..` component;
   - reject device, FIFO, and socket entries;
   - hard links only to earlier regular entries inside the tree;
   - symlinks allowed only if the target, resolved lexically from the link's directory, stays
     inside the tree;
   - mode masked to `0o755`/`0o644` with exec bits kept and setuid/setgid/sticky cleared;
   - caps: total ≤ `prepare.max_bundle_unpacked_gib` (default 8), entries ≤ 500 000, single
     file ≤ 2 GiB;
   - any violation → `BundleRejected(<entry>)`.
4. `chown -R mage-agent` (enforced) using `lchown` semantics, never following symlinks.

### Baselines (git)

The workspace has no `.git` (the bundle is a tree). The baseline lives outside the workspace, so
agent processes cannot rewrite it. All git calls run as `mage` with argv (no shell):

```
GIT_DIR=<base>/baseline.git GIT_WORK_TREE=<base>/tree
GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null HOME=<empty dir>
git -c core.hooksPath=/dev/null -c core.fsmonitor=false -c core.autocrlf=false \
    -c core.symlinks=true -c user.name=mage -c user.email=mage@localhost <cmd>
```

- `source`: `git init --bare`-style dir with `core.bare=false`. Exclusions go in
  `info/exclude`: build output dirs from runner config (`node_modules/`, `target/`, `dist/`,
  `.venv/`, `__pycache__/`, ...). Then `git add -A`,
  `git commit --allow-empty -m source --date=@0` (fixed `GIT_AUTHOR_DATE`/`GIT_COMMITTER_DATE`),
  `git update-ref refs/mage/source HEAD`.
- `prepared`: after build + migrate + seed, `git add -A` and commit, then `refs/mage/prepared`.
  This absorbs tracked files the build rewrote (lockfiles, generated code), so fix diffs contain
  only agent changes.
- Fork: `baseline.git` is a fresh dir with `objects/info/alternates` → the base objects, plus a
  copy of `refs/` and `HEAD`. The index is rebuilt lazily.

`diff_against_base(&DiffOptions { against, exclude })`:

```
GIT_INDEX_FILE=<fork>/run/diff.index git read-tree refs/mage/<against>
GIT_INDEX_FILE=... git add -A -- . ':(exclude,glob)<each exclude>'
GIT_INDEX_FILE=... git diff --cached --no-renames --no-color --no-ext-diff --full-index -U3 \
    --src-prefix=a/ --dst-prefix=b/ refs/mage/<against> -- . ':(exclude,glob)<each exclude>'
```

- Untracked, non-ignored files are included. The repo's `.gitignore` and `info/exclude` are
  honored.
- `excluded`: `git diff --cached --name-only refs/mage/<against> -- <each exclude as a positive
  pathspec>` on the index staged without excludes (a second temp index), so only paths that would
  have changed are listed.
- Binary files: first `git diff --cached --numstat refs/mage/<against>`. Lines `-\t-\t<path>` are
  binary; they go into `binary` and are added to the exclude pathspec for the patch run, so
  `patch` never contains `Binary files ... differ` stanzas.
- Output is deterministic for the same tree: git sorts paths, and the options are fixed.
- Cap: `patch` ≤ 4 MiB (hard), else `Invalid("patch too large")`. Policy limits are in
  [fix.md](fix.md).

`apply_patch(patch, mode)`:

- `Exact`: `git diff --quiet refs/mage/prepared` (with a temp index as above) must report no
  changes, else `Rejected { BaseMismatch }`. Then
  `git apply --whitespace=nowarn <patch>`. git apply requires every context line to match (zero
  fuzz); on an unchanged baseline every hunk lands at its stated line, so `Exact` never offsets.
- `AllowOffset`: the same `git apply` without the precondition. Offsets are allowed, fuzz is zero.
- A failure parses git's `error: patch failed: <path>:<line>` lines into `paths` →
  `Rejected { DoesNotApply }`. A parse failure → `Rejected { Malformed }`.
- `Applied.patch_sha256` = SHA-256 of the exact bytes passed in.
- Applied files are `chown`ed to `mage-agent`.

`check_patch(patch, Baseline::Source)` is a dry run with no worktree:
`GIT_INDEX_FILE=<tmp> git read-tree refs/mage/source && git apply --cached --check`. Fix gate
check 7 (PR applicability) uses it.

### Spawning processes

Every repo-controlled process goes through one internal `Launcher` with two implementations.

Precomputed before `fork()`:

- argv/env `CString`s;
- mount source and target `CStr`s;
- an `O_WRONLY|O_CLOEXEC` fd to `<env cgroup>/<leaf>/cgroup.procs`;
- uid/gid.

The child runs only raw syscalls in `pre_exec` (no allocation).

`LinuxLauncher` (enforced), `pre_exec` sequence:

1. `write(cgroup_fd, "0")`: move self into the leaf cgroup. Leaves: `harness`, `svc-<name>`,
   `exec-<seq>`, `build-<step>`, `pw-<n>`, `scip-<lang>`.
2. `setsid()`: new session and process group (pgid = pid).
3. `unshare(CLONE_NEWNS)`; `mount(NULL, "/", NULL, MS_REC|MS_PRIVATE)`.
4. Bind mounts (`MS_BIND|MS_REC`, then remount `MS_NOSUID|MS_NODEV`):
   - `<fork>/tree` → `/mage/ws`
   - `<fork>/home` → `/mage/home`
   - `<fork>/scratch` → `/mage/scratch`
   - `<fork>/tmp` → `/tmp` and `/var/tmp`
   - `<fork>/shm` → `/dev/shm`
5. `mount("tmpfs", <data_dir>, "tmpfs", MS_NOSUID|MS_NODEV|MS_NOEXEC, "size=64k,mode=000")`, and
   the same over `/etc/mage`. Order matters: the binds in step 4 resolve their sources before
   this hides them.
6. `prctl(PR_SET_NO_NEW_PRIVS, 1)`; `setrlimit(RLIMIT_CORE, 0)`;
   `setrlimit(RLIMIT_NOFILE, 65536)`.
7. `setgroups(&[])`; `setresgid(g, g, g)`; `setresuid(u, u, u)`.
8. `chdir(/mage/<area>/<cwd>)`, then exec.

After spawn the parent calls `RunnerState::record_process(ProcessRecord { pgid, pid, start_time, boot_id, run, env, role, cgroup })`.

`DevLauncher` (macOS): `CommandExt::process_group(0)`, `env_clear()`, cwd = real path, same uid.
It records the process the same way.

Base environment of every spawned process, built per env and fixed at spawn:

| Variable | Value |
|---|---|
| `PATH` | toolchain bin dirs for `[toolchain]` (runner config) + `runner.agent.path` (default `/opt/mage/bin:/usr/local/bin:/usr/bin:/bin`; harness binaries live in `/opt/mage/bin`, root-owned, visible in the mount namespace because only the data dir and `/etc/mage` are hidden) (+ `/opt/mage/scip/bin` for `Scip`) |
| `HOME` | `/mage/home` (enforced) / `<fork>/home` (dev; real home for `Harness` when `runner.dev.harness_real_home`) |
| `USER`, `LOGNAME` | `mage-agent` / current user |
| `SHELL` | `/bin/sh` |
| `LANG`, `LC_ALL` | `C.UTF-8` |
| `TZ` | `UTC` |
| `TMPDIR` | `/tmp` (private) / `<fork>/tmp` |
| `CI` | `1` |
| `HTTP_PROXY`, `HTTPS_PROXY`, `http_proxy`, `https_proxy` | `http://127.0.0.1:<env proxy port>` (enforced) |
| `NO_PROXY`, `no_proxy` | `127.0.0.1,localhost,::1` |
| `MAGE_ENV_ID`, `MAGE_RUN_ID`, `MAGE_WS`, `MAGE_SCRATCH` | ids and roots |
| `MAGE_PORT_<NAME>` | leased port per named port (name upper-cased) |
| `MAGE_DB_URL` | env DB URL, if any |
| `MAGE_BASE_URL`, `MAGE_LOGIN_URL`, `MAGE_LOGIN_USER`, `MAGE_LOGIN_PASSWORD` | from `[browser]`, rendered |
| recipe `[env]`, then step/service/test `env` | rendered |
| `runner.agent.env_passthrough` | named variables copied from the daemon env (config-controlled; `MAGE_*` and anything matching the reserved list are refused at config load) |
| `ExecSpec.env` / `SpawnSpec.env` | last. Reserved (→ `Invalid`): `PATH`, `HOME`, `USER`, `LOGNAME`, `SHELL`, `TMPDIR`, `LANG`, `LC_ALL`, `TZ`, `LD_*`, `DYLD_*`, `*_PROXY`/`*_proxy`, `MAGE_*`. Harness variables (`PI_*`, `OMP_*`, `CLAUDE_*`, `ANTHROPIC_BASE_URL`, `MCP_*`, ...) are allowed here; they are reserved only for recipes |

Kill escalation, `terminate(grace)` and `destroy`:

1. `killpg(pgid, SIGTERM)`.
2. Wait for `grace`: exec 5 s, service `stop_timeout_s`, harness 10 s.
3. Linux: write `1` to `<leaf>/cgroup.kill`, which also kills processes that left the group with
   `setsid`. macOS: `killpg(pgid, SIGKILL)`.
4. `clear_process(pgid)`.

`Exit::TimedOut` is reported when the exec timeout fired; `Killed(..)` when destroy, quota, or
cancel fired.

Output capture: stdout and stderr are read by two tasks. Each chunk goes to:

- `ExecSink` (if any);
- the log file `<fork>/logs/exec/<seq>.log` (both streams, arrival order);
- the in-memory `Captured`: the first `head` bytes plus a ring of the last `tail` bytes.

The log rotates at 64 MiB to `.1` (two files kept); `LogCursor` counts logical bytes across
rotations.

### Services

Native service (`run`):

- Spawned like exec with `ProcessRole::Service(name)` in cgroup leaf `svc-<name>`. stdout and
  stderr go to `<fork>/logs/svc/<name>.log` (same rotation).
- Start order: topological over `depends_on`. A service starts only after its dependencies are
  `Healthy`.
- Health loop per service: probe every `interval_ms`, with each probe bounded by `timeout_ms`.
  - First success within `start_period_s` → `Healthy`.
  - Timeout → `Unhealthy { reason: "no healthy probe within Ns; last: <detail>" }`.
  - After healthy, `retries` consecutive failures → `Unhealthy`.
  - Process exit → `Exited { exit }`.
- `http` probes run from the daemon (`reqwest`, no proxy, no redirects) and only to loopback
  addresses whose port is in this env's `PortMap`. `tcp` probes: same rule. `command` probes:
  `exec` with a 10 s timeout.

Compose service (`compose`):

- Project name `mage-<env_id_short>`. During prepare the project dir is the staging tree; in a
  fork it is the fork tree.
- Validation at prepare: `docker compose -f <file> -p <proj> config --format json` [unverified flag
  on older Compose]. Reject the model if any service has `privileged`, `cap_add`, `devices`,
  `network_mode: host`, `pid: host`, `ipc: host`, `userns_mode: host`, `security_opt` containing
  `unconfined`, a bind mount whose resolved source is outside the tree, a docker socket mount,
  or `volumes_from`.
- Mage generates `<fork>/run/compose.override.yml` (never written to the tree):
  - `ports: !override ["127.0.0.1:<leased>:<container_port>", ...]` [unverified: `!override` needs
    Compose ≥ 2.24];
  - every named volume redefined as a local bind to `<fork>/state/volumes/<volume>` (driver
    `local`, `type: none`, `o: bind`), so seeded volume data is cloned with the fork;
  - `pull_policy: never` for forks.
- Prepare runs `docker compose pull` and `docker compose build` (network allowed for the docker
  daemon only), then `up -d --no-deps <svc>`. Forks run `up -d --no-deps <svc>`.
- Logs: `docker compose logs -f --no-color <svc>` piped into `<fork>/logs/svc/<name>.log`.
- Stop: `docker compose stop -t <stop_timeout_s> <svc>`. Destroy: `docker compose down -v
  --remove-orphans`.
- Resource kinds recorded: `compose_project`.

After all services are healthy, the adapter lists listening TCP sockets of the env cgroup
(Linux: `/proc/<pid>/net/tcp` inode mapping) and emits a warning for any listener on a port not in
the env's `PortMap`. A fixed port collides across forks.

### Database (runner-local Postgres)

- One Postgres per runner (Linux: system package; dev: Docker), version ≥ 15, listening on
  `127.0.0.1:5432` and the local socket.
- The daemon connects as `mage_runner` (superuser on this disposable instance, password in runner
  config, never visible to env processes).
- `pg_hba`: `host all all 127.0.0.1/32 scram-sha-256`. `CONNECT` on every Mage database is revoked
  from `PUBLIC`.

| Step | SQL (identifiers quoted; names ≤ 63 bytes) |
|---|---|
| Base create (prepare) | `CREATE ROLE mb_<base_id> LOGIN PASSWORD '<random>'`; `CREATE DATABASE mb_<base_id> OWNER mb_<base_id>`; `REVOKE CONNECT ON DATABASE mb_<base_id> FROM PUBLIC`; per extension, as admin inside the DB: `CREATE EXTENSION IF NOT EXISTS "<ext>"` |
| Base freeze | terminate other backends; `ALTER DATABASE mb_<base_id> WITH ALLOW_CONNECTIONS false IS_TEMPLATE true` |
| Fork create | `CREATE ROLE mf_<env> LOGIN PASSWORD '<random 24 bytes, base64url>'`; `CREATE DATABASE mf_<env> TEMPLATE mb_<base_id> STRATEGY <s> OWNER mf_<env>`; `REVOKE CONNECT ... FROM PUBLIC`; `GRANT CONNECT ... TO mf_<env>`; inside `mf_<env>` as admin: `REASSIGN OWNED BY mb_<base_id> TO mf_<env>` |
| Reset (`reset_db`) | stop services; `DROP DATABASE mf_<env> WITH (FORCE)`; fork create again (role kept); start services |
| Destroy | `DROP DATABASE IF EXISTS mf_<env> WITH (FORCE)`; `DROP ROLE IF EXISTS mf_<env>` |
| Base evict | `ALTER DATABASE mb_<base_id> IS_TEMPLATE false`; `DROP DATABASE ... WITH (FORCE)`; `DROP ROLE` |

Notes:

- `STRATEGY`: config `postgres.strategy = "auto"` picks `WAL_LOG` (the Postgres default) for
  templates < 1 GiB and `FILE_COPY` above that. On Postgres ≥ 18 with
  `file_copy_method = clone` and an XFS/APFS data dir, `FILE_COPY` becomes a reflink clone. Both
  strategies are verified in the Postgres docs; clone speed on our disks is [unverified].
- The template must have no connections while it is copied, which `ALLOW_CONNECTIONS false`
  ensures.
- `REASSIGN OWNED` fixes table ownership, so the app's role owns its objects. Extensions owned by
  the admin stay admin-owned [unverified for every extension].
- `db_url()` = `postgres://mf_<env>:<pw>@127.0.0.1:5432/mf_<env>`. Resources recorded: `pg_db`,
  `pg_role`.
- `db_query` connects as `mf_<env>` with `sqlx` (extended protocol, so only one statement per
  call). The transaction is `READ ONLY` unless `write: true`. `SET LOCAL statement_timeout` comes
  from `DbQuery.timeout`. Rows are streamed and stopped at `max_rows`.
- `provider = "service"`: there is no runner DB. The DB's data dir is a compose volume under
  `state/volumes`, cloned with the fork. `db_url()` = rendered `database.url`. `reset_db` returns
  `Unsupported`.
- Learn (SourceOnly) envs: `setup_db` creates `mf_<env>` empty (no template), creates extensions,
  then runs `migrate` and `seed`.

### Copy-on-write fork

`fork(base, opts)`:

1. Check that the base is `Ready` and not being evicted. `begin_fork(ForkRecord { status: Creating })`.
2. Clone `tree/`, `home/`, and `state/volumes/` into `forks/<env_id>/`:
   - macOS: `clonefile(src, dst, CLONE_NOFOLLOW)` on each directory. APFS clones a whole
     directory hierarchy in one call [unverified for our sizes; `libc::clonefile` binding
     availability unverified]. `ENOTSUP` → walk fallback.
   - Linux: parallel walk (16 blocking workers) without following symlinks.
     - Directories: `mkdir` with the same mode.
     - Symlinks: recreated (`readlink` + `symlink`).
     - Regular files: `reflink_copy::reflink(src, dst)` (FICLONE).
     - mtime preserved (incremental builds compare mtimes).
     - Ownership: `lchown` to `mage-agent` for `tree/`/`home/`. Volume dirs keep their original
       uid/gid.
     - Hard links become independent files.
   - If any `reflink` fails with `EOPNOTSUPP`/`EXDEV`/`EINVAL`: restart the clone with
     `reflink_copy::reflink_or_copy` (full copy), set `fork_mode = Copy`, and emit
     `Notice("fork used a full copy: filesystem lacks reflink")`. In enforced mode this is allowed
     only with `fork.allow_copy_fallback = true`, else `Unavailable`.
3. At daemon start, `reflink_copy::check_reflink_support(<data>/probe-a, <data>/probe-b)` sets
   `capabilities.fork_mode`. The runner VM's data disk must be XFS with `reflink=1`; ext4 has no
   reflink.
4. Create the fork `baseline.git` (alternates), `scratch/`, `tmp/`, `shm/`, `logs/`, `run/`.
5. Create the cgroup `<runner cgroup>/env-<env_id>` with limits from config. Resource kind:
   `cgroup`.
6. Lease ports: one per declared port, plus the proxy port. Bind-test each; quarantine and
   re-lease on failure. Resource kind: `port`.
7. Database fork (above).
8. Start the egress proxy listener for the env.
9. `set_fork_status(Ready)`; return `Arc<HostEnv>`.

`empty_env(opts)`: steps 4–5, a proxy listener with `ServerOnly`, an empty `tree/`, no DB, no
ports. `ForkRecord.base = None`.

In enforced mode, paths are stable: the build ran with the workspace at `/mage/ws`, and every fork
mounts at `/mage/ws`, so absolute paths baked in by builds (Python venv shebangs, CMake caches)
keep working. Dev mode has no such guarantee; see Open questions.

### Ports

- Pool `runner.ports.range` (default `[20000, 29999]`). Owners: `Env` (named ports + proxy),
  `Base` (prepare-time services), `RunMcp`.
- The bind test binds `127.0.0.1:<port>` with `SO_REUSEADDR` off, then closes it. The TOCTOU
  window is acceptable: a service that then fails with `EADDRINUSE` shows up as unhealthy, and
  `start_services` retries once with fresh ports for that service.
- `PortMap` holds `127.0.0.1:<port>` addresses. Services must bind `127.0.0.1` (or `0.0.0.0`; the
  VPC blocks ingress anyway).

### Egress proxy

- One small CONNECT proxy listener per env on a leased loopback port, run inside the daemon
  (tokio + hyper).
- Only `CONNECT host:443`. Plain HTTP and other ports → `403`.
- `host` must match the env's profile:
  - `Registries`: `runner.egress.registries` (e.g. `registry.npmjs.org`, `pypi.org`,
    `files.pythonhosted.org`, `proxy.golang.org`, `sum.golang.org`, `index.crates.io`,
    `static.crates.io`, `repo.maven.apache.org`, `rubygems.org`) ∪ the server origin (model gateway,
    Knowledge MCP).
  - `ServerOnly`: the server origin only.
- Recipes cannot extend the list.
- The proxy resolves DNS itself and refuses private, link-local, and metadata addresses
  (`169.254.169.254`, RFC 1918) after resolution, except the server origin if it is private.
- Every CONNECT (allowed or denied) is logged to `<fork>/logs/egress.log` and surfaced in
  `logs`.
- Enforced mode backs this with nftables:

  ```
  table inet mage {
    chain out {
      type filter hook output priority 0; policy accept;
      meta skuid "mage-agent" oif "lo" accept
      meta skuid "mage-agent" counter reject
    }
  }
  ```

  Anything that ignores `HTTPS_PROXY` fails closed. The rules are installed by provisioning and
  verified by the daemon at start (`nft -j list table inet mage`).
- `HTTPS_PROXY` support: Node package managers, pip, cargo, go, and git honor it. Claude Code and
  omp: [unverified]. The harness doc must confirm, else the harness cannot reach the gateway in
  enforced mode.

### Destroy

`destroy()`:

1. Mark the fork `Destroying`.
2. Kill everything: TERM every recorded process group → grace → `cgroup.kill` (Linux) / SIGKILL
   (macOS).
3. `docker compose down -v --remove-orphans`.
4. Drop the DB and role.
5. Stop the proxy listener.
6. Release ports.
7. `rename(forks/<env_id>, trash/<nonce>)`, then delete in the background (`std::fs::remove_dir_all`,
   which is symlink-safe).
8. `rmdir` the cgroup.
9. `remove_fork`.

Each step removes its `EnvResource` row on success, so a crash mid-destroy resumes where it
stopped. Later calls on the `Env` return `Destroyed`. Host resource kinds: `dir`, `cgroup`,
`pg_db`, `pg_role`, `compose_project`, `port`, `proxy`.

### Reaper

`reap(Boot)`:

1. Destroy every recorded fork.
2. Kill recorded processes whose `boot_id` and `start_time` match (otherwise the pid is gone or
   was reused, and they are just cleared).
3. Delete Preparing bases and their staging dirs.
4. Release `Env`/`RunMcp` ports.
5. Orphan scan, which cleans what state does not know about:
   - dirs in `forks/`, `bases/*.staging-*`, and `trash/`;
   - databases `LIKE 'mf\_%'` / `'mb\_%'` not in state;
   - roles `mf_%`;
   - `docker compose ls --format json` projects named `mage-*`;
   - cgroups `env-*` under the runner cgroup.
6. Linux: any process owned by `mage-agent` outside known cgroups is killed.

`reap(Periodic)` does the same for forks whose run is no longer active, plus trash cleanup and
the disk policy (UC9).

## Run MCP (`adapters::driving::mcp`)

### Server lifecycle and auth

- One rmcp server per run, started by `RunMcpHost::start`. Transport: streamable HTTP on
  `127.0.0.1:<leased port>`, path `/mcp`, mounted in an axum router [rmcp API names
  unverified: `StreamableHttpService`, `#[tool_router]`].
- Bearer: 32 bytes from the OS RNG, base64url. Checked by a tower layer with constant-time
  comparison (`subtle`). Missing or wrong → HTTP 401.
- `Host` must be `127.0.0.1:<port>`. Requests with an `Origin` header are rejected (no browser
  clients; DNS-rebinding guard as recommended by the MCP transport spec).
- The token lives only in the harness MCP config under the env's scratch dir. In enforced mode
  that dir is invisible to other envs.
- Shutdown cancels in-flight calls. Cancelling an `exec` kills its process group.

### Tool sets per `RunKind`

`ToolSet::for_kind(kind) -> &'static [RunTool]`. Tools outside the set are not listed and calls
fail with "unknown tool".

| Tool | Triage | Learn | Diagnose | Fix |
|---|---|---|---|---|
| `exec` | | ✓ | ✓ | ✓ |
| `read` | | ✓ | ✓ | ✓ |
| `write` | | ✓ | ✓ | ✓ |
| `list` | | ✓ | ✓ | ✓ |
| `logs` | | ✓ | ✓ | ✓ |
| `services` (Additions) | | ✓ (draft-recipe actions) | ✓ | ✓ |
| `playwright_run` | | | ✓ | ✓ |
| `http_request` | | | ✓ | ✓ |
| `db_query` | | | ✓ | ✓ |
| `submit_triage` | ✓ | | | |
| `submit_recipe` | | ✓ | | |
| `submit_diagnosis` | | | ✓ | |
| `submit_fix` | | | | ✓ |

Verify runs have no harness and no Run MCP.

### Confinement and limits

- Paths: every `path`/`cwd` argument is workspace-relative and parsed into `RelPath` (rejects
  absolute paths, `..`, `~`, NUL, `\`). The adapter then opens files through `cap-std`
  (`cap_std::fs::Dir` rooted at the area). It uses `openat2(RESOLVE_BENEATH)` on Linux where
  available and component-wise `O_NOFOLLOW` resolution elsewhere [unverified per-version
  internals], so symlinks cannot escape. Absolute symlink targets are refused.
- Escape attempts → `ExecError::Confinement` → tool error "path escapes the workspace".
- Run MCP `read`/`write`/`list` act on `Area::Workspace` only. Scratch is used by the runner
  itself (harness config, Playwright invocations, verify scripts), through the same `Env` methods
  plus `put_file`/`make_dir`.
- Network tools (`http_request`, health probes) reach only `127.0.0.1`/`localhost`/`[::1]` with a
  port in `env.ports()`. No redirects are followed. No proxy is used.
- `db_query` reaches only this env's DB, as the env role.
- Concurrency per run: `exec` ≤ 4, `playwright_run` ≤ 1, `db_query` ≤ 2, `services` ≤ 1 (each
  serialized by a semaphore).
- Safety valve: total tool calls per run ≤ `mcp.max_calls` (default 2000). After that, every tool
  returns "tool budget exhausted". Model budget is enforced server-side.

Output shaping (applied in `RunTools`, so v1 and v2 match):

| Item | Cap | Truncation |
|---|---|---|
| `exec` stdout / stderr | 8 KiB head + 24 KiB tail each | marker `\n…[{n} bytes omitted; full output: logs source=exec:{seq}]…\n` |
| `read` | 2000 lines or 256 KiB; lines > 2000 chars cut with `…[line truncated]` | `truncated: true`, `next_line` given |
| `list` | 1000 entries | `truncated: true` |
| `logs` | 2000 lines or 64 KiB | oldest lines dropped |
| `http_request` body | 32 KiB | `body_truncated: true` |
| `db_query` | `max_rows` ≤ 1000; cell text 4 KiB; 64 KiB total | `truncated: true` |
| whole tool result | 64 KiB serialized (`mcp.max_response_bytes`) | largest text field cut first |

All text returned to the agent has invalid UTF-8 replaced and control characters other than `\n`
and `\t` stripped. ANSI sequences are removed.

### Tool schemas

Input structs derive `serde::Deserialize` + `schemars::JsonSchema` with
`deny_unknown_fields`. The JSON Schema served to the harness is generated from them.
`submit_triage` is the exception: its schema file is owned by [triage.md](triage.md). Notation
below: `?` optional, `= x` default, `(a..b)` bounds.

#### `exec`

```
in:  { command?: string(1..16384),        // run with /bin/sh -c
       argv?: string[](1..256),           // exactly one of command / argv
       cwd?: path = ".",
       timeout_s?: int(1..900) = 120,
       env?: { NAME: string(..4096) }(..32),   // NAME ^[A-Z_][A-Z0-9_]*$, not reserved
       stdin?: string(..1 MiB) }
out: { exit_code: int | null, signal: int | null, timed_out: bool, duration_ms: int,
       stdout: string, stderr: string, stdout_bytes: int, stderr_bytes: int,
       truncated: bool, log: "exec:<seq>" }
```

Runs `Env::exec` with `ProcessRole::Exec`. A non-zero exit is a normal result, not a tool error.

#### `read`

```
in:  { path: path, start_line?: int(1..) = 1, max_lines?: int(1..2000) = 2000 }
out: { path, content: string, start_line: int, end_line: int, total_lines: int,
       truncated: bool, next_line: int | null, sha256: hex, binary: bool }
```

Binary files (NUL in the first 8 KiB) return `content: ""`, `binary: true`.

#### `write`

```
in:  { path: path, content: string(..1 MiB), mode?: "create" | "overwrite" | "append" = "overwrite" }
out: { path, bytes: int, created: bool }
```

Missing parent dirs are created. The new file is `0644`, owner `mage-agent`. Writes into
`.mage/env.toml` are allowed (Learn).

#### `list`

```
in:  { path?: path = ".", depth?: int(1..4) = 1, glob?: string, hidden?: bool = false,
       respect_gitignore?: bool = true }
out: { entries: [{ path, kind: "file" | "dir" | "symlink", bytes: int }], truncated: bool }
```

#### `logs`

```
in:  { source: "prepare" | "build:<step>" | "service:<name>" | "exec:<seq>" | "egress" | "mcp",
       tail?: int(0..2000) = 200, grep?: string(..512), since?: string /* cursor */ }
out: { source, lines: string[], cursor: string, matched: int, truncated: bool }
```

`grep` is a Rust `regex` (size limit 1 MiB, case-sensitive; prefix `(?i)` for insensitive). It
scans at most the last 16 MiB of the log, then returns the last `tail` matches.

#### `services` (Additions)

```
in:  { action: "status" | "start" | "stop" | "restart" | "health"
               | "db_reset"                         // Diagnose, Fix
               | "validate" | "build" | "db_setup", // Learn only: use the draft .mage/env.toml
       names?: string[],                            // default all
       rebuild?: bool = false }                     // restart only: Env::rebuild(Changed(diff paths)) first
out: { services: [{ name, state, ports: { name: port }, detail }], all_healthy: bool,
       issues?: [{ path, code, message, severity }],   // validate / any Learn action
       steps?: [{ name, status, secs, log: "build:<step>" }] }
```

- In Learn, every action first re-reads the workspace `.mage/env.toml`, parses it, validates it
  (errors → tool error listing issues), and calls `Env::set_recipe` (ports re-leased as needed).
- `build` → `Env::build`; `db_setup` → `Env::setup_db`.
- `restart` with `rebuild: true` in Fix computes changed paths with
  `diff_against_base(Prepared, [])` and calls `Env::rebuild(Changed(..))`.

#### `playwright_run`

```
in:  { script: path,                     // *.spec.ts | *.spec.js | *.spec.mjs | *.test.ts | *.test.js
       grep?: string(..256), timeout_s?: int(10..600) = 180,
       record?: bool = true }            // video + trace + HAR
out: { passed: bool, status: "passed" | "failed" | "timed_out" | "interrupted" | "no_tests" | "crashed",
       error_text: string | null,
       failures: [{ title, message, location }](..10),
       http_statuses: int[](..50),
       duration_ms: int,
       artifacts: { video?: artifact_id, trace?: artifact_id, screenshots: artifact_id[] },
       stdout_tail: string }
```

- Runs `BrowserRunner::run(env, Workspace(script), RecordOpts { video: record, trace: record, screenshots: OnFailure, har: true, label: "explore-<n>" })`.
- Artifacts are uploaded via `ServerLink`, kinds `Video`/`Trace`/`Screenshot`, names
  `explore-<n>/<file>`. At most `mcp.max_explore_uploads` (default 20) uploads per run; after that,
  artifacts stay local and the result says so.
- `RunEventKind::Artifact(id)` is emitted per upload.

#### `http_request`

```
in:  { method: "GET" | "HEAD" | "POST" | "PUT" | "PATCH" | "DELETE" | "OPTIONS",
       url?: string,                     // http://127.0.0.1:<env port>/... (or localhost)
       port?: port_name, path?: string,  // alternative to url: http://127.0.0.1:<port.name><path>
       headers?: { string: string }(..32), body?: string(..1 MiB), body_base64?: string,
       timeout_s?: int(1..60) = 30 }
out: { status: int, headers: { string: string }, body: string, body_base64?: string,
       body_truncated: bool, duration_ms: int }
```

Sent from the daemon with `reqwest` (no proxy, `redirect::Policy::none()`). Allowed targets are
listed under Confinement. Non-UTF-8 bodies come back in `body_base64`.

#### `db_query`

```
in:  { sql: string(1..65536), params?: (null | bool | int | number | string)[](..100),
       write?: bool = false, max_rows?: int(1..1000) = 200, timeout_s?: int(1..30) = 10 }
out: { columns: [{ name, type }], rows: any[][], row_count: int, truncated: bool, duration_ms: int }
```

Only for envs with a DB (`Unsupported` otherwise). One statement per call. Read-only unless
`write: true`; writes hit only this fork's disposable DB.

#### `submit_triage` (Triage)

Input schema, caps, and the 200/422/409 mapping are owned by [triage.md](triage.md)
(checked-in JSON Schema served verbatim as `inputSchema`, ≤ 256 KiB). `RunTools` validates against the schema,
forwards the raw `TriageSubmission` via `ServerLink` `POST /result`, and maps the result:

- 200 → tool result `message`;
- 422 → tool error with `feedback`;
- 409 → tool error "run cancelled", and the runner aborts the session.

#### `submit_recipe` (Learn)

```
in:  { toml: string(1..65536), notes: string(..16384) /* markdown */ }
out: { submitted: true, recipe_hash: hex, warnings: [{ path, code, message }], message: string }
```

1. `RecipeSpec::parse` + `validate`. Errors → tool error listing every issue. No gate attempt is
   consumed.
2. If `toml` differs from the workspace `.mage/env.toml`, add a warning (the agent should keep
   them in sync).
3. `POST /result` with `RecipeProposal { toml, notes }`. The server recomputes the hash and starts
   the Boot gate. Then UC4.

#### `submit_diagnosis` (Diagnose)

```
in:  { verdict: "reproduced" | "not_reproduced" | "inconclusive",
       summary: string(1..16384),                        // markdown
       reason?: string(..4096),                          // required for inconclusive
       script?: path,                                    // required unless inconclusive
       signature?: { kind: "assertion_text" | "http_status" | "error_string" | "exit_code",
                     value: string(1..1024),
                     matcher: "exact" | "contains" | "regex" } }   // required unless inconclusive
out: { submitted: true, gate: "repro" | null, script_artifact: artifact_id | null, message: string }
```

Validation (tool error on any failure; no gate attempt consumed):

1. `script` resolves inside the workspace, exists, and is ≤ 256 KiB. `ScriptKind` comes from the
   extension: `*.spec.{ts,js,mjs}` / `*.test.{ts,js}` → `Playwright`; `*.sh` → `Shell`.
2. Signature compatibility:
   - `assertion_text` only with `Playwright`; `exit_code` only with `Shell`.
   - `http_status` value is three digits in `100..=599` with matcher `exact`.
   - `regex` compiles with the Rust `regex` crate (size limit 1 MiB). The domain uses the same
     engine.
3. Precheck, for `reproduced` only:
   - Run the script once in the session env with no recording (Playwright: `RecordOpts` all off;
     Shell: `exec` with timeout 300 s).
   - Extract observations for the signature's kind (`domain::observe`).
   - Require that `FailureSignature::matches` holds for at least one of them. Otherwise return a
     tool error: "signature not observed when your script ran here; observed: [...]".
   - This catches wrong signatures before the gate spends an attempt.
4. Upload the script as `ArtifactKind::Script`, name = file name, `content_type` `text/plain`.
   `ScriptKind` is derived from the extension the name keeps, so no extra metadata is needed.
5. `POST /result` with `DiagnoseResult { verdict, script, signature, summary }`. Then UC4 if the
   server answers that a gate follows, else UC7.

#### `submit_fix` (Fix)

```
in:  { summary: string(1..8192) /* markdown */ }
out: { submitted: true, patch_artifact: artifact_id, files: [{ path, change }], excluded: string[], message: string }
```

1. `diff_against_base(&DiffOptions { against: Prepared, exclude: FixSpec.policy exclusions })`.
   The exclusions come from the Fix run assignment ([fix.md](fix.md): always `.mage/**`, policy
   lockfiles, `[fix].generated`). Excluded paths are dropped silently and listed in `excluded`.
2. Local fast checks, which mirror the server's authoritative `PatchPolicy`:
   - empty patch → tool error "no changes";
   - deny-class paths (`.github/workflows/**`, `.github/actions/**` unless allowed, `.git/**`,
     symlinks, gitlinks), binary files, and size limits → tool error naming the paths.
   - The agent's own description of its changes is never used. The patch is always computed.
3. Upload `patch` as `ArtifactKind::Patch`, name `fix.patch`.
4. `POST /result` with `FixResult { patch, summary }`. Then UC4.

## Playwright adapter (`adapters::driven::playwright`)

Toolchain, provisioned on the runner and pinned:

```
/opt/mage/playwright/                   (dev: ~/.mage/playwright, installed by `mage runner setup`)
  package.json                          { "dependencies": { "@playwright/test": "<exact version>" } }
  node_modules/                         npm ci, read-only
  browsers/                             PLAYWRIGHT_BROWSERS_PATH; `playwright install chromium` (+ --with-deps on Linux)
/opt/mage/toolchains/node-<v>/bin/node  runner config playwright.node
```

The repo's own `node_modules` is never used. Repro scripts import only `@playwright/test` (and
Node built-ins). This keeps the Mage-run version fixed and works for repos that do not use
Playwright.

Invocation, per `run()` call, all through `Env` (so it works unchanged for aho):

1. `inv = <scratch>/pw/<n>/`. `Env::write` the files:
   - `mage.config.mjs` (generated, below);
   - `tests/<file name>` (the script; `Workspace(path)` is read with `Env::read`, `Inline` is
     written as given);
   - `node_modules` → symlink to `/opt/mage/playwright/node_modules`. Created with an `exec` of
     `ln -s`, because `Env::write` makes regular files only.
2. `Env::exec(ExecSpec { command: Argv([node, "/opt/mage/playwright/node_modules/@playwright/test/cli.js", "test", "--config", "<inv>/mage.config.mjs"]), cwd: scratch "pw/<n>", timeout: opts.timeout + 30 s, role: Playwright, env: [PLAYWRIGHT_BROWSERS_PATH, NO_COLOR=1, FORCE_COLOR=0, PW_TEST_HTML_REPORT_OPEN=never] })`.
   - The `cli.js` path is [unverified]; the fallback is `node_modules/.bin/playwright`.
   - `npx` is never used in enforced mode: it can fetch packages. Dev may use
     `npx --no-install playwright test` when `playwright.root` is unset.
   - `MAGE_BASE_URL`, `MAGE_PORT_*`, and `MAGE_LOGIN_*` come from the base env.
   - No proxy variables matter: the browser talks to loopback, and anything else fails closed.
3. Read `report.json` (`Env::read`, ≤ 32 MiB) and parse it.
4. Export the attachments and the report into `opts.outbox` (`Env::export`, sha256 computed while
   copying).

Generated config. Every value is JSON-encoded by Rust (`serde_json::to_string`), so script names
or URLs cannot inject code:

```js
// generated by mage; do not edit
import { defineConfig } from '@playwright/test';
export default defineConfig({
  testDir: './tests',
  outputDir: './out',
  timeout: 120000,                 // RecordOpts.test_timeout
  globalTimeout: 180000,           // RecordOpts.timeout
  expect: { timeout: 10000 },
  retries: 0,
  workers: 1,
  fullyParallel: false,
  reporter: [['json', { outputFile: './report.json' }], ['line']],
  grep: undefined,                 // RegExp from BrowserScript.grep when set
  use: {
    baseURL: "http://127.0.0.1:43123",   // Env::render(recipe.browser.base_url)
    headless: true,
    video: 'on',                   // 'off' when !opts.video
    trace: 'on',                   // 'off' when !opts.trace
    screenshot: 'only-on-failure', // ScreenshotMode
    viewport: { width: 1280, height: 800 },
    ignoreHTTPSErrors: true,
    contextOptions: { recordHar: { path: './out/network.har', content: 'omit' } },  // when opts.har [unverified in test runner]
  },
  projects: [{ name: 'chromium', use: { browserName: 'chromium' } }],
});
```

Chromium's sandbox may not start under `PR_SET_NO_NEW_PRIVS` in the mount namespace
[unverified]. If the integration test shows that, the config sets
`launchOptions: { chromiumSandbox: false }`. This is acceptable for v1 because the browser loads
only the local app.

JSON report parsing. Serde structs mirror the `JSONReport` subset, verified against Playwright's
`testReporter.d.ts`:

- `suites[]` (recursive `suites`) → `specs[]` → `tests[]` → `results[]` with `status`,
  `error { message, stack, location }`, `errors[] { message, location }`, `attachments[] { name, path, contentType }`;
- `tests[].annotations[] { type, description }`;
- top-level `errors[]`;
- `stats { expected, unexpected, flaky, skipped }`.

| Condition | `BrowserStatus` |
|---|---|
| top-level `errors` non-empty (syntax error, config error) | `Crashed { exit }`, `error_text = errors[0].message` |
| no `report.json` and non-zero exit | `Crashed { exit }`, `error_text` = stderr tail |
| exec `TimedOut` or any result `timedOut` | `TimedOut` |
| any result `interrupted` | `Interrupted` |
| `stats.expected + unexpected + flaky == 0` | `NoTests` |
| `unexpected > 0` or `flaky > 0` | `Failed` |
| otherwise | `Passed` (`passed = true` only here) |

- `error_text`: the first failing result's `error.message`, ANSI-stripped (`\x1b\[[0-9;?]*[A-Za-z]`),
  plus ` at <file>:<line>` from `location`, ≤ 4 KiB.
- `failures`: one per failing result (title path joined with ` › `).
- `http_statuses`:
  - statuses ≥ 400 from HAR `log.entries[].response.status`, in order and deduplicated;
  - annotation values with `type == "mage.http_status"`. Scripts may add
    `test.info().annotations.push({ type: 'mage.http_status', description: String(res.status()) })`
    for API calls the HAR does not see.
- Attachments: `video` (`video/webm`) → `Video`; `trace` (`application/zip`) → `Trace`;
  `screenshot` (`image/png`) → `Screenshot`. HAR and report → `Log`. Names
  `<label>/<video|trace|screenshot-N>.<ext>`.

Upload is the caller's job (`RunTools` or the verify use case) via `ServerLink`:

1. Request a signed upload with `{ kind, name, bytes, sha256, content_type }`.
2. PUT the file.
3. Get the `ArtifactId` and emit `RunEventKind::Artifact`.

Outbox files are deleted after a successful upload.

## Verify runs (Boot, Repro, Fix, TestBaseline)

A Verify run has no harness and no Run MCP. The assignment carries a `VerifySpec`
([runs.md](runs.md); Fix fields per [fix.md](fix.md)), the base key, the recipe TOML, and the
SourceBundle URL, so the runner can prepare when the base is absent. Every repetition gets a fresh
fork. The runner reports facts; `gates::{boot, repro, fix}` on the server decide.

```mermaid
flowchart TD
  A[lease Verify] --> P[prepare base]
  P -->|PrepareFailed| BF[outcome BootFailed]
  P --> K{spec}
  K -->|Boot| B1[fork → start_services → health] --> B2[attempt: health_ok] --> R[upload logs, POST result, finish]
  K -->|Repro| L1[loop r=1..repetitions]
  L1 --> F1[fork → start_services] --> S1[cursor logs → run script → observe] --> D1[destroy] --> L1
  L1 --> R
  K -->|Fix| U{upstream_check?}
  U -->|yes| UP[fork → run script unpatched → observe → destroy]
  U --> L2[loop r=1..repetitions]
  UP --> L2
  L2 --> F2[fork → apply_patch mode → write regression → rebuild → start_services → run script → observe → destroy] --> L2
  L2 --> T[fork → apply → rebuild → start_services → test command → lint → destroy]
  T --> C[check_patch Source] --> R
  K -->|TestBaseline| TB[fork → start_services → test command → destroy] --> R
```

Execution per spec (`app::runner::verify`):

| Spec | Steps per fresh fork | Reported |
|---|---|---|
| `Boot` | `start_services(All)` → `health()` | `outcome = Completed` with one attempt `health_ok`, or `BootFailed { reason }` (prepare step or service, with log tail) |
| `Repro { script, signature, repetitions }` | `start_services(All)` (unhealthy → `BootFailed`); `logs` cursors per service; run the script (below); extract observations; destroy | one `VerifyAttempt` per repetition |
| `Fix { patch, script, signature, repetitions, regression, mode, upstream_check }` | optional upstream attempt: fork, run the script **unpatched**, observe. Per repetition: `apply_patch(patch, mode)` (`Rejected` → `PatchRejected`); with `mode = AllowOffset` (rebase) compute the rebased patch via `diff_against_base(Prepared, exclusions)`, upload it as `Patch`, and use its bytes `Exact` afterwards; write the regression file if placed in the repo; `rebuild(Changed(patch paths ∪ regression path))`; `start_services`; run the script; observe. Tests fork: same apply + rebuild + services, then the test command and the optional `[fix].lint`. Finally `check_patch(patch, Source)` | attempts + `fix_checks { applied_patch_sha256, boot, test_exit, lint_exit, source_applies, upstream_already_fixed }` + `rebased_patch` ([fix.md](fix.md)) |
| `TestBaseline` | `start_services(All)`; test command | `fix_checks.test_exit` + test log |

Running the script:

- The script and patch artifacts are downloaded by signed URL and their sha256 checked against
  the spec pins (on `SourceUrlExpired`: refresh via `ServerLink`, retry once). The agent's
  workspace copy is never used.
- It is written to the fork's scratch (`repro/<file>`). The workspace tree stays pristine.
- `Playwright` → `BrowserRunner::run(env, Inline { .. }, RecordOpts { video: true, trace: true, screenshots: OnFailure, har: true, label: "verify-r<r>" })`.
- `Shell` → `exec(Argv(["bash", "<scratch>/repro/<file>"]), cwd = ws ".", timeout = 300 s, role = Test)`.
- Test command → `exec(ExecSpec::shell(env.render(test.run)), cwd = test.cwd, env = test.env, timeout = test.timeout_s, role = Test)`.
  Its log is uploaded as `Log` `tests.log`.

Artifacts kept:

- video + trace of repetition 1 (`verify-r1/...`);
- the trace of any repetition whose observations differ from repetition 1;
- the HAR and report of every repetition (as `Log`);
- service logs per fork (`env-logs-r<r>.tar.zst`).

The Repro gate's video comes from here, as the design requires.

### Observation extraction (`mage_runner::domain::observe`, pure)

Input: the expected `FailureSignature`, the script kind, the `BrowserOutcome` or `ExecOutcome`,
and service log lines appended after the pre-run cursors. Output: `Vec<FailureSignature>` with
`kind` = expected kind and `matcher = Exact`, `value` = observed text (≤ 1 KiB each).

| Expected kind | Playwright script | Shell script |
|---|---|---|
| `AssertionText` | each failing result's `error.message` and `errors[].message`, ANSI-stripped, whitespace-collapsed | none (rejected at submit) |
| `HttpStatus` | `BrowserOutcome.http_statuses` | stdout lines matching `^MAGE_HTTP_STATUS=(\d{3})$` |
| `ErrorString` | failure messages, stdout/stderr lines of the Playwright process, service log lines since the cursors | stdout/stderr lines, service log lines since the cursors |
| `ExitCode` | none (rejected at submit) | exit code as decimal; `signal:<n>`; `timeout` |

Ordering:

1. Candidates for which the runner's own `FailureSignature::matches` is true come first (the same
   pure domain function the server uses).
2. Then the others, in source order.
3. Deduplicate and cap at 16 per attempt.

The server re-evaluates. Repro needs a match on every repetition. Fix needs no match and script
exit 0 on every repetition ([domain.md](domain.md), [fix.md](fix.md)).

### `VerifyResult` field typing (Additions)

domain.md leaves `outcome` and `attempts` untyped. This doc uses:

```rust
pub enum VerifyOutcome { Completed, BootFailed { reason: String }, PatchRejected { reason: String }, Errored { reason: String } }
pub struct VerifyAttempt {
    pub index: u8, pub kind: AttemptKind /* Script | UpstreamScript | Boot */,
    pub script_exit: Option<i32>, pub script_passed: bool, pub status: Option<String> /* BrowserStatus */,
    pub observed: Vec<FailureSignature>,   // ≤ 16, see above
    pub health_ok: bool, pub duration_ms: u64,
}
// VerifyResult { outcome: VerifyOutcome, observed: Option<FailureSignature> /* first candidate of the last attempt, display */,
//                attempts: Vec<VerifyAttempt>, artifacts: Vec<ArtifactId>, fix_checks, rebased_patch /* fix.md */ }
```

`Errored` (infra: fork failed, Postgres down, toolchain missing) consumes no gate attempt
([runs.md](runs.md)).

## v2 VM adapters outline (`exec_vm`, `exec_ahocloud`)

v2 runs every fork in a Firecracker microVM. Two adapters share the same VM code
(`shared/vmm`, extracted from aho-cloud's `crates/runtime-agent` when we build it; see
[deployment.md](deployment.md)):

- `exec_vm`: the runner manages VMs on its own host through `shared/vmm` (self-host and our cloud).
- `exec_ahocloud` (feature `ahocloud`): the runner drives sandboxes on an aho-cloud runtime-agent.
  `mage-server` asks the aho-cloud control plane for a sandbox and a single-use exec token; the
  runner receives only that token (runner holds no secrets).

Facts from aho-cloud (`/Users/chico/m/projects/study-harness/aho-cloud`):

- `runtime-agent` backends: Firecracker (Linux/KVM; needs `firecracker`, kernel image, `skopeo`,
  `umoci`, `mkfs.ext4`, `ip`, `nft`, cgroup v2) and Docker (`--backend docker`, dev only: no VM
  isolation, no egress enforcement).
- Disks come from OCI images (image store: `skopeo` + `umoci` → rootfs → ext4). A sandbox's
  persistent disk starts as a copy of the image's `/workspace`.
- Exec protocol (`cloud_api::exec`): one `ExecRequest` (argv, env, cwd, user, optional PTY size),
  then binary frames: stdin, resize, close-stdin in; stdout, stderr, exit, error out. Served by
  `cloud-guest` (PID 1 in the VM) over vsock; Docker backend uses `docker exec`.
- Egress: per-guest `GuestPolicy` (hostname allowlist) enforced by an HTTP CONNECT proxy on the
  gateway plus nft rules; guests get `HTTP(S)_PROXY`.
- Resource ceilings: per-replica cgroup (CPU quota, memory).
- Files: tree and file reads only (`…/files`, `…/file`); no write API.
- No display device, no snapshot/fork, sandboxes long-lived per slug.

Harness placement stays `Host`: harnesses run on the runner outside the VM with built-in tools off
([harness.md](harness.md)); all execution goes through the Run MCP into the VM. (A harness inside
the VM, aho-cloud's sandbox-chat pattern, is possible later with a gateway token.)

| Port op | Mapping |
|---|---|
| `prepare` | Build a base OCI image: SourceBundle + recipe `[[build]]` steps on a per-arch toolchain base image (runner-side BuildKit, or aho-cloud's `builder` in `ahocloud` mode). Boot a VM from it, run `migrate`/`seed` and health checks, then bake the result into the image (or snapshot for L2). `BaseRef` → image digest (+ snapshot id later) |
| `fork` | L1: new VM whose disk is created from the base image; per-fork lease (IP/MAC/tap), egress `GuestPolicy`, cgroup ceilings. L2 (later): restore a Firecracker snapshot, then clock resync, entropy reseed, new MAC/IP, new exec session |
| `exec` / `spawn` | Exec protocol, pipes mode; `user = mage-agent`, explicit cleared env + allowlist; timeout → close + kill |
| `read` / `list` | Files API (`exec_ahocloud`) or exec of a reader (`exec_vm`); confined to the workspace |
| `write` / `put_file` / `make_dir` | Exec with stdin (no write API) |
| `export` | Exec streaming stdout (bulk out: videos, traces, SCIP indexes) |
| `start_services` / `stop_services` / `health` | Exec per service; health probes through the guest port |
| `ports` | Fixed guest ports (each VM has its own network); host reaches them via the guest IP (`exec_vm`) or the runtime's published address (`exec_ahocloud`) |
| `logs` | Exec `tail`/grep on guest log files |
| `db_url` / `db_query` | Postgres inside the guest; `db_query` via exec of `psql` or a guest-port connection |
| `diff_against_base` / `apply_patch` / `check_patch` | git in the guest via exec; baseline git dir outside the workspace path |
| `destroy` | Stop the VM, delete its disk, release lease and cgroup |
| `BrowserRunner` | Unchanged: Playwright runs in the guest through `Env::exec`; video recorded by Playwright (no display device) |
| `RunnerState` | Forks record VM id, lease, disk path as `EnvResource` kinds `vm`, `vm_disk`, `vm_lease` |

What `shared/vmm` / aho-cloud must add for Mage:

1. **Ephemeral VMs**: generated ids, TTL, no persistent disk, high creation rate (aho-cloud
   sandboxes are long-lived per slug and share the project namespace).
2. **Fork from a base**: L1 by creating disks from a base image quickly (reflink/sparse copy of the
   ext4 image instead of rebuilding); L2 with Firecracker snapshot/restore.
3. **Exec as an unprivileged guest user with an explicit env** (check `ExecRequest.user`/`env`
   semantics against `cloud-guest`).
4. **Hardware validation** on KVM hosts (GCP Intel with nested virtualization); today only the
   Docker backend ran end to end.
5. Per-arch base images (arm64 dev via Docker backend, x86_64 production).

## Failure modes and recovery

| Failure | Detection | Effect / recovery |
|---|---|---|
| Bundle download fails or sha256 mismatch | HTTP error, hash compare | 3 retries; then `BundleRejected`/`Unavailable` → run `Errored` (infra) |
| Signed URL expired (bundle, script, patch) | HTTP 400/403 with an expired signature | `SourceUrlExpired` → `ServerLink` refreshes download URLs → retry once → else `Errored` |
| Unsafe tar entry or size cap | extractor | `BundleRejected(entry)` → `Errored`; logged with the entry name |
| Build step non-zero | exit status | `PrepareFailed { step }`. In Verify: recipe-caused (build, migrate, seed, or health failure) → `VerifyOutcome::BootFailed`; infra (download, disk, Postgres, docker, toolchain) → `VerifyOutcome::Errored { reason }`. Diagnose/Fix session → `finish(Failed { stage: Prepare, reason: "base_does_not_boot" })` |
| Health timeout | health loop | `ServiceUnhealthy`; same mapping as above |
| `EADDRINUSE` on a leased port | service exits / unhealthy | quarantine port, re-lease, retry the service once |
| Reflink unsupported | probe / `EOPNOTSUPP` | full copy + `Notice` (dev), or `Unavailable` in enforced without `allow_copy_fallback` |
| Disk full (`ENOSPC`) | io error, `statvfs` | LRU eviction + retry once; else `disk_pressure`, run `Errored("disk")` |
| Fork over `fork_max_gib` | periodic du | `QuotaExceeded`, execs killed, run `Errored` |
| Runaway processes (fork bomb, memory) | cgroup `pids.max`/`memory.max` (Linux) | the process gets `EAGAIN`/OOM-kill inside the env only; dev: none |
| Process escapes its group | `cgroup.kill` (Linux); dev: reaper | killed at destroy; dev may leak until manual cleanup (documented) |
| Template DB busy | `CREATE DATABASE` error | the template has `ALLOW_CONNECTIONS false`; terminate stray backends and retry once |
| Local Postgres or docker down | connect error | `Unavailable`; the runner reports itself unhealthy and stops leasing until it recovers |
| Playwright missing or browser crash | spawn error / `Crashed` | tool error (session) or `Errored` (Verify) |
| Script timeout | `TimedOut` | no observation (Repro: not reproduced on that repetition; Fix: script failed) |
| Run MCP port taken | bind error | lease another port |
| `409 lease_lost` | `ServerLink` | abort harness, shutdown MCP, destroy env, `end_run`; no finish |
| Daemon crash or VM reboot | next start | UC8: finish active runs `Errored`, reap everything, keep Ready bases |
| Crash mid-destroy | leftover `EnvResource` rows | next reap resumes per resource |
| Crash mid-prepare | base row `Preparing` | `reap(Boot)` deletes staging and the base DB |
| Harness exits during `AwaitingGate` | `EnvChild::wait` | the session task sends `finish(Abandoned)` and the server sets `session_lost`. The gate keeps running: pass → `Succeeded`; fail with attempts left → `Running`, then `LeaseLost` (requeue with `prior_feedback`, or `Errored` at max attempts); fail with none left → `Failed` ([runs.md](runs.md)) |

## Security notes

- **Trust boundary.** The runner VM holds no DB, blob, vault, GitHub, Linear, Slack, or tenant
  model credentials.
  - Runner token: Ed25519, short-lived, from `/register`.
  - Per-run `knowledge_token` and `gateway_token`: signed, run-bound, rejected once the run is
    terminal ([server.md](server.md)). They reach only the harness's per-env config.
  - Run MCP bearer: runner-local random, run lifetime.
  - Signed URLs are short-lived and never logged.
- **Agent processes** (`mage-agent`):
  - cannot read runner state or config, or other envs (mount namespace);
  - cannot reach the network except loopback and allowlisted hosts through the proxy;
  - cannot use docker;
  - cannot regain privileges (`NO_NEW_PRIVS`, no sudo);
  - are killed as a unit (cgroup).
  - These rules apply to the harness too.
- **Daemon handling hostile files.** Every daemon-side file operation on a workspace uses `cap-std`
  beneath-resolution. Ownership changes use `lchown`/`fchown`, never following symlinks. Deletion
  uses symlink-safe `remove_dir_all`. Tar extraction rejects escapes. Git runs with hooks, system
  and global config, and fsmonitor disabled. Its git dir lives outside the workspace, so repo
  `.gitattributes` filters have no configured drivers.
- **No string interpolation of agent input into commands.** Mage-generated commands are argv.
  Agent-supplied `command` strings run as the agent in its env and nowhere else. The Playwright
  config embeds values JSON-encoded.
- **SSRF.** `http_request` and health probes reach only this env's loopback ports. The proxy
  refuses metadata and private ranges.
- **SQL.** `db_query` runs as the per-fork role on a disposable DB. One statement, statement
  timeout, read-only by default.
- **Compose.** The model is validated before any `up`. Images are pulled only in prepare. Runtime
  containers get no egress [unverified rules].
- **Recipes.** Fake-secret and live-secret rules block real credentials from entering envs or
  diffs. Recipes cannot add egress hosts. Patches are additionally scanned server-side
  ([fix.md](fix.md)).
- **Tool output is data.** Control characters and ANSI are stripped, and sizes are capped. Prompt
  injection through app output cannot be prevented. Its reach is bounded by the tool set and the
  isolation above.
- **Artifacts.** sha256 is computed on export and checked by the server on upload. Verify runs use
  pinned script and patch bytes, never workspace copies.
- **Dev mode is not a security boundary.** It refuses to start unless `isolation = "dev"` is set
  explicitly, and never with `runner.environment = "production"`.

## Testing

Port contract suites (`fn contract<E: Executor>(make)` style, in `mage_runner::testing`) run
against the fake, `exec_host` in dev mode (all CI), `exec_host` enforced (Linux CI VM, opt-in), and
later `exec_vm` (Docker backend in CI; Firecracker on a KVM CI host, opt-in) and `exec_ahocloud`.

| Suite | Cases |
|---|---|
| `Executor`/`Env` | prepare single-flight (two concurrent calls → one build); SourceOnly vs Prepared; fork isolation (write in A invisible in B and in the base; base bytes unchanged); `diff_against_base` → `apply_patch(Exact)` on a fresh fork reproduces the tree; deterministic diff bytes; `exclude` drops paths and lists them; binary listed, not encoded; `Exact` on a dirty tree → `BaseMismatch`; `check_patch(Source)` on a patch touching a build-rewritten file; confinement (`../x`, `/etc/passwd`, symlink to `/`, symlink chain out) → `Confinement`; exec timeout kills a grandchild that double-forked (Linux: cgroup; dev: process group); `Captured` head/tail boundaries at exactly cap and cap+1; reserved env names rejected; ports unique across concurrent forks; DB fork isolation (row inserted in A absent in B); `reset_db` restores the template state; `destroy` idempotent and later calls → `Destroyed`; `rebuild(Changed)` skips steps whose `inputs` don't match |
| `RunnerState` | `claim_base` single-flight; `lease_ports` uniqueness under concurrent callers; quarantine respected; `remove_base` conflict with live forks; resources cascade on `remove_fork`; `recovery_snapshot` after a simulated crash (rows left in each table) |
| `BrowserRunner` | fake returns scripted outcomes; real adapter (opt-in, needs the Playwright toolchain) against a fixture app booted by a fixture recipe: passing script → `Passed` + video + trace; failing assertion → `Failed`, `error_text` contains the assertion; syntax error → `Crashed`; zero tests → `NoTests`; timeout → `TimedOut`; HAR statuses and `mage.http_status` annotations extracted |
| JSON report parsing | golden `report.json` files produced by the pinned Playwright version (passed, failed, timed out, flaky, top-level error, nested suites) |
| Recipe (`mage-core`) | one test per `IssueCode` (pass and fail); canonical form: reordered keys, default omitted vs explicit, set arrays reordered → same hash; build order change → different hash; golden hash of the example recipe; parse → canonical → parse is a fixed point (property test) |
| Observation (`domain::observe`) | per kind × script kind table; matching candidates first; cap 16; ANSI stripping |
| `RunTools` (fakes) | tool set per `RunKind` (Triage cannot call `exec`); truncation markers and caps; `submit_diagnosis` rejects missing script, `exit_code` with Playwright, a bad regex, and a signature not observed in precheck (no `POST /result`); `submit_fix` empty diff → error, deny paths → error, excluded paths dropped; `submit_*` during `AwaitingGate` → "gate in progress"; 409 → run abort |
| Run MCP adapter | 401 without or with a wrong bearer; `Origin` rejected; unknown tool; a cancelled call kills its exec |
| Verify use case (fakes) | Repro: fresh fork per repetition, observations per attempt; Fix: `PatchRejected` stops early; rebase path uploads the rebased patch; TestBaseline; `Errored` on fork failure |
| Isolation (Linux CI VM, opt-in) | `mage-agent` process cannot list `<data_dir>`, read another env's workspace, read `/etc/mage`, connect to `1.1.1.1:443` directly, resolve DNS, or CONNECT to a non-allowlisted host; allowlisted CONNECT works; `sudo` fails (no-new-privs); compose file with `privileged: true` rejected |
| Recovery | kill -9 the daemon mid-run and mid-prepare; restart; assert no forks, DBs, compose projects, cgroups, or `mage-agent` processes remain and Ready bases survive |

## Additions to the contract

| Addition | Where | Why |
|---|---|---|
| `RecipeSpec` + `RecipeSpec::parse(&str)`, `validate()`, `canonical_toml()`, `hash()`; `RecipeIssue`, `IssueCode` | `mage_core::domain::recipe` | server and runner both parse/validate recipes; the hash must be identical on both |
| `toml` crate as a `mage-core` dependency | README dependency table | pure, no I/O, wasm-compatible; needed for `RecipeSpec::parse`. The canonical emitter is hand-written so the hash does not depend on `toml`'s formatting |
| Recipe schema above (incl. `[[build]].inputs`, `[[scip]]`, `[fakes]`, `[browser]`); `[fix]` keys owned by fix.md | recipe | build, boot, fork, rebuild, test, browser, SCIP need these fields |
| `Executor::empty_env`, `Executor::capabilities`, `Executor::reap`, `Executor::evict`, `Executor::base` | `ports::executor` | Triage harness isolation without a workspace; lease capabilities; recovery; disk policy |
| `Env::spawn` + `EnvChild` (incl. `interrupt` = SIGINT to the child pid) | `ports::executor` | v1 harnesses must run as `mage-agent` inside env isolation; adapters may not import `exec_host`; Claude Code abort falls back to SIGINT before `terminate` |
| `Env::put_file`, `Env::make_dir`, `FileAccess::{AgentReadOnly, AgentWritable, AgentCreateOnly}`; scratch root `1770` sticky | `ports::executor` | harness configs carry per-run tokens and must not be replaceable by `mage-agent` mid-session |
| `Env::export` | `ports::executor` | artifacts larger than tool read caps; aho needs copy-out |
| `BundleSource { url, sha256, bytes }` (named to avoid clashing with `mage_core::domain::SourceBundleRef`), `ExecError::SourceUrlExpired` | `ports::executor` | the run task refreshes expired signed URLs once ([runs.md](runs.md)) |
| `Env::check_patch`, `Env::rebuild`, `Env::set_recipe`, `Env::build`, `Env::setup_db`, `Env::reset_db`, `Env::render` | `ports::executor` | Fix gate PR applicability, post-patch rebuild, Learn iteration, `db_reset`, templated test/SCIP commands |
| `EnvId`, `BaseKey`, `BaseId`, `BaseKind::{Empty, SourceOnly, Prepared}` | `ports::executor` | runner-local identities; wire uses `PreparedBaseKey { repo_id, sha, recipe_hash }` |
| `RunMcpHost` / `RunMcpHandle` / `RunMcpEndpoint` | `ports::run_mcp` | the session use case starts a per-run driving adapter without importing `crate::adapters` |
| Run MCP tool `services` | Run MCP | Learn must exercise the engine's interpretation of the draft recipe before the Boot gate; Fix must restart after code edits; `db_reset` for repro iteration |
| `ScriptKind { Playwright, Shell }` by extension; `MAGE_*` env contract; `MAGE_HTTP_STATUS=` stdout convention for Shell | runner + fix brief | how Verify runs a script and extracts `HttpStatus` / `ExitCode` |
| `VerifyOutcome`, `VerifyAttempt`, `AttemptKind` typing of `VerifyResult.outcome/attempts` | `mage_core::domain` | domain.md leaves them untyped; gates need per-repetition observations |
| `DiagnoseResult.script` and `.signature` become `Option` (required unless `Inconclusive`) | `mage_core::domain` | an inconclusive diagnosis (e.g. app never booted) may have no script |
| `RunPhase` (local, persisted by `RunnerState`) maps onto wire `RunnerPhase { Preparing, Running, AwaitingGate, Finishing }` | `ports::runner_state` | recovery needs the local phase; heartbeats report the wire phase |

## Open questions

1. **Dev-mode path stability.** On macOS a fork lives at a different path than the base was built
   at. Tools that bake absolute paths (Python venv shebangs, some caches) point back into the base
   (read-only in practice, but mutable by the same user). Options: accept and document; add an
   optional `[[fork]]` relink step; or run dev runners in a Linux VM (Lima/OrbStack) to get
   `/mage/ws`.
2. **Per-env uids** (`mage-agent-<slot>`) instead of one shared `mage-agent`. This would add
   file-level and nftables-level separation of envs, including loopback ports, without relying
   only on the mount namespace. It departs from the shared fact "one `mage-agent` user".
3. **In-house namespace setup vs `bubblewrap`.** The `pre_exec` sequence is ~60 lines of unsafe
   code; `bwrap` is packaged and audited, but needs unprivileged userns or setuid.
4. **Per-env Postgres server** (data dir in the fork, cloned by reflink) vs the shared
   runner-local server with templates. Per-env gives version freedom and isolation; templates
   are faster to start.
5. **Harness egress.** Claude Code and omp honoring `HTTPS_PROXY` is [unverified]. The alternative
   is loopback relays in the daemon for the gateway and Knowledge MCP, which would also keep
   tokens out of the harness env.
6. **HAR in the test runner** (`contextOptions.recordHar`) is [unverified]. Without it,
   `HttpStatus` relies on the `mage.http_status` annotation.
7. **GCP FQDN egress.** Cloud NGFW FQDN objects vs a managed Secure Web Proxy for VPC-level
   registry allowlisting [unverified]. The host proxy + nftables work either way.
8. **`.git` in the workspace.** Some builds read git metadata (`git describe`). The bundle has none;
   recipes set version env vars instead. Revisit if this blocks real repos.
9. **Toolchain-aware scheduling.** Today an unmet `[toolchain]` fails prepare with `Unavailable`.
   The server could match `toolchain` against runner capabilities when leasing.
10. **Exploration artifact volume.** `playwright_run` uploads up to 20 explorations per run.
    Revisit with storage retention ([storage.md](storage.md)).
