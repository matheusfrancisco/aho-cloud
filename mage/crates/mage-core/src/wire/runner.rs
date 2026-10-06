//! `/runner/v1` protocol DTOs (runs.md "Protocol types" and "Runner protocol"). The server's
//! runner API and the runner's `ServerLink` both use these types directly.
//!
//! Every request and response body of the endpoint table has a type here; per-run requests carry
//! the fencing `attempt`. Durations travel as `*_ms` integers, byte arrays as lowercase hex, times
//! as RFC 3339.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use uuid::Uuid;

use crate::domain::budget::duration_ms;
use crate::domain::fix::{PatchPolicy, RegressionPlacement};
use crate::domain::ids::hex_array;
use crate::domain::{
    ArtifactId, ArtifactKind, Budget, CommitSha, DiagnoseResult, FailStage, FailureSignature, FixResult, GateId, GateKind, HarnessKind,
    OrgId, RecipeHash, RecipeProposal, RepoId, ReproMethod, RunCommand, RunEventKind, RunId, RunKind, RunStatus, RunnerId, Usage,
    VerifyResult, VerifySpec,
};
use crate::ports::blob::HttpMethod;

/// Protocol version header: every call sends `Mage-Protocol: 1`; an unknown major version gets
/// 400 `unsupported_protocol`.
pub const PROTOCOL_HEADER: &str = "Mage-Protocol";
pub const PROTOCOL_VERSION: u32 = 1;

/// Server-side cap on long-poll waits (`POST /lease`, `GET /runs/{id}/commands`).
pub const LONG_POLL_MAX: Duration = Duration::from_secs(25);
/// Event batch limits (`POST /runs/{id}/events`): count, and body bytes after decompression.
pub const MAX_EVENTS_PER_BATCH: usize = 500;
pub const MAX_EVENTS_BATCH_BYTES: usize = 4 * 1024 * 1024;
/// Previews above this are truncated by the server (not rejected).
pub const MAX_PREVIEW_BYTES: usize = 4 * 1024;
/// `LeaseRequest.prepared` carries at most this many bases, newest first.
pub const MAX_PREPARED_BASES: usize = 64;

/// Paths under `/runner/v1` (server.md "Runner protocol"). `{run}` and `{artifact}` take the
/// prefixed id `Display` form.
pub mod routes {
    pub const PREFIX: &str = "/runner/v1";
    pub const REGISTER: &str = "/runner/v1/register";
    pub const LEASE: &str = "/runner/v1/lease";
    pub const HEARTBEAT: &str = "/runner/v1/runs/{run}/heartbeat";
    pub const EVENTS: &str = "/runner/v1/runs/{run}/events";
    /// Query: `attempt`, `after`, `wait_ms` ([`super::CommandsQuery`]).
    pub const COMMANDS: &str = "/runner/v1/runs/{run}/commands";
    pub const ARTIFACTS: &str = "/runner/v1/runs/{run}/artifacts";
    pub const ARTIFACT_COMPLETE: &str = "/runner/v1/runs/{run}/artifacts/{artifact}/complete";
    pub const DOWNLOADS: &str = "/runner/v1/runs/{run}/downloads";
    pub const RESULT: &str = "/runner/v1/runs/{run}/result";
    pub const FINISH: &str = "/runner/v1/runs/{run}/finish";

    /// Every route with its constant name.
    pub const ALL: &[(&str, &str)] = &[
        ("REGISTER", REGISTER),
        ("LEASE", LEASE),
        ("HEARTBEAT", HEARTBEAT),
        ("EVENTS", EVENTS),
        ("COMMANDS", COMMANDS),
        ("ARTIFACTS", ARTIFACTS),
        ("ARTIFACT_COMPLETE", ARTIFACT_COMPLETE),
        ("DOWNLOADS", DOWNLOADS),
        ("RESULT", RESULT),
        ("FINISH", FINISH),
    ];
}

// ---------------------------------------------------------------------------------------------
// Capabilities
// ---------------------------------------------------------------------------------------------

/// CPU architecture of a runner (and of a prepared base; Verify runs are pinned to the parent's).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Arch {
    #[serde(rename = "x86_64")]
    X86_64,
    #[serde(rename = "aarch64")]
    Aarch64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Os {
    #[serde(rename = "macos")]
    MacOs,
    #[serde(rename = "linux")]
    Linux,
}

