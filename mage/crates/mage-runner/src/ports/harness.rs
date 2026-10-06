//! Harness port (harness.md): one interface to drive a coding agent (omp, Claude Code) as an
//! external process inside the run's `Env`, plus the launch spec, its validation table, the
//! normalized event model, and the `HarnessEvent` → `RunEventKind` mapping.

use std::collections::BTreeSet;
use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mage_core::domain::{HarnessKind, OrgId, RunEventKind, RunId, RunKind, Usage};
pub use mage_core::wire::runner::CredentialModeKind;
use mage_core::wire::runner::HarnessAvailability;

use crate::error::AppError;
use crate::ports::executor::{Env, EnvPath, ExecError, HarnessPlacement, IsolationMode, ListOpts, RelPath, is_reserved_env_name};

/// MCP server name of the Knowledge MCP.
pub const KNOWLEDGE_MCP_SERVER: &str = "mage_knowledge";
/// MCP server name of the per-run Run MCP.
pub const RUN_MCP_SERVER: &str = "mage_run";
/// `HarnessSpec.prompt` and steer text cap.
pub const PROMPT_MAX_BYTES: usize = 256 * 1024;
/// `HarnessSpec.system_append` cap.
pub const SYSTEM_APPEND_MAX_BYTES: usize = 64 * 1024;
/// Default `HarnessSpec.session_dir`.
pub const DEFAULT_SESSION_DIR: &str = "harness";
/// Scratch directory the harness process starts in (isolation rule H-ISO-1). Created by the Env.
pub const LAUNCH_DIR: &str = "launch";

// ---------------------------------------------------------------------------------------------
// Ports
// ---------------------------------------------------------------------------------------------

#[async_trait]
pub trait Harness: Send + Sync {
    /// Which harness this adapter drives.
    fn kind(&self) -> HarnessKind;

    /// Probe the installed binary and host config once at runner startup (version, supported
    /// credential modes, host preflight). Cached by the adapter; advertised in
    /// `POST /runner/v1/lease` capabilities.
    async fn probe(&self) -> Result<HarnessInfo, HarnessError>;

    /// Validate `spec`, write the session files into `env`'s scratch area, spawn the process
    /// through `env`, complete the protocol handshake, verify the tool inventory (fail closed),
    /// and submit `spec.prompt` as the first turn. Returns once the first prompt is accepted.
    async fn start(&self, env: Arc<dyn Env>, spec: HarnessSpec) -> Result<Box<dyn HarnessSession>, HarnessError>;
}

#[async_trait]
pub trait HarnessSession: Send + Sync {
    /// Identity of the live session (native session id, pid/pgid, resolved model, tools).
    fn info(&self) -> &SessionInfo;

    /// Take the event stream. Callable once; a second call returns `HarnessError::Conflict`.
    /// The stream yields `None` after the process has exited and all buffered events drained.
    fn events(&self) -> Result<Box<dyn HarnessEventStream>, HarnessError>;

    /// Deliver `text` as a user message. Mid-turn: steering (read after the running tool calls
    /// finish). Idle: starts a new turn. Never waits for the turn to finish.
    async fn steer(&self, text: String) -> Result<SteerId, HarnessError>;

    /// Cancel the current turn. The session stays alive and idle afterwards (steer works).
    /// No-op when idle. If the harness does not end the turn within `abort_deadline`, the adapter
    /// terminates the process and returns `HarnessError::Unavailable`.
    async fn abort(&self) -> Result<(), HarnessError>;

    /// Graceful shutdown: refuse new input, abort the turn if one is running, close stdin, keep
    /// draining stdout, wait up to `grace`, then `EnvChild::terminate`. Idempotent.
    /// `close(Duration::ZERO)` is the hard kill.
    async fn close(&self, grace: Duration) -> Result<(), HarnessError>;

    /// Resolves when the process has exited (after close, crash, or kill). Idempotent; every
    /// caller gets the same `HarnessExit`.
    async fn wait(&self) -> HarnessExit;
}

#[async_trait]
pub trait HarnessEventStream: Send {
    async fn next(&mut self) -> Option<HarnessEvent>;
}

// ---------------------------------------------------------------------------------------------
// Launch spec
// ---------------------------------------------------------------------------------------------

