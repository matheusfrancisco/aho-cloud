//! `Executor` / `Env` / `EnvChild` ports and their types (executor.md "Ports defined here").
//!
//! An `Executor` prepares bases for `(repo, sha, recipe)` and forks them into disposable `Env`s.
//! Everything that touches customer code (processes, files, services, the test database, patches)
//! goes through an `Env`. Adapters: `adapters::driven::exec_host` (v1), fakes in
//! `crate::testing::executor`.
//!
//! Additions to the doc's signatures (needed by harness and Run MCP adapters):
//! `Env::harness_placement`, `Env::isolation`, `Env::record_mcp` (writer for `LogSource::Mcp`),
//! and `LogSource::Egress` (the Run MCP `logs source=egress`).

use std::collections::BTreeMap;
use std::fmt;
use std::net::SocketAddr;
use std::path::Path;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mage_core::domain::fix::Glob;
use mage_core::domain::recipe::RecipeSpec;
use mage_core::domain::{CommitSha, OrgId, RecipeHash, RepoId, RunId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use tokio::io::{AsyncRead, AsyncWrite};
use uuid::Uuid;

pub use mage_core::domain::fix::ApplyMode;
pub use mage_core::wire::runner::{Arch, ExecutorKind};

use crate::error::AppError;

// ---------------------------------------------------------------------------------------------
// Identities
// ---------------------------------------------------------------------------------------------

/// Runner-local env id (minted from `IdGen::uuid_v7`). Displays as `env_<26 base32 chars>`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EnvId(Uuid);

const CROCKFORD: &[u8; 32] = b"0123456789abcdefghjkmnpqrstvwxyz";

impl EnvId {
    pub const PREFIX: &'static str = "env";

    pub const fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    pub const fn as_uuid(&self) -> Uuid {
        self.0
    }

    /// The 26 lowercase Crockford base32 characters without the prefix. Used in resource names
    /// (`mf_<slug>` databases and roles, `mage-<slug>` compose projects, `env-<slug>` cgroups).
    pub fn slug(&self) -> String {
        let value = self.0.as_u128();
        (0..26)
            .map(|i| {
                let shift = 5 * (25 - i);
                char::from(CROCKFORD[((value >> shift) & 0x1f) as usize])
            })
            .collect()
    }
}

impl From<Uuid> for EnvId {
    fn from(uuid: Uuid) -> Self {
        Self(uuid)
    }
}

impl fmt::Display for EnvId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}_{}", Self::PREFIX, self.slug())
    }
}

impl fmt::Debug for EnvId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl FromStr for EnvId {
    type Err = ExecError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bad = || ExecError::Invalid(format!("not an env id: {s:?}"));
        let rest = s.strip_prefix("env_").ok_or_else(bad)?;
        if rest.len() != 26 {
            return Err(bad());
        }
        let mut value: u128 = 0;
        for (i, c) in rest.bytes().enumerate() {
            let digit = CROCKFORD.iter().position(|&d| d == c).ok_or_else(bad)? as u128;
            if i == 0 && digit > 7 {
                return Err(bad());
            }
            value = (value << 5) | digit;
        }
        Ok(Self(Uuid::from_u128(value)))
    }
}

impl Serialize for EnvId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for EnvId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// `hex(sha256(canonical BaseKey bytes))[..20]`.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct BaseId(String);

impl BaseId {
    /// Parses a stored id: exactly 20 lowercase hex characters.
    pub fn parse(s: &str) -> Result<Self, ExecError> {
        if s.len() == 20 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
            Ok(Self(s.to_owned()))
        } else {
            Err(ExecError::Invalid(format!("not a base id: {s:?}")))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for BaseId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for BaseId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BaseId({})", self.0)
    }
}

/// Identity of a base: what was prepared, for which architecture and executor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BaseKey {
    pub org_id: OrgId,
    pub repo_id: RepoId,
    pub sha: CommitSha,
    /// `None` = SourceOnly base.
    pub recipe_hash: Option<RecipeHash>,
    pub arch: Arch,
    pub executor: ExecutorKind,
}