/// `Host` = v1 process isolation on the runner host; `Aho` = v2 aho-cloud VMs (executor.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutorKind {
    Host,
    Aho,
}

/// How a harness reaches its model (harness.md `CredentialsMode` without the secrets).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialModeKind {
    /// Developer's own harness login (dev only).
    LocalLogin,
    /// Mage model gateway with a run-bound `gateway_token`.
    Gateway,
    /// Direct Vertex with the runner VM's credentials (internal deployments).
    Vertex,
}

/// One installed harness the runner can launch (harness.md `HarnessInfo`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessAvailability {
    pub kind: HarnessKind,
    pub version: String,
    pub credential_modes: Vec<CredentialModeKind>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub arch: Arch,
    pub os: Os,
    pub executor: ExecutorKind,
    pub harnesses: Vec<HarnessAvailability>,
    /// Playwright and its browsers are installed.
    pub browser: bool,
    pub slots: SlotCounts,
}

impl Capabilities {
    /// The runner advertises `kind` with `mode` (lease matching: a run's harness requirement).
    pub fn offers_harness(&self, kind: HarnessKind, mode: CredentialModeKind) -> bool {
        self.harnesses.iter().any(|h| h.kind == kind && h.credential_modes.contains(&mode))
    }
}

/// Concurrency class of a run on a runner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlotClass {
    /// Triage.
    Light,
    /// Learn, Diagnose, Fix.
    Work,
    Verify,
}

impl SlotClass {
    pub const fn of(kind: RunKind) -> SlotClass {
        match kind {
            RunKind::Triage => SlotClass::Light,
            RunKind::Learn | RunKind::Diagnose | RunKind::Fix => SlotClass::Work,
            RunKind::Verify => SlotClass::Verify,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub struct SlotCounts {
    pub light: u16,
    pub work: u16,
    pub verify: u16,
}

impl SlotCounts {
    pub const fn get(&self, class: SlotClass) -> u16 {
        match class {
            SlotClass::Light => self.light,
            SlotClass::Work => self.work,
            SlotClass::Verify => self.verify,
        }
    }

    pub const fn total(&self) -> u32 {
        self.light as u32 + self.work as u32 + self.verify as u32
    }
}

/// A warm base the runner already prepared (`Executor::capabilities().bases`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PreparedBaseKey {
    pub repo_id: RepoId,
    pub sha: CommitSha,
    /// `None` = SourceOnly base (Learn without a validated recipe).
    pub recipe_hash: Option<RecipeHash>,
}

// ---------------------------------------------------------------------------------------------
// POST /register
// ---------------------------------------------------------------------------------------------

/// Credential presented to `/register` (grant shapes owned by server.md).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RegisterGrant {
    /// First start: one-time `mgb_` token plus the runner's own Ed25519 public key.
    Bootstrap {
        bootstrap_token: String,
        name: String,
        #[serde(with = "hex_array")]
        public_key: [u8; 32],
    },
    /// Restart and token refresh: a runner assertion signed with the registered key
    /// ([`crate::wire::token::TokenCodec::sign_assertion`]).
    Assertion { runner_id: RunnerId, assertion: String },
}

/// `POST /register` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registration {
    pub grant: RegisterGrant,
    /// Random per process start; the runner token's `inst` claim.
    pub instance_id: Uuid,
    pub version: String,
    pub capabilities: Capabilities,
    /// `RunnerState::active_runs` at startup (informational).
    pub held: Vec<HeldRun>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct HeldRun {
    pub run_id: RunId,
    pub attempt: u16,
}

/// Lease timing the runner must follow; on the wire `lease_ttl_ms`, `heartbeat_every_ms`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseTerms {
    #[serde(rename = "lease_ttl_ms", with = "duration_ms")]
    pub ttl: Duration,
    #[serde(rename = "heartbeat_every_ms", with = "duration_ms")]
    pub heartbeat_every: Duration,
}

