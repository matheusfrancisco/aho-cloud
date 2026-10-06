//! `RunnerState` port (executor.md "`RunnerState`"): local bookkeeping that makes crashes
//! recoverable: active runs, process groups, bases, forks and their resources, port leases.
//! Adapter: `adapters::driven::runner_state` (SQLite); fake: `crate::testing::runner_state`.

use async_trait::async_trait;
use mage_core::domain::{OrgId, RunId, RunKind};
use mage_core::wire::runner::RunnerPhase;
use time::OffsetDateTime;

use crate::error::AppError;
use crate::ports::executor::{BaseId, BaseKey, BaseKind, EnvId, ExecError, ForkMode, ForkPurpose, PortName, ProcessRole};

#[async_trait]
pub trait RunnerState: Send + Sync {
    // runs
    /// Conflict if present.
    async fn begin_run(&self, run: ActiveRun) -> Result<(), RunnerStateError>;
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
    /// Conflict if live forks.
    async fn remove_base(&self, key: &BaseKey) -> Result<(), RunnerStateError>;

    // forks and their adapter-owned resources
    async fn begin_fork(&self, f: ForkRecord) -> Result<(), RunnerStateError>;
    async fn set_fork_status(&self, env: EnvId, s: ForkStatus) -> Result<(), RunnerStateError>;
    async fn add_resource(&self, env: EnvId, r: EnvResource) -> Result<(), RunnerStateError>;
    async fn remove_resource(&self, env: EnvId, r: &EnvResource) -> Result<(), RunnerStateError>;
    async fn forks(&self) -> Result<Vec<ForkRecord>, RunnerStateError>;
    async fn remove_fork(&self, env: EnvId) -> Result<(), RunnerStateError>;

    // ports
    /// Atomic: picks the lowest free, non-quarantined ports in `pool` and inserts them, one per
    /// entry of `names`. `Unavailable` when the pool is exhausted.
    async fn lease_ports(&self, owner: &PortOwner, names: &[Option<PortName>], pool: PortRange) -> Result<Vec<PortLease>, RunnerStateError>;
    /// Releases a lease on `port` (if any) and keeps it out of `lease_ports` until `until`.
    async fn quarantine_port(&self, port: u16, until: OffsetDateTime) -> Result<(), RunnerStateError>;
    async fn release_ports(&self, owner: &PortOwner) -> Result<(), RunnerStateError>;
    async fn port_leases(&self) -> Result<Vec<PortLease>, RunnerStateError>;

    // recovery
    async fn recovery_snapshot(&self) -> Result<RecoverySnapshot, RunnerStateError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveRun {
    pub run_id: RunId,
    pub org_id: OrgId,
    pub kind: RunKind,
    pub attempt: u16,
    pub phase: RunPhase,
    pub env: Option<EnvId>,
    pub started_at: OffsetDateTime,
}

/// Local phase. Heartbeats send the wire `RunnerPhase` (runs.md): `Leased` sends nothing, the rest
/// map 1:1.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RunPhase {
    Leased,
    Preparing,
    Running,
    AwaitingGate,
    /// Learn SCIP post-step + teardown.
    Finishing,
}

impl RunPhase {
    pub const fn to_wire(self) -> Option<RunnerPhase> {
        match self {
            Self::Leased => None,
            Self::Preparing => Some(RunnerPhase::Preparing),
            Self::Running => Some(RunnerPhase::Running),
            Self::AwaitingGate => Some(RunnerPhase::AwaitingGate),
            Self::Finishing => Some(RunnerPhase::Finishing),
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Leased => "leased",
            Self::Preparing => "preparing",
            Self::Running => "running",
            Self::AwaitingGate => "awaiting_gate",
            Self::Finishing => "finishing",
        }
    }

