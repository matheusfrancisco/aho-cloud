//! The control-plane HTTP API as data. The server (`cloud-control-plane`), the CLI, the aho
//! `deploy` capability and the web console (through generated TypeScript) all use these types;
//! none of them spells a path or a body by hand.
//!
//! Conventions: JSON bodies; `Authorization: Bearer <token>`; errors are [`ApiError`] with a
//! stable `code`; binary uploads (`archives`, `sessions`) are raw request bodies.

#![forbid(unsafe_code)]

#[cfg(feature = "client")]
pub mod client;
pub mod internal;
pub mod ts;

use cloud_contract::diff::SurfaceDiff;
use cloud_contract::edge::EdgeConfigError;
use cloud_contract::ledger::{LedgerBody, Provenance, Stage};
use cloud_contract::manifest::{DomainMode, Manifest};
use cloud_contract::policy::{OnCallAction, ReleasePolicy, Requirement, StandingOrder};
use cloud_contract::validate::Refusal;
use serde::{Deserialize, Serialize};
use ts_rs::TS;

/// Route templates. `{p}` = project slug, `{r}` = release id, `{org}` = org slug, `{host}` = hostname.
pub mod routes {
    pub const DEVICE_START: &str = "/v1/auth/device/start";
    pub const DEVICE_APPROVE: &str = "/v1/auth/device/approve";
    pub const DEVICE_POLL: &str = "/v1/auth/device/poll";
    pub const PASSKEY_REGISTER_START: &str = "/v1/auth/passkey/register/start";
    pub const PASSKEY_REGISTER_FINISH: &str = "/v1/auth/passkey/register/finish";
    pub const PASSKEY_LOGIN_START: &str = "/v1/auth/passkey/login/start";
    pub const PASSKEY_LOGIN_FINISH: &str = "/v1/auth/passkey/login/finish";
    pub const ME: &str = "/v1/me";
    pub const ORGS: &str = "/v1/orgs";
    pub const ORG_MEMBERS: &str = "/v1/orgs/{org}/members";
    pub const ORG_POLICY: &str = "/v1/orgs/{org}/policy";
    pub const ORG_PROJECTS: &str = "/v1/orgs/{org}/projects";
    pub const PROJECT: &str = "/v1/projects/{p}";
    pub const ARCHIVES: &str = "/v1/projects/{p}/archives";
    pub const SESSIONS: &str = "/v1/projects/{p}/sessions";
    pub const MANIFEST_CHECK: &str = "/v1/manifests/check";
    pub const RELEASES: &str = "/v1/projects/{p}/releases";
    pub const RELEASE: &str = "/v1/projects/{p}/releases/{r}";
    pub const APPROVE: &str = "/v1/projects/{p}/releases/{r}/approve";
    pub const REJECT: &str = "/v1/projects/{p}/releases/{r}/reject";
    pub const PROMOTE: &str = "/v1/projects/{p}/releases/{r}/promote";
    pub const ROLLBACK: &str = "/v1/projects/{p}/rollback";
    pub const STANDING_ORDERS: &str = "/v1/projects/{p}/standing-orders";
    pub const ONCALL: &str = "/v1/projects/{p}/oncall";
    pub const DOMAINS: &str = "/v1/projects/{p}/domains";
    pub const DOMAIN_VERIFY: &str = "/v1/projects/{p}/domains/{host}/verify";
    pub const WORKSPACES: &str = "/v1/projects/{p}/workspaces";
    pub const LEDGER: &str = "/v1/projects/{p}/ledger";
    pub const SECRETS: &str = "/v1/projects/{p}/secrets";

    /// Fills `{name}` placeholders in order of `args`.
    pub fn fill(template: &str, args: &[(&str, &str)]) -> String {
        let mut out = template.to_owned();
        for (k, v) in args {
            out = out.replace(&format!("{{{k}}}"), v);
        }
        out
    }
}

// ---- errors ---------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS, thiserror::Error)]
#[error("{code}: {message}")]
pub struct ApiError {
    /// Stable, snake_case: `unauthorized`, `forbidden`, `not_found`, `conflict`, `invalid`,
    /// `refused`, `quorum_not_met`, `tainted`, `rate_limited`, `internal`.
    pub code: String,
    pub message: String,
    /// Manifest refusals, when `code = "refused"`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refusals: Vec<Refusal>,
}