impl BaseKey {
    /// Canonical bytes hashed into the `BaseId`: one `name=value` line per field in declaration
    /// order, LF-terminated, under a version line.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        let recipe = self.recipe_hash.map_or_else(|| "-".to_owned(), |h| h.to_hex());
        let arch = match self.arch {
            Arch::X86_64 => "x86_64",
            Arch::Aarch64 => "aarch64",
        };
        let executor = match self.executor {
            ExecutorKind::Host => "host",
            ExecutorKind::Aho => "aho",
        };
        format!(
            "mage-base-key/1\norg={}\nrepo={}\nsha={}\nrecipe={recipe}\narch={arch}\nexecutor={executor}\n",
            self.org_id,
            self.repo_id,
            self.sha.to_hex()
        )
        .into_bytes()
    }

    pub fn id(&self) -> BaseId {
        let digest = Sha256::digest(self.canonical_bytes());
        let mut hex = hex::encode(digest);
        hex.truncate(20);
        BaseId(hex)
    }

    /// The wire form advertised in lease capabilities.
    pub fn to_wire(&self) -> mage_core::wire::runner::PreparedBaseKey {
        mage_core::wire::runner::PreparedBaseKey { repo_id: self.repo_id, sha: self.sha, recipe_hash: self.recipe_hash }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BaseKind {
    Empty,
    SourceOnly,
    Prepared,
}

impl BaseKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Empty => "empty",
            Self::SourceOnly => "source_only",
            Self::Prepared => "prepared",
        }
    }

    pub fn parse(s: &str) -> Result<Self, ExecError> {
        match s {
            "empty" => Ok(Self::Empty),
            "source_only" => Ok(Self::SourceOnly),
            "prepared" => Ok(Self::Prepared),
            other => Err(ExecError::Invalid(format!("unknown base kind {other:?}"))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BaseRef {
    pub key: BaseKey,
    pub id: BaseId,
    pub kind: BaseKind,
    pub ready_at: OffsetDateTime,
}

impl BaseRef {
    /// The base reported by an `empty_env`: nil org/repo ids, zero sha, no recipe, kind `Empty`.
    pub fn empty(arch: Arch, executor: ExecutorKind, at: OffsetDateTime) -> Self {
        let key = BaseKey {
            org_id: OrgId::from_uuid(Uuid::nil()),
            repo_id: RepoId::from_uuid(Uuid::nil()),
            sha: CommitSha([0; 20]),
            recipe_hash: None,
            arch,
            executor,
        };
        Self { id: key.id(), key, kind: BaseKind::Empty, ready_at: at }
    }
}

// ---------------------------------------------------------------------------------------------
// Prepare and fork requests
// ---------------------------------------------------------------------------------------------

/// A SourceBundle download. `Debug` never prints the signed URL.
#[derive(Clone, PartialEq, Eq)]
pub struct BundleSource {
    /// Signed GET URL; never logged.
    pub url: String,
    pub sha256: [u8; 32],
    pub bytes: u64,
}

impl fmt::Debug for BundleSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BundleSource")
            .field("url", &"<signed>")
            .field("sha256", &hex::encode(self.sha256))
            .field("bytes", &self.bytes)
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct PrepareRequest {
    pub org_id: OrgId,
    pub repo_id: RepoId,
    pub sha: CommitSha,
    /// `None` → SourceOnly (Learn without a validated recipe).
    pub recipe: Option<RecipeSpec>,
    pub source_bundle: BundleSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ForkOptions {
    pub run_id: RunId,
    pub purpose: ForkPurpose,
    pub egress: EgressProfile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForkPurpose {
    Session,
    Verify,
    Scip,
    Triage,
}

impl ForkPurpose {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Session => "session",
            Self::Verify => "verify",
            Self::Scip => "scip",
            Self::Triage => "triage",
        }
    }

    pub fn parse(s: &str) -> Result<Self, ExecError> {
        match s {
            "session" => Ok(Self::Session),
            "verify" => Ok(Self::Verify),
            "scip" => Ok(Self::Scip),
            "triage" => Ok(Self::Triage),
            other => Err(ExecError::Invalid(format!("unknown fork purpose {other:?}"))),
        }
    }
}

/// `Registries` = package registries ∪ server origin; `ServerOnly` = server origin only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EgressProfile {
    Registries,
    ServerOnly,
}

// ---------------------------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------------------------

/// Maximum length of a `RelPath` in bytes.
pub const REL_PATH_MAX: usize = 4096;

/// Validated relative path: '/'-separated, no leading '/', no '..' segment, no NUL, no '\',
/// no empty segments, ≤ 4096 bytes, leading "./" stripped. "." = area root.
///
/// Escapes (a leading `/`, a `..` segment, a leading `~`) are `ExecError::Confinement`; other
/// malformed input is `ExecError::Invalid`. `RelPath::new` reports escapes against
/// `Area::Workspace`; `EnvPath::scratch` re-labels them with `Area::Scratch`.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct RelPath(String);

impl RelPath {
    pub fn new(path: &str) -> Result<Self, ExecError> {
        Self::in_area(path, Area::Workspace)
    }

    /// The area root (".").
    pub fn root() -> Self {
        Self(".".to_owned())
    }

    fn in_area(path: &str, area: Area) -> Result<Self, ExecError> {
        let escape = || ExecError::Confinement { area, path: path.to_owned() };
        if path.len() > REL_PATH_MAX {
            return Err(ExecError::Invalid("path longer than 4096 bytes".into()));
        }
        if path.contains('\0') {
            return Err(ExecError::Invalid("path contains NUL".into()));
        }
        if path.contains('\\') {
            return Err(ExecError::Invalid("path contains '\\'".into()));
        }
        if path.starts_with('/') || path.starts_with('~') {
            return Err(escape());
        }
        let mut rest = path;
        while let Some(stripped) = rest.strip_prefix("./") {
            rest = stripped;
        }
        if rest.is_empty() || rest == "." {
            return Ok(Self::root());
        }
        let trimmed = rest.strip_suffix('/').unwrap_or(rest);
        for segment in trimmed.split('/') {
            match segment {
                "" => return Err(ExecError::Invalid(format!("path {path:?} has an empty segment"))),
                ".." => return Err(escape()),
                "." => return Err(ExecError::Invalid(format!("path {path:?} has a '.' segment"))),
                _ => {}
            }
        }
        Ok(Self(trimmed.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_root(&self) -> bool {
        self.0 == "."
    }

    /// Path segments; empty for the root.
    pub fn segments(&self) -> impl Iterator<Item = &str> {
        let s = if self.is_root() { "" } else { self.0.as_str() };
        s.split('/').filter(|seg| !seg.is_empty())
    }

    /// `self/child` (child must itself be a valid relative path).
    pub fn join(&self, child: &str) -> Result<Self, ExecError> {
        let child = Self::new(child)?;
        if self.is_root() {
            return Ok(child);
        }
        if child.is_root() {
            return Ok(self.clone());
        }
        Self::new(&format!("{}/{}", self.0, child.0))
    }

    /// The last segment ("" for the root).
    pub fn file_name(&self) -> &str {
        if self.is_root() { "" } else { self.0.rsplit('/').next().unwrap_or("") }
    }

    /// The parent path (`None` for the root; "." for a top-level entry).
    pub fn parent(&self) -> Option<Self> {
        if self.is_root() {
            return None;
        }
        Some(match self.0.rfind('/') {
            Some(i) => Self(self.0[..i].to_owned()),
            None => Self::root(),
        })
    }
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

impl TryFrom<String> for RelPath {
    type Error = ExecError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(&value)
    }
}

impl From<RelPath> for String {
    fn from(value: RelPath) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Area {
    Workspace,
    Scratch,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EnvPath {
    pub area: Area,
    pub rel: RelPath,
}

impl EnvPath {
    pub fn new(area: Area, rel: RelPath) -> Self {
        Self { area, rel }
    }

    pub fn ws(rel: &str) -> Result<Self, ExecError> {
        Ok(Self { area: Area::Workspace, rel: RelPath::in_area(rel, Area::Workspace)? })
    }

    pub fn scratch(rel: &str) -> Result<Self, ExecError> {
        Ok(Self { area: Area::Scratch, rel: RelPath::in_area(rel, Area::Scratch)? })
    }
}

impl fmt::Display for EnvPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let area = match self.area {
            Area::Workspace => "ws",
            Area::Scratch => "scratch",
        };
        write!(f, "{area}:{}", self.rel)
    }
}

/// `^[a-z][a-z0-9_]{0,31}$`.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct PortName(String);

impl PortName {
    pub fn new(name: &str) -> Result<Self, ExecError> {
        let bytes = name.as_bytes();
        let ok = !bytes.is_empty()
            && bytes.len() <= 32
            && bytes[0].is_ascii_lowercase()
            && bytes[1..].iter().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'_');
        if ok { Ok(Self(name.to_owned())) } else { Err(ExecError::Invalid(format!("invalid port name {name:?}"))) }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for PortName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for PortName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

impl TryFrom<String> for PortName {
    type Error = ExecError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(&value)
    }
}

impl From<PortName> for String {
    fn from(value: PortName) -> Self {
        value.0
    }
}

/// Named-port leases; addresses reachable from the runner process.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PortMap(pub BTreeMap<PortName, SocketAddr>);

impl PortMap {
    pub fn get(&self, name: &str) -> Option<SocketAddr> {
        self.0.iter().find(|(k, _)| k.as_str() == name).map(|(_, v)| *v)
    }

    /// Whether `port` is one of this env's leased ports.
    pub fn contains_port(&self, port: u16) -> bool {
        self.0.values().any(|addr| addr.port() == port)
    }
}

/// Git pathspec glob (`**`, `*`, `?`), matched with `mage_core::domain::fix::Glob` semantics.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PathGlob(Glob);

impl PathGlob {
    pub fn new(pattern: &str) -> Result<Self, ExecError> {
        Glob::new(pattern).map(Self).map_err(|e| ExecError::Invalid(format!("glob {pattern:?}: {e}")))
    }

    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    pub fn matches(&self, path: &str) -> bool {
        self.0.matches(path)
    }
}

impl From<Glob> for PathGlob {
    fn from(glob: Glob) -> Self {
        Self(glob)
    }
}

/// What a process is for; selects cgroup leaf, PATH additions, and egress.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ProcessRole {
    Harness,
    Service(String),
    Exec,
    Build,
    Test,
    Playwright,
    Scip,
    Precheck,
}

impl fmt::Display for ProcessRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Harness => f.write_str("harness"),
            Self::Service(name) => write!(f, "service:{name}"),
            Self::Exec => f.write_str("exec"),
            Self::Build => f.write_str("build"),
            Self::Test => f.write_str("test"),
            Self::Playwright => f.write_str("playwright"),
            Self::Scip => f.write_str("scip"),
            Self::Precheck => f.write_str("precheck"),
        }
    }
}

impl FromStr for ProcessRole {
    type Err = ExecError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "harness" => Self::Harness,
            "exec" => Self::Exec,
            "build" => Self::Build,
            "test" => Self::Test,
            "playwright" => Self::Playwright,
            "scip" => Self::Scip,
            "precheck" => Self::Precheck,
            other => match other.strip_prefix("service:") {
                Some(name) => Self::Service(name.to_owned()),
                None => return Err(ExecError::Invalid(format!("unknown process role {other:?}"))),
            },
        })
    }
}