/// `POST /register` response.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistrationAck {
    pub runner_id: RunnerId,
    pub runner_token: String,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
    /// Every lease of a previous instance of this runner, now lost.
    pub released: Vec<ReleasedRun>,
    #[serde(flatten)]
    pub lease: LeaseTerms,
    #[serde(rename = "long_poll_max_ms", with = "duration_ms")]
    pub long_poll_max: Duration,
    pub min_version: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleasedRun {
    pub run_id: RunId,
    pub attempt: u16,
    pub status: RunStatus,
}

// ---------------------------------------------------------------------------------------------
// POST /lease
// ---------------------------------------------------------------------------------------------

/// `POST /lease` body (long-poll).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseRequest {
    /// Reused verbatim when retrying the same logical request: the server returns the same
    /// assignment.
    pub request_id: Uuid,
    #[serde(rename = "wait_ms", with = "duration_ms")]
    pub wait: Duration,
    pub free: SlotCounts,
    /// Newest first, at most [`MAX_PREPARED_BASES`].
    pub prepared: Vec<PreparedBaseKey>,
}

/// `POST /lease` response; `assignment: null` when the wait elapsed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LeaseResponse {
    pub assignment: Option<RunAssignment>,
}

/// Everything a runner needs to execute one attempt of a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunAssignment {
    pub run_id: RunId,
    pub org_id: OrgId,
    /// Lease fencing token; every per-run call echoes it.
    pub attempt: u16,
    pub kind: RunKind,
    pub lease: LeaseTerms,
    /// `None` for repo-less Triage.
    pub repo: Option<RepoRef>,
    pub sha: Option<CommitSha>,
    /// SourceBundle (`tar.zst`); `None` for Triage.
    pub source: Option<Download>,
    /// Learn: the current recipe if any; Diagnose/Fix/Verify: the frozen recipe.
    pub recipe: Option<RecipeRef>,
    /// `None` for Verify.
    pub harness: Option<HarnessAssignment>,
    /// `Some` for Verify only.
    pub verify: Option<VerifySpec>,
    /// Artifacts referenced by `verify` (script, patch, …).
    pub downloads: Vec<Download>,
    pub knowledge: Option<KnowledgeAccess>,
    /// `Some` iff the harness credential mode is `Gateway`.
    pub gateway: Option<GatewayAccess>,
    pub budget: Budget,
    /// Gate feedback from earlier attempts of this run.
    pub prior_feedback: Vec<String>,
    /// Fix runs only (fix.md).
    pub fix: Option<FixSpec>,
    /// W3C `traceparent` of the run (server.md).
    pub trace: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoRef {
    pub repo_id: RepoId,
    pub owner: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecipeRef {
    pub hash: RecipeHash,
    pub toml: String,
}

/// Knowledge MCP endpoint and the run-bound `mgk_` token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnowledgeAccess {
    pub url: String,
    pub knowledge_token: String,
}

/// Model gateway base URL (`{public_url}/llm/anthropic`) and the run-bound `mgg_` token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GatewayAccess {
    pub base_url: String,
    pub gateway_token: String,
}

/// Harness launch inputs rendered by the server per kind (harness.md).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessAssignment {
    pub kind: HarnessKind,
    pub model: String,
    pub credential_mode: CredentialModeKind,
    pub prompt: String,
    pub system_append: String,
}

/// Inputs of a Fix run the runner enforces locally (fix.md Additions).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixSpec {
    /// `FixBrief` artifact (prompt source).
    pub brief: ArtifactId,
    /// Repro script artifact, placed at `.mage/repro/{cause_id}/{name}` in the fork.
    pub script: ArtifactId,
    /// Pin of the script bytes; the runner checks it after download.
    #[serde(with = "hex_array")]
    pub script_sha256: [u8; 32],
    pub signature: FailureSignature,
    /// Exclusions and deny rules for `submit_fix`.
    pub policy: PatchPolicy,
    pub regression: RegressionPlacement,
}

/// Something a run may download.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum DownloadRef {
    SourceBundle { repo_id: RepoId, sha: CommitSha },
    Artifact(ArtifactId),
}

/// A signed GET URL with the bytes it must yield. `name` is the artifact name (e.g. the repro
/// script file name, which decides `ScriptKind`); `None` for SourceBundles.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Download {
    pub what: DownloadRef,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub url: String,
    pub bytes: u64,
    #[serde(with = "hex_array")]
    pub sha256: [u8; 32],
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
}