impl ApiError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self { code: code.into(), message: message.into(), refusals: Vec::new() }
    }
}

// ---- auth -----------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct DeviceStart {
    /// Human-readable name shown on the approval screen, e.g. `ana@laptop`.
    pub device_name: String,
    /// Ed25519 public key (32 bytes, base64url) the device signs approvals with.
    pub public_key: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct DeviceStarted {
    pub device_code: String,
    /// Short code the user types in the console, e.g. `WDJB-MJHT`.
    pub user_code: String,
    pub verification_uri: String,
    pub interval_secs: u32,
    pub expires_in_secs: u32,
}

/// Sent by the logged-in console user to bind a pending device to themselves.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct DeviceApprove {
    pub user_code: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct DevicePoll {
    pub device_code: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DevicePolled {
    Pending,
    Expired,
    Approved { token: String, key_id: String, user: User },
}

/// WebAuthn options as JSON the browser passes to `navigator.credentials.*` after base64url
/// decoding. Opaque to everything but the server and the console.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
pub struct PasskeyChallenge {
    pub ceremony_id: String,
    #[ts(type = "unknown")]
    pub options: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
pub struct PasskeyRegisterStart {
    pub email: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
pub struct PasskeyLoginStart {
    pub email: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
pub struct PasskeyFinish {
    pub ceremony_id: String,
    /// The browser's `PublicKeyCredential` serialized as JSON (base64url binary fields).
    #[ts(type = "unknown")]
    pub credential: serde_json::Value,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct Session {
    pub token: String,
    pub user: User,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct User {
    pub id: String,
    pub email: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct Me {
    pub user: User,
    pub orgs: Vec<Membership>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct Membership {
    pub org: String,
    pub owner: bool,
}

// ---- orgs and projects ----------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct CreateOrg {
    pub slug: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct Org {
    pub slug: String,
    pub name: String,
    pub policy: ReleasePolicy,
    pub members: Vec<Member>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct Member {
    pub user: User,
    pub owner: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct AddMember {
    pub email: String,
    pub owner: bool,
}

/// Changing the release policy is itself an owner-approved, signed action.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
pub struct SetPolicy {
    pub policy: ReleasePolicy,
    pub proof: ApprovalProof,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct CreateProject {
    pub slug: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct Project {
    pub slug: String,
    pub org: String,
    /// Release id active per stage.
    pub active: Vec<StageHead>,
    pub domains: Vec<DomainView>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct StageHead {
    pub stage: Stage,
    pub release: String,
}

// ---- uploads --------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ArchiveUploaded {
    /// `blake3:<hex>` of the uploaded bytes; use as `source.hash`.
    pub hash: String,
    pub bytes: u64,
}

/// An aho session log whose hash chain the server verified.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct SessionUploaded {
    pub session_id: String,
    /// Hash of the verified head entry.
    pub head_hash: String,
    pub entries: u64,
}

// ---- manifests and releases -----------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct CheckManifest {
    pub manifest: Manifest,
    /// `edge.toml` text, if the project ships one.
    #[serde(default)]
    pub edge_toml: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct CheckResult {
    pub refusals: Vec<Refusal>,
    pub edge_errors: Vec<String>,
}

impl CheckResult {
    pub fn accepted(&self) -> bool {
        self.refusals.is_empty() && self.edge_errors.is_empty()
    }

    pub fn of(manifest: &Manifest, edge_toml: Option<&str>) -> Self {
        let refusals = cloud_contract::validate(manifest).err().unwrap_or_default();
        let edge_errors = match edge_toml.map(cloud_contract::edge::EdgeConfig::parse) {
            None => Vec::new(),
            Some(Err(e)) => vec![e.to_string()],
            Some(Ok(cfg)) => {
                let mut errs: Vec<String> = cfg.compile().err().map(|e: EdgeConfigError| e.to_string()).into_iter().collect();
                errs.extend(cfg.check_against(manifest).err().unwrap_or_default().iter().map(ToString::to_string));
                errs
            }
        };
        Self { refusals, edge_errors }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ProposeRelease {
    pub manifest: Manifest,
    #[serde(default)]
    pub edge_toml: Option<String>,
    /// `Attested` only for a session previously uploaded to this project.
    pub provenance: Provenance,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseStatus {
    /// Waiting for the cloud build.
    Building,
    BuildFailed,
    /// Built; waiting for approvals to reach the next stage.
    AwaitingApproval,
    /// Blocked until the listed secrets are rotated.
    Tainted,
    Rejected,
    /// Active at `stage`.
    Active,
    /// Was active, replaced by a later activation.
    Superseded,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct Release {
    pub id: String,
    pub project: String,
    pub manifest_hash: String,
    pub manifest: Manifest,
    pub provenance: Provenance,
    #[serde(default)]
    pub image_digest: Option<String>,
    /// Highest stage this release has been activated at.
    #[serde(default)]
    pub stage: Option<Stage>,
    pub status: ReleaseStatus,
    /// Against the release currently live (or nothing, for the first).
    pub diff: SurfaceDiff,
    /// What the next promotion needs.
    pub requirement: Requirement,
    pub approvals: Vec<ApprovalView>,
    pub taint: Vec<cloud_contract::ledger::TaintFinding>,
    /// Hosts the staging run reached that the manifest did not declare.
    pub undeclared_egress: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ApprovalView {
    pub stage: Stage,
    pub user: User,
    pub owner: bool,
}

/// Proof that a specific human approved [`cloud_contract::ledger::approval_message`].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
#[serde(tag = "method", rename_all = "snake_case")]
pub enum ApprovalProof {
    /// Ed25519 signature (base64url) by a device key bound at `ahocloud login`.
    DeviceKey { key_id: String, signature: String },
    /// A WebAuthn assertion (the browser's `PublicKeyCredential` as JSON, base64url binary
    /// fields) whose challenge is SHA-256 of the approval message.
    Passkey {
        #[ts(type = "unknown")]
        credential: serde_json::Value,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
pub struct Approve {
    pub stage: Stage,
    pub proof: ApprovalProof,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct Reject {
    pub stage: Stage,
    pub reason: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct Promote {
    pub stage: Stage,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
pub struct Rollback {
    pub release: String,
    /// Required when the target does not fit within the live release's surface.
    #[serde(default)]
    pub proof: Option<ApprovalProof>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, TS)]
pub struct GrantStandingOrders {
    pub orders: Vec<StandingOrder>,
    pub ttl_secs: u64,
    pub proof: ApprovalProof,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct OnCall {
    pub action: OnCallAction,
    /// Rollback target when `action` is a rollback.
    #[serde(default)]
    pub release: Option<String>,
}

// ---- domains --------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct AddDomain {
    pub hostname: String,
    pub mode: DomainMode,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum DomainState {
    Pending,
    Verified,
    /// TXT record disappeared; serving continues until the grace period ends.
    Grace,
    Detached,
    Tombstoned,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct DnsRecord {
    /// `TXT`, `CNAME`, `A`, `AAAA`, `NS`.
    pub kind: String,
    pub name: String,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct DomainView {
    pub hostname: String,
    pub mode: DomainMode,
    pub state: DomainState,
    /// Records the customer must create; stays listed after verification.
    pub records: Vec<DnsRecord>,
}

// ---- secrets --------------------------------------------------------------------------------

/// Sets a secret value for a stage. The value is stored encrypted and never returned.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct SetSecret {
    pub name: String,
    pub stage: Stage,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct SecretView {
    pub name: String,
    pub stages: Vec<Stage>,
    /// Keyed fingerprint (HMAC) used by the taint scan; never the value.
    pub fingerprint: String,
}

// ---- workspaces (Entry A) -------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct Workspace {
    /// `wss://…/rpc` speaking aho rpc, one text frame per NDJSON line.
    pub rpc_url: String,
    /// Short-lived token for the rpc socket (`?token=`).
    pub token: String,
    /// `None` until the first connection's `hello_ok`; the rpc handshake always carries it.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Preview URL on the edge.
    pub preview_url: String,
}

// ---- ledger ---------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct LedgerEntry {
    pub id: u64,
    pub hash: String,
    /// Unix milliseconds.
    pub at: u64,
    pub body: LedgerBody,
}