// ---------------------------------------------------------------------------------------------
// Executor
// ---------------------------------------------------------------------------------------------

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutorCapabilities {
    pub kind: ExecutorKind,
    pub arch: Arch,
    /// Ready + Prepared only (wire: `PreparedBaseKey`).
    pub bases: Vec<BaseKey>,
    pub free_env_slots: u16,
    pub disk_free_bytes: u64,
    pub disk_pressure: bool,
    pub fork_mode: ForkMode,
    pub harness_placement: HarnessPlacement,
    pub isolation: IsolationMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForkMode {
    Clonefile,
    Reflink,
    Copy,
    Overlay,
}

impl ForkMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Clonefile => "clonefile",
            Self::Reflink => "reflink",
            Self::Copy => "copy",
            Self::Overlay => "overlay",
        }
    }

    pub fn parse(s: &str) -> Result<Self, ExecError> {
        match s {
            "clonefile" => Ok(Self::Clonefile),
            "reflink" => Ok(Self::Reflink),
            "copy" => Ok(Self::Copy),
            "overlay" => Ok(Self::Overlay),
            other => Err(ExecError::Invalid(format!("unknown fork mode {other:?}"))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessPlacement {
    /// v1 host: the harness runs inside the env.
    InEnv,
    /// v2 aho: the harness runs on the runner, outside the VM.
    Host,
}

/// `runner.isolation` (executor.md "v1 isolation rules").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IsolationMode {
    Enforced,
    Dev,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ReapScope {
    Boot,
    Periodic,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReapReport {
    pub forks_destroyed: u32,
    pub processes_killed: u32,
    pub databases_dropped: u32,
    pub compose_projects_removed: u32,
    pub dirs_removed: u32,
    pub bases_evicted: u32,
    pub bytes_freed: u64,
}

impl ReapReport {
    pub fn add(&mut self, other: &ReapReport) {
        self.forks_destroyed += other.forks_destroyed;
        self.processes_killed += other.processes_killed;
        self.databases_dropped += other.databases_dropped;
        self.compose_projects_removed += other.compose_projects_removed;
        self.dirs_removed += other.dirs_removed;
        self.bases_evicted += other.bases_evicted;
        self.bytes_freed += other.bytes_freed;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepStatus {
    Started,
    Ok { secs: u32 },
    Failed { exit: Option<Exit>, reason: String },
}

pub trait PrepareObserver: Send + Sync {
    fn step(&self, step: &str, status: StepStatus);
    /// Non-blocking; the receiver may drop lines.
    fn line(&self, step: &str, line: &str);
}

/// Observer that discards everything.
#[derive(Debug, Clone, Copy, Default)]
pub struct NullObserver;

impl PrepareObserver for NullObserver {
    fn step(&self, _step: &str, _status: StepStatus) {}
    fn line(&self, _step: &str, _line: &str) {}
}

// ---------------------------------------------------------------------------------------------
// Env
// ---------------------------------------------------------------------------------------------

#[async_trait]
pub trait Env: Send + Sync {
    fn id(&self) -> EnvId;
    fn base(&self) -> &BaseRef;
    /// Absolute workspace path as env processes see it.
    fn root(&self) -> &str;
    /// Absolute scratch path as env processes see it.
    fn scratch_root(&self) -> &str;
    /// Current named-port leases (Learn may add ports).
    fn ports(&self) -> PortMap;
    /// URL env processes use (local, fake credentials).
    fn db_url(&self) -> Option<String>;
    fn recipe(&self) -> Option<RecipeSpec>;
    /// Expands `${ws}`, `${scratch}`, `${port.x}`, `${service.x.url}`, `${db.*}`; `$${` is a literal
    /// `${`. Unknown or unavailable variables → `Invalid`. `${scip.out}` is not an env variable:
    /// callers substitute it before rendering.
    fn render(&self, template: &str) -> Result<String, ExecError>;
    /// Where harnesses run relative to this env (Addition; v1 host: `InEnv`).
    fn harness_placement(&self) -> HarnessPlacement;
    /// Isolation mode of this env (Addition; LocalLogin credentials are dev-only).
    fn isolation(&self) -> IsolationMode;

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
    /// SourceOnly-based envs only.
    async fn set_recipe(&self, spec: RecipeSpec) -> Result<(), ExecError>;
    async fn build(&self, obs: &dyn PrepareObserver) -> Result<(), ExecError>;
    async fn rebuild(&self, scope: RebuildScope, obs: &dyn PrepareObserver) -> Result<RebuildReport, ExecError>;
    /// SourceOnly envs: create DB, extensions, migrate, seed.
    async fn setup_db(&self) -> Result<(), ExecError>;
    /// Prepared envs: re-clone from the base template.
    async fn reset_db(&self) -> Result<(), ExecError>;
    async fn start_services(&self, which: ServiceSelector) -> Result<ServicesReport, ExecError>;
    async fn health(&self) -> Result<HealthReport, ExecError>;
    async fn stop_services(&self, which: ServiceSelector) -> Result<(), ExecError>;
    async fn logs(&self, q: LogQuery) -> Result<LogSlice, ExecError>;
    async fn db_query(&self, q: DbQuery) -> Result<DbRows, ExecError>;
    /// Appends one line (a newline is added) to the Run MCP call log `<fork>/logs/mcp.jsonl`,
    /// served as `LogSource::Mcp` (Addition: UC3 step 6 needs a writer).
    async fn record_mcp(&self, line: &[u8]) -> Result<(), ExecError>;

    // patches
    async fn diff_against_base(&self, opts: &DiffOptions) -> Result<PatchOutput, ExecError>;
    async fn apply_patch(&self, patch: &[u8], mode: ApplyMode) -> Result<ApplyOutcome, ExecError>;
    async fn check_patch(&self, patch: &[u8], baseline: Baseline) -> Result<ApplyOutcome, ExecError>;

    /// Idempotent. Afterwards every other method returns ExecError::Destroyed.
    async fn destroy(&self) -> Result<(), ExecError>;
}

// ---------------------------------------------------------------------------------------------
// Process types
// ---------------------------------------------------------------------------------------------

/// Default exec timeout (`runner.exec.default_timeout_s`).
pub const DEFAULT_EXEC_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecSpec {
    /// `Shell(String)` runs `/bin/sh -c`; `Argv(Vec<String>)` runs directly.
    pub command: Command,
    /// Default `EnvPath::ws(".")`.
    pub cwd: EnvPath,
    /// Added on top of the env base set; reserved names → `Invalid`.
    pub env: Vec<(String, String)>,
    /// `None` = /dev/null.
    pub stdin: Option<Vec<u8>>,
    /// Required, ≤ config `exec.max_timeout_s`.
    pub timeout: Duration,
    /// Per stream: head bytes + tail bytes kept in memory.
    pub capture: CaptureLimits,
    pub role: ProcessRole,
}

impl ExecSpec {
    fn with_command(command: Command, role: ProcessRole) -> Self {
        Self {
            command,
            cwd: EnvPath::new(Area::Workspace, RelPath::root()),
            env: Vec::new(),
            stdin: None,
            timeout: DEFAULT_EXEC_TIMEOUT,
            capture: CaptureLimits::default(),
            role,
        }
    }

    /// `/bin/sh -c <cmd>` in the workspace root, 120 s, default capture.
    pub fn shell(cmd: impl Into<String>, role: ProcessRole) -> Self {
        Self::with_command(Command::Shell(cmd.into()), role)
    }

    /// Direct argv in the workspace root, 120 s, default capture.
    pub fn argv(argv: Vec<String>, role: ProcessRole) -> Self {
        Self::with_command(Command::Argv(argv), role)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Shell(String),
    Argv(Vec<String>),
}

impl Command {
    /// The argv actually executed.
    pub fn to_argv(&self) -> Vec<String> {
        match self {
            Self::Shell(cmd) => vec!["/bin/sh".to_owned(), "-c".to_owned(), cmd.clone()],
            Self::Argv(argv) => argv.clone(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CaptureLimits {
    pub head: usize,
    pub tail: usize,
}

impl Default for CaptureLimits {
    /// 8 KiB + 24 KiB.
    fn default() -> Self {
        Self { head: 8 * 1024, tail: 24 * 1024 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecOutcome {
    pub exit: Exit,
    pub stdout: Captured,
    pub stderr: Captured,
    pub duration: Duration,
    pub log: LogSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Exit {
    Code(i32),
    Signal(i32),
    TimedOut,
    Killed(KillReason),
}

impl Exit {
    pub fn success(&self) -> bool {
        matches!(self, Self::Code(0))
    }
}

impl fmt::Display for Exit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Code(c) => write!(f, "exit {c}"),
            Self::Signal(s) => write!(f, "signal {s}"),
            Self::TimedOut => f.write_str("timed out"),
            Self::Killed(r) => write!(f, "killed ({r:?})"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum KillReason {
    Destroyed,
    Quota,
    Cancelled,
}

/// First `head` bytes plus the last `tail` bytes of a stream; `tail` is empty when
/// `total_bytes ≤ head`, and never overlaps `head`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Captured {
    pub head: Vec<u8>,
    pub tail: Vec<u8>,
    pub total_bytes: u64,
}

impl Captured {
    /// Capture of a complete stream.
    pub fn new(bytes: &[u8], limits: CaptureLimits) -> Self {
        let head_len = bytes.len().min(limits.head);
        let after = &bytes[head_len..];
        let tail_len = after.len().min(limits.tail);
        Self {
            head: bytes[..head_len].to_vec(),
            tail: after[after.len() - tail_len..].to_vec(),
            total_bytes: bytes.len() as u64,
        }
    }

    /// Bytes dropped between head and tail.
    pub fn omitted(&self) -> u64 {
        self.total_bytes.saturating_sub((self.head.len() + self.tail.len()) as u64)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StdStream {
    Stdout,
    Stderr,
}

/// Must not block.
pub trait ExecSink: Send + Sync {
    fn chunk(&self, stream: StdStream, data: &[u8]);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnSpec {
    pub argv: Vec<String>,
    pub cwd: EnvPath,
    pub env: Vec<(String, String)>,
    pub role: ProcessRole,
}

#[async_trait]
pub trait EnvChild: Send {
    fn pid(&self) -> u32;
    fn take_stdin(&mut self) -> Option<Pin<Box<dyn AsyncWrite + Send>>>;
    fn take_stdout(&mut self) -> Option<Pin<Box<dyn AsyncRead + Send>>>;
    fn take_stderr(&mut self) -> Option<Pin<Box<dyn AsyncRead + Send>>>;
    async fn wait(&mut self) -> Result<Exit, ExecError>;
    /// TERM → grace → KILL.
    async fn terminate(&mut self, grace: Duration) -> Result<Exit, ExecError>;
    /// SIGINT to the child pid only.
    fn interrupt(&self) -> Result<(), ExecError>;
}

/// Names `ExecSpec.env` / `SpawnSpec.env` may not set (executor.md "Spawning processes"):
/// `PATH`, `HOME`, `USER`, `LOGNAME`, `SHELL`, `TMPDIR`, `LANG`, `LC_ALL`, `TZ`, `LD_*`, `DYLD_*`,
/// `*_PROXY`/`*_proxy` (any case), `MAGE_*`. Harness variables are allowed.
pub fn is_reserved_env_name(name: &str) -> bool {
    const EXACT: &[&str] = &["PATH", "HOME", "USER", "LOGNAME", "SHELL", "TMPDIR", "LANG", "LC_ALL", "TZ"];
    EXACT.contains(&name)
        || name.starts_with("LD_")
        || name.starts_with("DYLD_")
        || name.starts_with("MAGE_")
        || name.to_ascii_uppercase().ends_with("_PROXY")
}

/// Rejects empty, malformed (`=` or NUL), and reserved names; NUL in values.
pub fn check_extra_env(vars: &[(String, String)]) -> Result<(), ExecError> {
    for (name, value) in vars {
        if name.is_empty() || name.contains('=') || name.contains('\0') {
            return Err(ExecError::Invalid(format!("invalid environment variable name {name:?}")));
        }
        if value.contains('\0') {
            return Err(ExecError::Invalid(format!("environment variable {name} contains NUL")));
        }
        if is_reserved_env_name(name) {
            return Err(ExecError::Invalid(format!("environment variable {name} is reserved")));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// File types
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadRange {
    Whole { max_bytes: u64 },
    /// `start` is 1-based.
    Lines { start: u32, count: u32, max_bytes: u64 },
    Bytes { offset: u64, len: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRead {
    pub bytes: Vec<u8>,
    pub total_bytes: u64,
    pub total_lines: Option<u32>,
    pub truncated: bool,
    pub binary: bool,
    /// Of the whole file.
    pub sha256: [u8; 32],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteMode {
    /// Conflict if exists.
    Create,
    Overwrite,
    Append,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteReport {
    pub bytes: u64,
    pub created: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListOpts {
    /// 1..=4.
    pub depth: u8,
    pub glob: Option<PathGlob>,
    pub hidden: bool,
    pub respect_gitignore: bool,
    pub max_entries: u32,
}

impl Default for ListOpts {
    fn default() -> Self {
        Self { depth: 1, glob: None, hidden: false, respect_gitignore: true, max_entries: 1000 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    pub entries: Vec<DirEntry>,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// Relative to the area root.
    pub path: RelPath,
    pub kind: EntryKind,
    pub bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExportReport {
    pub bytes: u64,
    pub sha256: [u8; 32],
}

/// Enforced-mode meaning (dev mode: same modes, no boundary since everything runs as one user;
/// dev directories keep the owner write bit so the runner can still manage them).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileAccess {
    /// Owner mage, group mage-agent; file 0440, dir 0550: agent reads, cannot modify or replace.
    AgentReadOnly,
    /// Owner mage-agent; file 0600, dir 0700.
    AgentWritable,
    /// Dir only: owner mage, group mage-agent, 1770 (sticky): agent creates its own files, cannot
    /// touch runner-owned ones.
    AgentCreateOnly,
}

// ---------------------------------------------------------------------------------------------
// Service, log, and DB types
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceSelector {
    All,
    Named(Vec<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServicesReport {
    pub services: Vec<ServiceStatus>,
    pub all_healthy: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceStatus {
    pub name: String,
    pub state: ServiceState,
    pub ports: Vec<(PortName, SocketAddr)>,
    pub since: OffsetDateTime,
    pub last_check: Option<HealthCheckResult>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceState {
    Stopped,
    Starting,
    Healthy,
    Unhealthy { reason: String },
    Exited { exit: Exit },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthReport {
    pub checks: Vec<HealthCheckResult>,
    pub all_ok: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthCheckResult {
    pub service: String,
    pub ok: bool,
    pub detail: String,
    pub at: OffsetDateTime,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebuildScope {
    All,
    Changed(Vec<RelPath>),
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RebuildReport {
    pub steps_run: Vec<String>,
    pub steps_skipped: Vec<String>,
    pub migrated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum LogSource {
    Prepare,
    Build(String),
    Service(String),
    Exec(u64),
    Mcp,
    /// Egress proxy CONNECT log (Addition: Run MCP `logs source=egress`).
    Egress,
}

impl fmt::Display for LogSource {
    /// The Run MCP `source` spelling: `prepare`, `build:<step>`, `service:<name>`, `exec:<seq>`,
    /// `mcp`, `egress`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Prepare => f.write_str("prepare"),
            Self::Build(step) => write!(f, "build:{step}"),
            Self::Service(name) => write!(f, "service:{name}"),
            Self::Exec(seq) => write!(f, "exec:{seq}"),
            Self::Mcp => f.write_str("mcp"),
            Self::Egress => f.write_str("egress"),
        }
    }
}

impl FromStr for LogSource {
    type Err = ExecError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bad = || ExecError::Invalid(format!("unknown log source {s:?}"));
        Ok(match s {
            "prepare" => Self::Prepare,
            "mcp" => Self::Mcp,
            "egress" => Self::Egress,
            _ => match s.split_once(':') {
                Some(("build", step)) if !step.is_empty() => Self::Build(step.to_owned()),
                Some(("service", name)) if !name.is_empty() => Self::Service(name.to_owned()),
                Some(("exec", seq)) => Self::Exec(seq.parse().map_err(|_| bad())?),
                _ => return Err(bad()),
            },
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogQuery {
    pub source: LogSource,
    pub tail: u32,
    pub grep: Option<String>,
    pub since: Option<LogCursor>,
}

/// Logical byte offset; survives rotation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LogCursor(pub u64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogSlice {
    pub lines: Vec<String>,
    pub cursor: LogCursor,
    pub truncated: bool,
    pub matched: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DbQuery {
    pub sql: String,
    pub params: Vec<DbValue>,
    pub write: bool,
    pub max_rows: u32,
    pub timeout: Duration,
}

/// Other types are rendered as `Text`.
#[derive(Debug, Clone, PartialEq)]
pub enum DbValue {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Text(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DbColumn {
    pub name: String,
    pub type_name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DbRows {
    pub columns: Vec<DbColumn>,
    pub rows: Vec<Vec<DbValue>>,
    pub row_count: u64,
    pub truncated: bool,
}

// ---------------------------------------------------------------------------------------------
// Patch types (shared with fix.md, which owns the exclusion policy)
// ---------------------------------------------------------------------------------------------

/// Hard cap on `PatchOutput.patch`.
pub const PATCH_MAX_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Baseline {
    /// Tree at sha.
    Source,
    /// After build + migrate.
    Prepared,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffOptions {
    pub against: Baseline,
    pub exclude: Vec<PathGlob>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchOutput {
    pub patch: Vec<u8>,
    /// Every changed path that is not excluded, binary ones included (flagged).
    pub files: Vec<PatchFile>,
    pub excluded: Vec<RelPath>,
    /// Listed, never encoded in `patch`.
    pub binary: Vec<RelPath>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchFile {
    pub path: RelPath,
    pub change: FileChange,
    pub binary: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FileChange {
    Added,
    Modified,
    Deleted,
    ModeChanged,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyOutcome {
    Applied { files: Vec<RelPath>, patch_sha256: [u8; 32] },
    Rejected { reason: RejectReason, detail: String, paths: Vec<RelPath> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RejectReason {
    BaseMismatch,
    DoesNotApply,
    Malformed,
}

// ---------------------------------------------------------------------------------------------
// Shared semantics (one implementation for every adapter and the fakes)
// ---------------------------------------------------------------------------------------------

/// Expands a recipe template (`mage_core::domain::recipe::template_parts` grammar): `${name}` is
/// looked up with `lookup`, `$${` is a literal `${`. Unknown variables and unterminated `${` are
/// `Invalid`.
pub fn render_template(template: &str, lookup: impl Fn(&str) -> Option<String>) -> Result<String, ExecError> {
    use mage_core::domain::recipe::{TemplatePart, template_parts};
    let mut out = String::with_capacity(template.len());
    for part in template_parts(template) {
        match part.map_err(|e| ExecError::Invalid(format!("template: {e}")))? {
            TemplatePart::Literal(text) => out.push_str(text),
            TemplatePart::Var(name) => match lookup(name) {
                Some(value) => out.push_str(&value),
                None => return Err(ExecError::Invalid(format!("unknown or unavailable template variable ${{{name}}}"))),
            },
        }
    }
    Ok(out)
}

/// Steps `Env::rebuild(scope)` runs and skips, in recipe order. `All` runs every step;
/// `Changed(paths)` runs a step when its `inputs` is empty or any input glob matches any changed
/// path (an input that is not a valid glob counts as matching).
pub fn rebuild_plan<'a>(
    recipe: &'a RecipeSpec,
    scope: &RebuildScope,
) -> (Vec<&'a mage_core::domain::recipe::BuildStep>, Vec<&'a mage_core::domain::recipe::BuildStep>) {
    let changed = match scope {
        RebuildScope::All => return (recipe.build.iter().collect(), Vec::new()),
        RebuildScope::Changed(paths) => paths,
    };
    recipe.build.iter().partition(|step| {
        step.inputs.is_empty()
            || step.inputs.iter().any(|input| {
                let pattern = input.strip_prefix("./").unwrap_or(input);
                match Glob::new(pattern) {
                    Ok(glob) => changed.iter().any(|p| glob.matches(p.as_str())),
                    Err(_) => true,
                }
            })
    })
}

/// Bytes inspected for NUL to classify a file as binary.
pub const BINARY_SNIFF_BYTES: usize = 8 * 1024;

/// Streaming implementation of `Env::read` semantics: feed the whole file with [`push`], then
/// [`finish`]. `sha256`, `total_bytes`, and `total_lines` cover the whole file; `binary` = NUL in
/// the first 8 KiB (`total_lines` is `None` then). `Whole` keeps the first `max_bytes`;
/// `Lines { start, count, max_bytes }` keeps lines `start..start+count` (1-based, terminators
/// included) up to `max_bytes`; `Bytes` keeps `offset..offset+len`. `truncated` = the file has
/// content after what was returned.
///
/// [`push`]: ReadCollector::push
/// [`finish`]: ReadCollector::finish
pub struct ReadCollector {
    range: ReadRange,
    hasher: Sha256,
    total: u64,
    newlines: u64,
    last: Option<u8>,
    binary: bool,
    out: Vec<u8>,
    /// 1-based number of the line the next byte belongs to.
    line: u64,
    cut: bool,
}

impl ReadCollector {
    pub fn new(range: ReadRange) -> Self {
        Self {
            range,
            hasher: Sha256::new(),
            total: 0,
            newlines: 0,
            last: None,
            binary: false,
            out: Vec::new(),
            line: 1,
            cut: false,
        }
    }

    pub fn push(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        if (self.total as usize) < BINARY_SNIFF_BYTES {
            let sniff = (BINARY_SNIFF_BYTES - self.total as usize).min(data.len());
            self.binary |= data[..sniff].contains(&0);
        }
        self.hasher.update(data);
        let start_pos = self.total;
        match self.range {
            ReadRange::Whole { max_bytes } => {
                let room = max_bytes.saturating_sub(self.out.len() as u64) as usize;
                self.out.extend_from_slice(&data[..room.min(data.len())]);
            }
            ReadRange::Bytes { offset, len } => {
                let end = offset.saturating_add(len);
                let lo = offset.max(start_pos);
                let hi = end.min(start_pos + data.len() as u64);
                if lo < hi {
                    self.out.extend_from_slice(&data[(lo - start_pos) as usize..(hi - start_pos) as usize]);
                }
            }
            ReadRange::Lines { start, count, max_bytes } => {
                let first = u64::from(start.max(1));
                let end = first + u64::from(count);
                let mut rest = data;
                while !rest.is_empty() {
                    let (segment, ends) = match rest.iter().position(|&b| b == b'\n') {
                        Some(i) => (&rest[..=i], true),
                        None => (rest, false),
                    };
                    if self.line >= first && self.line < end && !self.cut {
                        let room = max_bytes.saturating_sub(self.out.len() as u64) as usize;
                        if segment.len() > room {
                            self.out.extend_from_slice(&segment[..room]);
                            self.cut = true;
                        } else {
                            self.out.extend_from_slice(segment);
                        }
                    }
                    if ends {
                        self.line += 1;
                    }
                    rest = &rest[segment.len()..];
                }
            }
        }
        self.newlines += data.iter().filter(|&&b| b == b'\n').count() as u64;
        self.total += data.len() as u64;
        self.last = data.last().copied();
    }

    pub fn finish(self) -> FileRead {
        let lines = self.newlines + u64::from(self.last.is_some_and(|b| b != b'\n'));
        let truncated = match self.range {
            ReadRange::Whole { max_bytes } => self.total > max_bytes,
            ReadRange::Bytes { offset, len } => self.total > offset.saturating_add(len),
            ReadRange::Lines { start, count, .. } => {
                self.cut || lines > u64::from(start.max(1)).saturating_add(u64::from(count)).saturating_sub(1)
            }
        };
        FileRead {
            bytes: self.out,
            total_bytes: self.total,
            total_lines: (!self.binary).then(|| u32::try_from(lines).unwrap_or(u32::MAX)),
            truncated,
            binary: self.binary,
            sha256: self.hasher.finalize().into(),
        }
    }
}

impl FileRead {
    /// `Env::read` semantics over in-memory content.
    pub fn of(bytes: &[u8], range: ReadRange) -> Self {
        let mut collector = ReadCollector::new(range);
        collector.push(bytes);
        collector.finish()
    }
}

// ---------------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------------

/// One error enum for `Executor` and `Env`.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExecError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("invalid: {0}")]
    Invalid(String),
    /// Toolchain, Postgres, docker, disk.
    #[error("unavailable: {0}")]
    Unavailable(String),
    #[error("path escapes {area:?}: {path}")]
    Confinement { area: Area, path: String },
    #[error("prepare step {step} failed")]
    PrepareFailed { step: String, exit: Option<Exit>, log_tail: String },
    #[error("service {service} unhealthy: {reason}")]
    ServiceUnhealthy { service: String, reason: String, log_tail: String },
    /// sha256 mismatch, unsafe tar entry, size cap.
    #[error("bundle rejected: {0}")]
    BundleRejected(String),
    /// The caller refreshes via `ServerLink` and retries once.
    #[error("signed URL expired: {0}")]
    SourceUrlExpired(String),
    #[error("quota exceeded: {0}")]
    QuotaExceeded(String),
    #[error("env destroyed")]
    Destroyed,
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("io: {0}")]
    Io(String),
}

impl From<std::io::Error> for ExecError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err.to_string())
    }
}

impl From<ExecError> for AppError {
    fn from(err: ExecError) -> Self {
        match err {
            ExecError::NotFound(m) => AppError::NotFound(m),
            ExecError::Conflict(m) => AppError::Conflict(m),
            ExecError::Invalid(m) => AppError::Invalid(m),
            e @ ExecError::Confinement { .. } => AppError::Invalid(e.to_string()),
            ExecError::Unavailable(m) => AppError::Unavailable(m),
            e @ (ExecError::QuotaExceeded(_) | ExecError::SourceUrlExpired(_)) => AppError::Unavailable(e.to_string()),
            e => AppError::Internal(e.to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rel_path_normalizes_and_rejects_escapes() {
        assert_eq!(RelPath::new("./a/b/").map(|p| p.0), Ok("a/b".to_owned()));
        assert!(RelPath::new(".").is_ok_and(|p| p.is_root()));
        assert!(RelPath::new("").is_ok_and(|p| p.is_root()));
        assert!(matches!(RelPath::new("../x"), Err(ExecError::Confinement { area: Area::Workspace, .. })));
        assert!(matches!(RelPath::new("a/../../x"), Err(ExecError::Confinement { .. })));
        assert!(matches!(RelPath::new("/etc/passwd"), Err(ExecError::Confinement { .. })));
        assert!(matches!(RelPath::new("~/x"), Err(ExecError::Confinement { .. })));
        assert!(matches!(EnvPath::scratch("../x"), Err(ExecError::Confinement { area: Area::Scratch, .. })));
        assert!(matches!(RelPath::new("a//b"), Err(ExecError::Invalid(_))));
        assert!(matches!(RelPath::new("a\\b"), Err(ExecError::Invalid(_))));
        assert!(matches!(RelPath::new("a\0b"), Err(ExecError::Invalid(_))));
        assert!(matches!(RelPath::new(&"a".repeat(4097)), Err(ExecError::Invalid(_))));
        assert!(RelPath::new(&"a".repeat(4096)).is_ok());
    }

    #[test]
    fn rel_path_parent_join_and_name() {
        let p = RelPath::new("a/b/c.txt").unwrap();
        assert_eq!(p.file_name(), "c.txt");
        assert_eq!(p.parent().unwrap().as_str(), "a/b");
        assert!(RelPath::new("x").unwrap().parent().unwrap().is_root());
        assert_eq!(RelPath::root().join("a/b").unwrap().as_str(), "a/b");
        assert!(matches!(p.join(".."), Err(ExecError::Confinement { .. })));
    }

    #[test]
    fn env_id_round_trips_through_display() {
        let id = EnvId::from_uuid(Uuid::from_u128(0x0190_0000_0000_7000_8000_0000_0000_0001));
        let s = id.to_string();
        assert!(s.starts_with("env_") && s.len() == 30);
        assert_eq!(s.parse::<EnvId>().unwrap(), id);
        assert!("env_zzzzzzzzzzzzzzzzzzzzzzzzzz".parse::<EnvId>().is_err());
        assert!("run_00000000000000000000000000".parse::<EnvId>().is_err());
    }

    #[test]
    fn base_id_depends_on_every_key_field() {
        let key = BaseKey {
            org_id: OrgId::from_uuid(Uuid::from_u128(1)),
            repo_id: RepoId::from_uuid(Uuid::from_u128(2)),
            sha: CommitSha([3; 20]),
            recipe_hash: None,
            arch: Arch::Aarch64,
            executor: ExecutorKind::Host,
        };
        let id = key.id();
        assert_eq!(id.as_str().len(), 20);
        assert_eq!(BaseId::parse(id.as_str()).unwrap(), id);
        assert_ne!(BaseKey { recipe_hash: Some(RecipeHash([0; 32])), ..key }.id(), id);
        assert_ne!(BaseKey { arch: Arch::X86_64, ..key }.id(), id);
        assert_ne!(BaseKey { sha: CommitSha([4; 20]), ..key }.id(), id);
    }

    #[test]
    fn captured_boundaries() {
        let limits = CaptureLimits { head: 4, tail: 3 };
        let exact = Captured::new(b"abcd", limits);
        assert_eq!((exact.head.as_slice(), exact.tail.as_slice(), exact.total_bytes), (&b"abcd"[..], &b""[..], 4));
        let plus_one = Captured::new(b"abcde", limits);
        assert_eq!((plus_one.head.as_slice(), plus_one.tail.as_slice()), (&b"abcd"[..], &b"e"[..]));
        let long = Captured::new(b"abcdefghij", limits);
        assert_eq!((long.head.as_slice(), long.tail.as_slice(), long.omitted()), (&b"abcd"[..], &b"hij"[..], 3));
    }

    #[test]
    fn reserved_env_names() {
        for name in ["PATH", "HOME", "LD_PRELOAD", "DYLD_INSERT_LIBRARIES", "https_proxy", "No_Proxy", "MAGE_X", "TZ"] {
            assert!(is_reserved_env_name(name), "{name}");
        }
        for name in ["PI_TOKEN", "ANTHROPIC_BASE_URL", "CLAUDE_CONFIG_DIR", "MCP_TIMEOUT", "NODE_ENV"] {
            assert!(!is_reserved_env_name(name), "{name}");
        }
        assert!(check_extra_env(&[("A=B".into(), "x".into())]).is_err());
    }

    #[test]
    fn log_source_round_trips() {
        for s in ["prepare", "build:deps", "service:api", "exec:12", "mcp", "egress"] {
            assert_eq!(s.parse::<LogSource>().unwrap().to_string(), s);
        }
        assert!("exec:x".parse::<LogSource>().is_err());
        assert!("other".parse::<LogSource>().is_err());
    }

    #[test]
    fn process_role_round_trips() {
        for role in [ProcessRole::Harness, ProcessRole::Service("api".into()), ProcessRole::Precheck] {
            assert_eq!(role.to_string().parse::<ProcessRole>().unwrap(), role);
        }
    }

    #[test]
    fn read_lines_window_and_truncation() {
        let text = b"one\ntwo\nthree\nfour";
        let mid = FileRead::of(text, ReadRange::Lines { start: 2, count: 2, max_bytes: 1024 });
        assert_eq!(mid.bytes, b"two\nthree\n");
        assert_eq!((mid.total_lines, mid.truncated, mid.binary), (Some(4), true, false));
        let tail = FileRead::of(text, ReadRange::Lines { start: 3, count: 10, max_bytes: 1024 });
        assert_eq!((tail.bytes.as_slice(), tail.truncated), (&b"three\nfour"[..], false));
        let capped = FileRead::of(text, ReadRange::Lines { start: 1, count: 4, max_bytes: 6 });
        assert_eq!((capped.bytes.as_slice(), capped.truncated), (&b"one\ntw"[..], true));
        assert_eq!(FileRead::of(b"a\n", ReadRange::Whole { max_bytes: 9 }).total_lines, Some(1));
        assert_eq!(FileRead::of(b"", ReadRange::Whole { max_bytes: 9 }).total_lines, Some(0));
    }

    #[test]
    fn read_collector_is_chunking_independent() {
        let mut data = b"line\n".repeat(4000);
        data.extend_from_slice(b"tail");
        let ranges = [
            ReadRange::Whole { max_bytes: 100 },
            ReadRange::Lines { start: 1999, count: 3, max_bytes: 1 << 20 },
            ReadRange::Bytes { offset: 19_990, len: 50 },
        ];
        for range in ranges {
            let whole = FileRead::of(&data, range);
            let mut chunked = ReadCollector::new(range);
            for chunk in data.chunks(7) {
                chunked.push(chunk);
            }
            assert_eq!(chunked.finish(), whole, "{range:?}");
        }
        let bytes = FileRead::of(&data, ReadRange::Bytes { offset: 19_990, len: 50 });
        assert_eq!((bytes.bytes.as_slice(), bytes.truncated), (&b"line\nline\ntail"[..], false));
        assert_eq!(bytes.sha256, <[u8; 32]>::from(Sha256::digest(&data)));
    }

    #[test]
    fn binary_is_nul_in_first_8k_only() {
        let mut late = vec![b'a'; BINARY_SNIFF_BYTES];
        late.push(0);
        assert!(!FileRead::of(&late, ReadRange::Whole { max_bytes: 1 }).binary);
        let early = FileRead::of(b"ab\0cd", ReadRange::Whole { max_bytes: 1 });
        assert!(early.binary);
        assert_eq!(early.total_lines, None);
    }

    #[test]
    fn render_template_expands_and_rejects_unknown() {
        let lookup = |name: &str| match name {
            "port.api" => Some("20001".to_owned()),
            "ws" => Some("/mage/ws".to_owned()),
            _ => None,
        };
        assert_eq!(render_template("http://127.0.0.1:${port.api}${ws}", lookup).unwrap(), "http://127.0.0.1:20001/mage/ws");
        assert_eq!(render_template("cost $5 $${ws}", lookup).unwrap(), "cost $5 ${ws}");
        assert!(matches!(render_template("${port.web}", lookup), Err(ExecError::Invalid(_))));
        assert!(matches!(render_template("${ws", lookup), Err(ExecError::Invalid(_))));
    }

    #[test]
    fn rebuild_plan_skips_steps_whose_inputs_do_not_match() {
        let recipe = RecipeSpec::parse(
            "version = 1\n[[build]]\nname = \"deps\"\nrun = \"npm ci\"\ninputs = [\"package.json\", \"./package-lock.json\"]\n\
             [[build]]\nname = \"gen\"\nrun = \"make gen\"\ninputs = [\"proto/**\"]\n[[build]]\nname = \"compile\"\nrun = \"make\"\n",
        )
        .unwrap();
        let names = |steps: Vec<&mage_core::domain::recipe::BuildStep>| steps.iter().map(|s| s.name.clone()).collect::<Vec<_>>();
        let (run, skipped) = rebuild_plan(&recipe, &RebuildScope::Changed(vec![RelPath::new("src/a.ts").unwrap()]));
        assert_eq!((names(run), names(skipped)), (vec!["compile".to_owned()], vec!["deps".to_owned(), "gen".to_owned()]));
        let (run, _) = rebuild_plan(&recipe, &RebuildScope::Changed(vec![RelPath::new("package-lock.json").unwrap()]));
        assert_eq!(names(run), vec!["deps".to_owned(), "compile".to_owned()]);
        let (run, _) = rebuild_plan(&recipe, &RebuildScope::Changed(vec![RelPath::new("proto/v1/a.proto").unwrap()]));
        assert_eq!(names(run), vec!["gen".to_owned(), "compile".to_owned()]);
        assert_eq!(rebuild_plan(&recipe, &RebuildScope::All).0.len(), 3);
    }
}