// ---------------------------------------------------------------------------------------------
// POST /runs/{id}/heartbeat
// ---------------------------------------------------------------------------------------------

/// Local phase reported by heartbeats; ordered, so "ahead of the status" is a comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerPhase {
    Preparing,
    Running,
    AwaitingGate,
    /// Post-gate work and teardown (`RunnerState` `PostGate`, `TearingDown`).
    Finishing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeartbeatRequest {
    pub attempt: u16,
    pub phase: RunnerPhase,
}

/// On the wire `{ status, lease_ttl_ms, heartbeat_every_ms, commands_pending }`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HeartbeatAck {
    pub status: RunStatus,
    #[serde(flatten)]
    pub lease: LeaseTerms,
    pub commands_pending: bool,
}

// ---------------------------------------------------------------------------------------------
// POST /runs/{id}/events
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboundEvent {
    /// Monotonic per attempt; the server's ingest cursor drops `(attempt, local_seq)` it has seen.
    pub local_seq: u64,
    #[serde(with = "time::serde::rfc3339")]
    pub at: OffsetDateTime,
    pub kind: OutboundEventKind,
}

/// The subset of `RunEventKind` a runner may send (status, gate and artifact events are
/// server-only). Same JSON shape as the matching `RunEventKind` variants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum OutboundEventKind {
    AssistantText(String),
    ToolCall {
        name: String,
        args_preview: String,
    },
    ToolResult {
        name: String,
        ok: bool,
        preview: String,
    },
    /// Cumulative for the attempt; not sent in Gateway mode.
    Usage(Usage),
    Notice(String),
    Error(String),
}

impl From<OutboundEventKind> for RunEventKind {
    fn from(kind: OutboundEventKind) -> Self {
        match kind {
            OutboundEventKind::AssistantText(text) => RunEventKind::AssistantText(text),
            OutboundEventKind::ToolCall { name, args_preview } => RunEventKind::ToolCall { name, args_preview },
            OutboundEventKind::ToolResult { name, ok, preview } => RunEventKind::ToolResult { name, ok, preview },
            OutboundEventKind::Usage(usage) => RunEventKind::Usage(usage),
            OutboundEventKind::Notice(text) => RunEventKind::Notice(text),
            OutboundEventKind::Error(text) => RunEventKind::Error(text),
        }
    }
}

/// `POST /runs/{id}/events` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventsRequest {
    pub attempt: u16,
    pub events: Vec<OutboundEvent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventsAck {
    pub acked_local_seq: u64,
    pub last_seq: u64,
    pub status: RunStatus,
}

// ---------------------------------------------------------------------------------------------
// GET /runs/{id}/commands
// ---------------------------------------------------------------------------------------------

/// Query string of `GET /runs/{id}/commands?attempt=&after=&wait_ms=`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandsQuery {
    pub attempt: u16,
    /// Last applied command `seq` (0 = from the start of the attempt).
    pub after: u64,
    #[serde(rename = "wait_ms", with = "duration_ms")]
    pub wait: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandEnvelope {
    pub seq: u64,
    pub attempt: u16,
    #[serde(with = "time::serde::rfc3339")]
    pub issued_at: OffsetDateTime,
    pub command: RunCommand,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandBatch {
    pub commands: Vec<CommandEnvelope>,
    pub status: RunStatus,
}

// ---------------------------------------------------------------------------------------------
// Artifacts and downloads
// ---------------------------------------------------------------------------------------------

/// What the runner declares before uploading; `sha256` is the runner's claim (consumers that act
/// on the bytes recompute it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactSpec {
    pub kind: ArtifactKind,
    /// `[a-z0-9][a-z0-9._-]{0,127}` (storage.md `BlobKey` segment), see [`ArtifactSpec::name_valid`].
    pub name: String,
    pub bytes: u64,
    #[serde(with = "hex_array")]
    pub sha256: [u8; 32],
    pub content_type: String,
}

impl ArtifactSpec {
    /// Kinds a runner may upload (runs.md "RequestUpload").
    pub const fn kind_allowed(kind: ArtifactKind) -> bool {
        matches!(
            kind,
            ArtifactKind::Video
                | ArtifactKind::Gif
                | ArtifactKind::Trace
                | ArtifactKind::Screenshot
                | ArtifactKind::Log
                | ArtifactKind::Script
                | ArtifactKind::Patch
                | ArtifactKind::Transcript
                | ArtifactKind::ScipIndex
        )
    }