/// Runner-side launch spec, built by `mage_runner::app` from the `RunAssignment`. Distinct from
/// the server's persisted `RunSpec`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessSpec {
    pub run_id: RunId,
    pub org_id: OrgId,
    pub kind: RunKind,
    /// First user message. ≤ 256 KiB. Sent over the protocol, never argv.
    pub prompt: String,
    /// Appended to the harness's default system prompt. ≤ 64 KiB. The adapter prepends its own
    /// workspace preamble.
    pub system_append: String,
    /// Model id as the gateway / login knows it, e.g. "claude-sonnet-4-5".
    pub model: ModelId,
    pub mcp_servers: Vec<McpServerSpec>,
    pub builtin_tools: BuiltinToolPolicy,
    /// Agent working root, relative to `Env::root()` (root = repository root). Granted to
    /// built-in tools with `--add-dir`; never the process cwd. `Some` iff `builtin_tools != None`.
    pub cwd: Option<RelPath>,
    /// Names copied from the runner's own environment into the harness environment.
    pub env_allowlist: Vec<String>,
    pub credentials: CredentialsMode,
    pub budget: BudgetHints,
    /// Directory in the Env scratch area owned by this adapter (default "harness").
    pub session_dir: RelPath,
}

/// `[A-Za-z0-9._:@/-]{1,128}`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModelId(String);

impl ModelId {
    pub fn new(id: impl Into<String>) -> Result<Self, HarnessError> {
        let id = id.into();
        let ok = (1..=128).contains(&id.len())
            && id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'@' | b'/' | b'-'));
        if ok { Ok(Self(id)) } else { Err(HarnessError::Invalid(format!("model id {id:?} must match [A-Za-z0-9._:@/-]{{1,128}}"))) }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ModelId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// `[a-z][a-z0-9_]{0,31}`; Mage uses "mage_knowledge" and "mage_run".
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct McpServerName(String);

impl McpServerName {
    pub fn new(name: impl Into<String>) -> Result<Self, HarnessError> {
        let name = name.into();
        let bytes = name.as_bytes();
        let ok = (1..=32).contains(&bytes.len())
            && bytes[0].is_ascii_lowercase()
            && bytes.iter().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_');
        if ok { Ok(Self(name)) } else { Err(HarnessError::Invalid(format!("MCP server name {name:?} must match [a-z][a-z0-9_]{{0,31}}"))) }
    }

    pub fn knowledge() -> Self {
        Self(KNOWLEDGE_MCP_SERVER.to_owned())
    }