    pub fn parse(s: &str) -> Result<Self, RunnerStateError> {
        Ok(match s {
            "leased" => Self::Leased,
            "preparing" => Self::Preparing,
            "running" => Self::Running,
            "awaiting_gate" => Self::AwaitingGate,
            "finishing" => Self::Finishing,
            other => return Err(RunnerStateError::Invalid(format!("unknown run phase {other:?}"))),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessRecord {
    pub pgid: i32,
    pub pid: i32,
    /// Linux /proc/<pid>/stat field 22; macOS proc start µs.
    pub start_time: u64,
    /// Linux /proc/sys/kernel/random/boot_id; macOS kern.boottime.
    pub boot_id: String,
    pub run_id: Option<RunId>,
    pub env: Option<EnvId>,
    pub role: ProcessRole,
    pub cgroup: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BaseClaim {
    /// The caller owns the prepare; the base row is `Preparing`.
    Owned,
    Ready(BaseRecord),
    InProgress,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BaseStatus {
    Preparing,
    Ready,
}

impl BaseStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Preparing => "preparing",
            Self::Ready => "ready",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseRecord {
    pub key: BaseKey,
    pub id: BaseId,
    pub kind: BaseKind,
    pub status: BaseStatus,
    /// Staging dir while `Preparing`, final dir once `Ready`.
    pub dir: String,
    pub bytes: u64,
    pub created_at: OffsetDateTime,
    pub last_used_at: OffsetDateTime,
    pub live_forks: u32,
    /// Set once `Ready`.
    pub fork_mode: Option<ForkMode>,
    pub template_db: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseMeta {
    pub kind: BaseKind,
    pub dir: String,
    pub bytes: u64,
    pub fork_mode: ForkMode,
    pub template_db: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkRecord {
    pub env: EnvId,
    /// `None` for `empty_env`.
    pub base: Option<BaseKey>,
    pub run_id: RunId,
    pub purpose: ForkPurpose,
    pub dir: String,
    pub status: ForkStatus,
    pub created_at: OffsetDateTime,
    pub resources: Vec<EnvResource>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ForkStatus {
    Creating,
    Ready,
    Destroying,
}

impl ForkStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Creating => "creating",
            Self::Ready => "ready",
            Self::Destroying => "destroying",
        }
    }

    pub fn parse(s: &str) -> Result<Self, RunnerStateError> {
        Ok(match s {
            "creating" => Self::Creating,
            "ready" => Self::Ready,
            "destroying" => Self::Destroying,
            other => return Err(RunnerStateError::Invalid(format!("unknown fork status {other:?}"))),
        })
    }
}

/// Adapter-defined. Host kinds: `dir`, `cgroup`, `pg_db`, `pg_role`, `compose_project`, `port`,
/// `proxy`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EnvResource {
    pub kind: String,
    pub value: String,
}

impl EnvResource {
    pub fn new(kind: &str, value: impl Into<String>) -> Self {
        Self { kind: kind.to_owned(), value: value.into() }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum PortOwner {
    Env(EnvId),
    Base(BaseId),
    RunMcp(RunId),
}

impl PortOwner {
    /// `(owner_kind, owner_id)` as stored.
    pub fn parts(&self) -> (&'static str, String) {
        match self {
            Self::Env(id) => ("env", id.to_string()),
            Self::Base(id) => ("base", id.to_string()),
            Self::RunMcp(id) => ("run_mcp", id.to_string()),
        }
    }

    pub fn from_parts(kind: &str, id: &str) -> Result<Self, RunnerStateError> {
        let bad = |e: String| RunnerStateError::Invalid(format!("port owner {kind}:{id}: {e}"));
        Ok(match kind {
            "env" => Self::Env(id.parse::<EnvId>().map_err(|e| bad(e.to_string()))?),
            "base" => Self::Base(BaseId::parse(id).map_err(|e| bad(e.to_string()))?),
            "run_mcp" => Self::RunMcp(id.parse::<RunId>().map_err(|e| bad(e.to_string()))?),
            other => return Err(bad(format!("unknown kind {other:?}"))),
        })
    }
}

/// Inclusive range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PortRange {
    pub start: u16,
    pub end: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PortLease {
    pub port: u16,
    pub owner: PortOwner,
    pub name: Option<PortName>,
    pub at: OffsetDateTime,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoverySnapshot {
    pub runs: Vec<ActiveRun>,
    pub processes: Vec<ProcessRecord>,
    pub forks: Vec<ForkRecord>,
    pub preparing_bases: Vec<BaseRecord>,
    pub ports: Vec<PortLease>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RunnerStateError {
    #[error("not found")]
    NotFound,
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("unavailable: {0}")]
    Unavailable(String),
}

impl From<RunnerStateError> for AppError {
    fn from(err: RunnerStateError) -> Self {
        match err {
            RunnerStateError::NotFound => AppError::NotFound("runner state record".into()),
            RunnerStateError::Conflict(m) => AppError::Conflict(m),
            RunnerStateError::Invalid(m) => AppError::Invalid(m),
            RunnerStateError::Unavailable(m) => AppError::Unavailable(m),
        }
    }
}

impl From<RunnerStateError> for ExecError {
    fn from(err: RunnerStateError) -> Self {
        match err {
            RunnerStateError::NotFound => ExecError::NotFound("runner state record".into()),
            RunnerStateError::Conflict(m) => ExecError::Conflict(m),
            RunnerStateError::Invalid(m) => ExecError::Invalid(m),
            RunnerStateError::Unavailable(m) => ExecError::Unavailable(format!("runner state: {m}")),
        }
    }
}