    /// Size cap per kind: Video 2 GiB, others 256 MiB.
    pub const fn max_bytes(kind: ArtifactKind) -> u64 {
        match kind {
            ArtifactKind::Video => 2 * 1024 * 1024 * 1024,
            _ => 256 * 1024 * 1024,
        }
    }

    /// `name` matches `[a-z0-9][a-z0-9._-]{0,127}` (the storage.md key rule enforced by
    /// `BlobKey::run_artifact`), so a runner cannot traverse into other prefixes of
    /// `org/{org}/runs/{run}/{kind}/{name}`.
    pub fn name_valid(name: &str) -> bool {
        let bytes = name.as_bytes();
        (1..=128).contains(&bytes.len())
            && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
            && bytes.iter().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(b))
    }
}

/// `POST /runs/{id}/artifacts` body: `{ attempt, kind, name, bytes, sha256, content_type }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadRequest {
    pub attempt: u16,
    #[serde(flatten)]
    pub spec: ArtifactSpec,
}

/// Signed upload URL. The runner token is never sent to the blob host.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadGrant {
    pub artifact_id: ArtifactId,
    pub url: String,
    pub method: HttpMethod,
    /// Sent verbatim with the upload (they are signed, e.g. `Content-Type`).
    pub headers: Vec<(String, String)>,
    #[serde(with = "time::serde::rfc3339")]
    pub expires_at: OffsetDateTime,
    /// Retry of an artifact that was already completed: skip the upload.
    pub already_complete: bool,
}

/// `POST /runs/{id}/artifacts/{artifact_id}/complete` body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompleteUploadRequest {
    pub attempt: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactState {
    Pending,
    Complete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompleteUploadAck {
    pub artifact_id: ArtifactId,
    pub state: ArtifactState,
}

/// `POST /runs/{id}/downloads` body: re-sign URLs the run may read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshDownloadsRequest {
    pub attempt: u16,
    pub refs: Vec<DownloadRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefreshDownloadsResponse {
    pub downloads: Vec<Download>,
}

// ---------------------------------------------------------------------------------------------
// POST /runs/{id}/result
// ---------------------------------------------------------------------------------------------

/// Result of one run, per `RunKind`; serialized as `{"kind": "learn" | "triage" | …, …}`.
/// Mirrors `RunResult` except that Triage carries the raw agent submission.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WireRunResult {
    Learn(RecipeProposal),
    Triage(TriageSubmission),
    Diagnose(DiagnoseResult),
    Verify(VerifyResult),
    Fix(FixResult),
}

impl WireRunResult {
    /// The `RunKind` this result belongs to (must equal `run.kind`).
    pub const fn kind(&self) -> RunKind {
        match self {
            WireRunResult::Learn(_) => RunKind::Learn,
            WireRunResult::Triage(_) => RunKind::Triage,
            WireRunResult::Diagnose(_) => RunKind::Diagnose,
            WireRunResult::Verify(_) => RunKind::Verify,
            WireRunResult::Fix(_) => RunKind::Fix,
        }
    }
}

/// Raw `submit_triage` input, forwarded unchanged by the runner (triage.md). The server
/// re-validates everything; unknown fields are rejected.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriageSubmission {
    pub summary: String,
    pub causes: Vec<CauseDraftDto>,
    pub missing_info: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CauseDraftDto {
    pub title: String,
    pub explanation: String,
    pub evidence: Vec<CitationDraftDto>,
    pub repro_steps: Vec<String>,
    pub confirm_if: String,
    pub rule_out_if: String,
    pub confidence: f64,
    pub method: ReproMethod,
}

/// Agent citation: repo-relative path at the target commit, inclusive 1-based lines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CitationDraftDto {
    pub path: String,
    pub start_line: u32,
    pub end_line: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quote: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// `POST /runs/{id}/result` body. `submission_id` is the idempotency key: reused only to retry
/// the same payload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubmitResultRequest {
    pub attempt: u16,
    pub submission_id: Uuid,
    pub result: WireRunResult,
}