    pub fn run() -> Self {
        Self(RUN_MCP_SERVER.to_owned())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for McpServerName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerSpec {
    pub name: McpServerName,
    /// Absolute URL of a streamable-HTTP MCP endpoint as seen from inside the Env.
    pub url: String,
    pub bearer: Secret,
    /// Per-request timeout the harness must allow (Run MCP `exec`/`playwright_run` run minutes).
    pub tool_timeout: Duration,
    /// Exact tool names this server serves for this run; used by the inventory check. Must not
    /// start with the server name (omp shortens such names).
    pub expected_tools: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BuiltinToolPolicy {
    /// No built-in tools. All work goes through MCP.
    None,
    /// Read/search built-ins scoped to `cwd`.
    ReadOnly,
    /// Read/search/edit/write + shell in `cwd`.
    WorkspaceRw,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CredentialsMode {
    /// Dev only: the harness uses a dedicated, developer-logged-in config dir.
    LocalLogin,
    /// Production: Anthropic-Messages-compatible gateway; `token` is the per-run gateway_token.
    Gateway { base_url: String, token: Secret },
    /// Internal deployments only: direct Vertex with the runner VM's ADC. Claude Code only.
    Vertex { project_id: String, region: String },
}

impl CredentialsMode {
    pub fn kind(&self) -> CredentialModeKind {
        match self {
            Self::LocalLogin => CredentialModeKind::LocalLogin,
            Self::Gateway { .. } => CredentialModeKind::Gateway,
            Self::Vertex { .. } => CredentialModeKind::Vertex,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BudgetHints {
    pub max_wall: Duration,
    pub max_cost_micros: Option<u64>,
    pub max_turns: Option<u32>,
}

/// Opaque secret: redacting `Debug`, no `Display`, `expose()` for the one place that writes it.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret([redacted])")
    }
}

impl HarnessSpec {
    /// Every `Secret` the spec carries (redaction input).
    pub fn secrets(&self) -> Vec<&Secret> {
        let mut out: Vec<&Secret> = self.mcp_servers.iter().map(|s| &s.bearer).collect();
        if let CredentialsMode::Gateway { token, .. } = &self.credentials {
            out.push(token);
        }
        out
    }

    /// The spec-only rows of the validation table (harness.md "Spec validation"). Rows that need
    /// the Env or the runner config are in [`HarnessSpec::validate_in`].
    pub fn validate(&self) -> Result<(), HarnessError> {
        if (self.builtin_tools == BuiltinToolPolicy::None) != self.cwd.is_none() {
            return Err(HarnessError::Invalid("cwd must be set exactly when built-in tools are enabled".into()));
        }
        if self.session_dir.is_root() {
            return Err(HarnessError::Invalid("session_dir must be a directory inside scratch, not the scratch root".into()));
        }
        if self.session_dir.segments().next() == Some(LAUNCH_DIR) {
            return Err(HarnessError::Invalid(format!("session_dir must not be inside {LAUNCH_DIR}/")));
        }
        let mut names = BTreeSet::new();
        for server in &self.mcp_servers {
            if !names.insert(server.name.as_str()) {
                return Err(HarnessError::Invalid(format!("duplicate MCP server {}", server.name)));
            }
            validate_url(&server.url, &format!("MCP server {} url", server.name))?;
            if server.expected_tools.is_empty() {
                return Err(HarnessError::Invalid(format!("MCP server {} has no expected_tools", server.name)));
            }
            if let Some(tool) = server.expected_tools.iter().find(|t| t.starts_with(server.name.as_str())) {
                return Err(HarnessError::Invalid(format!("MCP tool {tool:?} starts with its server name {}", server.name)));
            }
        }
        for secret in self.secrets() {
            validate_secret(secret)?;
        }
        match &self.credentials {
            CredentialsMode::LocalLogin => {}
            CredentialsMode::Gateway { base_url, .. } => validate_url(base_url, "gateway base_url")?,
            CredentialsMode::Vertex { project_id, region } => {
                let ok = |s: &str| (1..=63).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
                if !ok(project_id) || !ok(region) {
                    return Err(HarnessError::Invalid("Vertex project_id/region must match [a-z0-9-]{1,63}".into()));
                }
            }
        }
        for name in &self.env_allowlist {
            validate_allowlisted_env(name)?;
        }
        if self.prompt.is_empty() || self.prompt.len() > PROMPT_MAX_BYTES {
            return Err(HarnessError::Invalid(format!("prompt must be 1..={PROMPT_MAX_BYTES} bytes")));
        }
        if self.system_append.len() > SYSTEM_APPEND_MAX_BYTES {
            return Err(HarnessError::Invalid(format!("system_append exceeds {SYSTEM_APPEND_MAX_BYTES} bytes")));
        }
        if self.budget.max_wall.is_zero() {
            return Err(HarnessError::Invalid("budget.max_wall must be > 0".into()));
        }
        Ok(())
    }

    /// The full validation table: [`HarnessSpec::validate`] plus the rows that need the Env
    /// (placement, `cwd` exists, `session_dir` fresh) and the runner policy (`supported` = the
    /// adapter's credential modes; `LocalLogin` only in a dev-mode Env; `Vertex` only with
    /// `allow_direct_vertex`).
    pub async fn validate_in(&self, env: &dyn Env, supported: &[CredentialModeKind], allow_direct_vertex: bool) -> Result<(), HarnessError> {
        self.validate()?;
        let mode = self.credentials.kind();
        if !supported.contains(&mode) {
            return Err(HarnessError::Unsupported(format!("credentials mode {mode:?} is not supported by this harness")));
        }
        if mode == CredentialModeKind::LocalLogin && env.isolation() != IsolationMode::Dev {
            return Err(HarnessError::Unsupported("LocalLogin credentials are only allowed in dev mode".into()));
        }
        if mode == CredentialModeKind::Vertex && !allow_direct_vertex {
            return Err(HarnessError::Unsupported("direct Vertex requires runner.harness.allow_direct_vertex".into()));
        }
        if env.harness_placement() == HarnessPlacement::Host && self.builtin_tools != BuiltinToolPolicy::None {
            return Err(HarnessError::Unsupported("built-in tools are not available when the harness runs outside the env".into()));
        }
        if let Some(cwd) = &self.cwd {
            let path = EnvPath::new(crate::ports::executor::Area::Workspace, cwd.clone());
            match env.list(&path, ListOpts { depth: 1, max_entries: 1, ..ListOpts::default() }).await {
                Ok(_) => {}
                Err(ExecError::NotFound(_) | ExecError::Invalid(_) | ExecError::Confinement { .. }) => {
                    return Err(HarnessError::Invalid(format!("cwd {cwd} is not a directory in the workspace")));
                }
                Err(e) => return Err(HarnessError::Env(e)),
            }
        }
        let session = EnvPath::new(crate::ports::executor::Area::Scratch, self.session_dir.clone());
        match env.list(&session, ListOpts { depth: 1, max_entries: 1, ..ListOpts::default() }).await {
            Err(ExecError::NotFound(_)) => Ok(()),
            Ok(_) | Err(ExecError::Invalid(_) | ExecError::Conflict(_)) => {
                Err(HarnessError::Invalid(format!("session_dir {} already exists", self.session_dir)))
            }
            Err(ExecError::Confinement { .. }) => Err(HarnessError::Invalid(format!("session_dir {} escapes scratch", self.session_dir))),
            Err(e) => Err(HarnessError::Env(e)),
        }
    }
}

/// Absolute `http(s)` URL; plain `http` only for loopback hosts.
fn validate_url(raw: &str, what: &str) -> Result<(), HarnessError> {
    let url = url::Url::parse(raw).map_err(|e| HarnessError::Invalid(format!("{what} {raw:?}: {e}")))?;
    let loopback = match url.host() {
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        None => false,
    };
    match url.scheme() {
        "https" if url.host().is_some() => Ok(()),
        "http" if loopback => Ok(()),
        "http" => Err(HarnessError::Invalid(format!("{what} {raw:?}: plain http is only allowed for loopback"))),
        _ => Err(HarnessError::Invalid(format!("{what} {raw:?}: must be an absolute http(s) URL"))),
    }
}

/// `[A-Za-z0-9_.-]{16,4096}`: no shell (`!`), no `${VAR}` expansion, no YAML/JSON metacharacters.
fn validate_secret(secret: &Secret) -> Result<(), HarnessError> {
    let s = secret.expose();
    let ok = (16..=4096).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'.' | b'-'));
    if ok { Ok(()) } else { Err(HarnessError::Invalid("a secret does not match [A-Za-z0-9_.-]{16,4096}".into())) }
}

fn validate_allowlisted_env(name: &str) -> Result<(), HarnessError> {
    const DENY_SUBSTRINGS: &[&str] = &["KEY", "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL"];
    const DENY_PREFIXES: &[&str] =
        &["ANTHROPIC_", "CLAUDE_", "OMP_", "PI_", "AWS_", "GOOGLE_", "GCP_", "CLOUDSDK_", "GITHUB_", "GH_", "OPENAI_", "BUN_", "NODE_OPTIONS"];
    let bytes = name.as_bytes();
    let shaped = !bytes.is_empty()
        && (bytes[0].is_ascii_uppercase() || bytes[0] == b'_')
        && bytes.iter().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || *b == b'_');
    if !shaped {
        return Err(HarnessError::Invalid(format!("env_allowlist name {name:?} must match ^[A-Z_][A-Z0-9_]*$")));
    }
    if is_reserved_env_name(name) {
        return Err(HarnessError::Invalid(format!("env_allowlist name {name} is reserved by the Env")));
    }
    if DENY_SUBSTRINGS.iter().any(|s| name.contains(s)) || DENY_PREFIXES.iter().any(|p| name.starts_with(p)) {
        return Err(HarnessError::Invalid(format!("env_allowlist name {name} is denied (credential or harness configuration)")));
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Session types and events
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessInfo {
    pub kind: HarnessKind,
    /// "18.3.1", "2.1.269".
    pub version: String,
    pub credential_modes: Vec<CredentialModeKind>,
}

impl HarnessInfo {
    /// Lease capability entry (`mage_core::wire::runner::HarnessAvailability`).
    pub fn availability(&self) -> HarnessAvailability {
        HarnessAvailability { kind: self.kind, version: self.version.clone(), credential_modes: self.credential_modes.clone() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub harness: HarnessKind,
    pub version: String,
    pub native_session_id: String,
    /// As resolved by the harness.
    pub model: String,
    /// Inventory that passed the check.
    pub tools: Vec<ToolRef>,
    /// From `EnvChild` (the Env records it in `RunnerState`).
    pub pid: u32,
    pub pgid: u32,
}

/// Adapter-generated; correlates notices.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SteerId(String);

impl SteerId {
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HarnessEvent {
    Ready(SessionInfo),
    TurnStarted { turn: u32 },
    /// Coalesced deltas, ≤ 4 KiB per event.
    AssistantText { turn: u32, text: String },
    ToolCall { turn: u32, call_id: String, tool: ToolRef, args_preview: String },
    ToolResult { turn: u32, call_id: String, tool: ToolRef, ok: bool, preview: String },
    Usage(UsageDelta),
    Notice { level: NoticeLevel, text: String },
    TurnEnded { turn: u32, outcome: TurnOutcome },
    /// Nothing is running or queued; the session waits for `steer` or `close`.
    Idle,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ToolRef {
    /// "bash", "Read", ...
    Builtin(String),
    /// Normalized from harness naming.
    Mcp { server: McpServerName, tool: String },
}

impl ToolRef {
    /// Name used in `RunEventKind`: built-ins raw, MCP as `knowledge.search_code` / `run.exec`
    /// (other servers keep their full name).
    pub fn event_name(&self) -> String {
        match self {
            Self::Builtin(name) => name.clone(),
            Self::Mcp { server, tool } => {
                let short = server.as_str().strip_prefix("mage_").unwrap_or(server.as_str());
                format!("{short}.{tool}")
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnOutcome {
    Completed,
    Aborted,
    Failed { message: String, retryable: bool, http_status: Option<u16> },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UsageDelta {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    /// Harness-side estimate; `None` when unknown.
    pub cost_micros: Option<u64>,
    pub model_calls: u32,
}

impl UsageDelta {
    /// Saturating sum; `cost_micros` stays `None` only if both sides are `None`.
    pub fn add(&self, other: &UsageDelta) -> UsageDelta {
        UsageDelta {
            input_tokens: self.input_tokens.saturating_add(other.input_tokens),
            output_tokens: self.output_tokens.saturating_add(other.output_tokens),
            cache_read_tokens: self.cache_read_tokens.saturating_add(other.cache_read_tokens),
            cache_write_tokens: self.cache_write_tokens.saturating_add(other.cache_write_tokens),
            cost_micros: match (self.cost_micros, other.cost_micros) {
                (None, None) => None,
                (a, b) => Some(a.unwrap_or(0).saturating_add(b.unwrap_or(0))),
            },
            model_calls: self.model_calls.saturating_add(other.model_calls),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NoticeLevel {
    Info,
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HarnessExit {
    pub reason: ExitReason,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    /// Last 64 KiB, secrets redacted.
    pub stderr_tail: String,
    /// Runner-local file, redacted.
    pub stderr_log: Option<PathBuf>,
    /// Runner-local redacted copy of the native session JSONL.
    pub transcript: Option<PathBuf>,
    /// Totals observed by the adapter.
    pub usage: UsageDelta,
    pub wall: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitReason {
    /// Exited after `close()`.
    Closed,
    /// Exited on its own.
    Crashed,
    Killed(KillCause),
    /// Adapter killed it: bad framing / oversized output.
    ProtocolError(String),
    /// Adapter killed it: inventory check failed.
    IsolationViolation(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KillCause {
    CloseGraceExpired,
    AbortUnanswered,
    Stalled,
    OutputLimit,
}

#[derive(Debug, thiserror::Error)]
pub enum HarnessError {
    /// Binary missing, unsupported version.
    #[error("not found: {0}")]
    NotFound(String),
    /// `events()` taken twice, steer while closing.
    #[error("conflict: {0}")]
    Conflict(String),
    /// `HarnessSpec` failed validation.
    #[error("invalid: {0}")]
    Invalid(String),
    /// Spawn/handshake/MCP readiness failed, process gone.
    #[error("unavailable: {0}")]
    Unavailable(String),
    /// Credentials mode, policy, or placement not supported.
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// Fail-closed inventory check.
    #[error("isolation violation: {0}")]
    IsolationViolation(String),
    /// Unparseable frames, chunk errors.
    #[error("protocol: {0}")]
    Protocol(String),
    /// Session already exited.
    #[error("harness session closed")]
    Closed,
    /// `Env::spawn` / scratch file operation failed.
    #[error("env: {0}")]
    Env(#[from] ExecError),
}

impl HarnessError {
    /// Infrastructure error (runs.md decides requeue by attempt count) vs a non-retryable run
    /// error (`Invalid`, `Unsupported`, `IsolationViolation`, `NotFound`, `Conflict`).
    pub fn is_infrastructure(&self) -> bool {
        matches!(self, Self::Unavailable(_) | Self::Protocol(_) | Self::Env(_) | Self::Closed)
    }
}

impl From<HarnessError> for AppError {
    fn from(e: HarnessError) -> Self {
        match e {
            HarnessError::NotFound(m) => AppError::NotFound(m),
            HarnessError::Conflict(m) => AppError::Conflict(m),
            HarnessError::Invalid(m) => AppError::Invalid(m),
            HarnessError::Unsupported(m) => AppError::Invalid(format!("unsupported: {m}")),
            HarnessError::IsolationViolation(m) => AppError::Invalid(format!("isolation violation: {m}")),
            HarnessError::Unavailable(m) => AppError::Unavailable(m),
            HarnessError::Protocol(m) => AppError::Unavailable(format!("harness protocol: {m}")),
            HarnessError::Closed => AppError::Unavailable("harness session closed".into()),
            HarnessError::Env(e) => AppError::Unavailable(format!("env: {e}")),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Mapping to the domain (harness.md "Mapping to the domain")
// ---------------------------------------------------------------------------------------------

/// Converts `HarnessEvent`s to `RunEventKind`s for `POST /runner/v1/runs/{id}/events`.
/// `Usage` is accumulated and forwarded as a cumulative value except in `Gateway` mode, where the
/// gateway's `RecordUsage` is authoritative (forwarding would double count).
#[derive(Debug, Clone)]
pub struct RunEventMapper {
    forward_usage: bool,
    total: Usage,
}

impl RunEventMapper {
    pub fn new(credentials: CredentialModeKind) -> Self {
        Self { forward_usage: credentials != CredentialModeKind::Gateway, total: Usage::default() }
    }

    /// Cumulative usage forwarded so far (zero in Gateway mode).
    pub fn usage(&self) -> Usage {
        self.total
    }

    pub fn map(&mut self, event: &HarnessEvent) -> Option<RunEventKind> {
        let mut kind = match event {
            HarnessEvent::Ready(info) => {
                let harness = match info.harness {
                    HarnessKind::Omp => "omp",
                    HarnessKind::ClaudeCode => "claude",
                };
                let mcp: BTreeSet<&str> = info
                    .tools
                    .iter()
                    .filter_map(|t| match t {
                        ToolRef::Mcp { server, .. } => Some(server.as_str()),
                        ToolRef::Builtin(_) => None,
                    })
                    .collect();
                let mcp: Vec<&str> = mcp.into_iter().collect();
                RunEventKind::Notice(format!(
                    "harness ready: {harness} {}, model {}, tools {}, mcp [{}]",
                    info.version,
                    info.model,
                    info.tools.len(),
                    mcp.join(", ")
                ))
            }
            HarnessEvent::TurnStarted { .. } | HarnessEvent::Idle => return None,
            HarnessEvent::AssistantText { text, .. } => RunEventKind::AssistantText(text.clone()),
            HarnessEvent::ToolCall { tool, args_preview, .. } => {
                RunEventKind::ToolCall { name: tool.event_name(), args_preview: args_preview.clone() }
            }
            HarnessEvent::ToolResult { tool, ok, preview, .. } => {
                RunEventKind::ToolResult { name: tool.event_name(), ok: *ok, preview: preview.clone() }
            }
            HarnessEvent::Usage(delta) => {
                if !self.forward_usage {
                    return None;
                }
                let input = delta.input_tokens.saturating_add(delta.cache_read_tokens).saturating_add(delta.cache_write_tokens);
                self.total = self.total.add(&Usage {
                    input_tokens: input,
                    output_tokens: delta.output_tokens,
                    cost_micros: delta.cost_micros.unwrap_or(0),
                    turns: delta.model_calls,
                    wall: Duration::ZERO,
                });
                RunEventKind::Usage(self.total)
            }
            HarnessEvent::Notice { level, text } => RunEventKind::Notice(match level {
                NoticeLevel::Info => text.clone(),
                NoticeLevel::Warning => format!("warning: {text}"),
                NoticeLevel::Error => format!("error: {text}"),
            }),
            HarnessEvent::TurnEnded { outcome, .. } => match outcome {
                TurnOutcome::Completed => return None,
                TurnOutcome::Aborted => RunEventKind::Notice("turn aborted".into()),
                TurnOutcome::Failed { message, .. } => RunEventKind::Error(message.clone()),
            },
        };
        kind.truncate_previews();
        Some(kind)
    }
}

#[cfg(test)]
mod tests;