/// `POST /runs/{id}/result` 200 body. `message` is written for the agent and returned verbatim as
/// the `submit_*` tool result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubmitAck {
    pub status: RunStatus,
    pub gate: Option<GateTicket>,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GateTicket {
    pub gate_id: GateId,
    pub kind: GateKind,
    pub attempt: u8,
}

/// A 422 `rejected` answer as the runner surfaces it (MCP tool error with `message` and items).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rejection {
    pub gate: Option<GateTicket>,
    pub message: String,
    pub items: Vec<RejectionItem>,
}

/// One problem, located by a JSON path into the submission (e.g. `causes[0].evidence[1]`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct RejectionItem {
    pub path: String,
    pub reason: String,
}

// ---------------------------------------------------------------------------------------------
// POST /runs/{id}/finish
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinishReport {
    pub outcome: FinishOutcome,
    pub message: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FinishOutcome {
    /// Server status is terminal; env destroyed.
    Done,
    /// Harness ended without `submit_*` after the nudge.
    NoResult,
    /// Runner-side failure. `retryable` hands the lease back (`LeaseLost`, requeue by attempt
    /// count); otherwise the run ends `Errored` with `RunError::Runner { stage }`.
    Failed { stage: FailStage, reason: String, retryable: bool },
    /// Runner self-fenced or is shutting down.
    Abandoned,
}

/// `POST /runs/{id}/finish` body: `{ attempt, outcome, message }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinishRequest {
    pub attempt: u16,
    #[serde(flatten)]
    pub report: FinishReport,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FinishAck {
    pub status: RunStatus,
}

// ---------------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------------

/// `code` of [`WireError`], one per row of runs.md "Errors".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireErrorCode {
    Invalid,
    UnsupportedProtocol,
    Unauthorized,
    Forbidden,
    NotFound,
    LeaseLost,
    RunTerminal,
    InvalidState,
    IdempotencyMismatch,
    TooLarge,
    Rejected,
    RateLimited,
    Internal,
    Unavailable,
}

impl WireErrorCode {
    pub const fn http_status(self) -> u16 {
        match self {
            WireErrorCode::Invalid | WireErrorCode::UnsupportedProtocol => 400,
            WireErrorCode::Unauthorized => 401,
            WireErrorCode::Forbidden => 403,
            WireErrorCode::NotFound => 404,
            WireErrorCode::LeaseLost | WireErrorCode::RunTerminal | WireErrorCode::InvalidState | WireErrorCode::IdempotencyMismatch => 409,
            WireErrorCode::TooLarge => 413,
            WireErrorCode::Rejected => 422,
            WireErrorCode::RateLimited => 429,
            WireErrorCode::Internal => 500,
            WireErrorCode::Unavailable => 503,
        }
    }

    /// The link adapter retries these with backoff (runs.md "Retry rules"); network errors too.
    pub const fn retryable(self) -> bool {
        matches!(self, WireErrorCode::Internal | WireErrorCode::Unavailable | WireErrorCode::RateLimited)
    }
}

/// Error body of every `/runner/v1` endpoint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireError {
    pub code: WireErrorCode,
    pub message: String,
    /// Run status, set with `run_terminal` / `invalid_state`.
    pub status: Option<RunStatus>,
    pub details: Vec<RejectionItem>,
    pub retry_after_ms: Option<u64>,
    /// The gate attempt a `rejected` answer consumed (triage `Reject`/`Fail`), so the runner's
    /// [`Rejection`] can name it.
    #[serde(default)]
    pub gate: Option<GateTicket>,
}

impl WireError {
    pub fn new(code: WireErrorCode, message: impl Into<String>) -> Self {
        WireError { code, message: message.into(), status: None, details: Vec::new(), retry_after_ms: None, gate: None }
    }

    /// 422 `rejected` carrying `rejection`.
    pub fn rejected(rejection: Rejection) -> Self {
        WireError { details: rejection.items, gate: rejection.gate, ..WireError::new(WireErrorCode::Rejected, rejection.message) }
    }

    /// The [`Rejection`] of a `rejected` error; `None` for every other code.
    pub fn rejection(&self) -> Option<Rejection> {
        (self.code == WireErrorCode::Rejected).then(|| Rejection {
            gate: self.gate,
            message: self.message.clone(),
            items: self.details.clone(),
        })
    }
}

#[cfg(test)]
mod tests;
