//! Service-to-service bodies: control-plane ↔ builder, runtime-agent, edge, session-host. Never
//! exposed to customers. Authenticated with a shared service token (`Authorization: Bearer`).

use std::collections::BTreeMap;

use cloud_contract::edge::EdgeConfig;
use cloud_contract::ledger::Stage;
use cloud_contract::manifest::{Auth, Build, Manifest, Source};
use serde::{Deserialize, Serialize};

pub mod routes {
    /// builder: `POST` a [`super::BuildJob`] → [`super::BuildResult`] (synchronous; long timeout).
    pub const BUILD: &str = "/internal/v1/builds";
    /// runtime-agent: `PUT` a [`super::Deployment`] → [`super::DeploymentStatus`].
    pub const DEPLOYMENT: &str = "/internal/v1/deployments/{p}/{stage}";
    pub const DEPLOYMENT_RESTART: &str = "/internal/v1/deployments/{p}/{stage}/restart";
    /// `POST` a [`super::Scale`].
    pub const DEPLOYMENT_SCALE: &str = "/internal/v1/deployments/{p}/{stage}/scale";
    /// `POST` a [`super::Maintenance`].
    pub const DEPLOYMENT_MAINTENANCE: &str = "/internal/v1/deployments/{p}/{stage}/maintenance";
    /// `POST` → stops every process (budget degrade = sleep).
    pub const DEPLOYMENT_SLEEP: &str = "/internal/v1/deployments/{p}/{stage}/sleep";
    /// control-plane: `GET` → [`super::Sites`]; the edge polls it (`?since=<version>`).
    pub const SITES: &str = "/internal/v1/sites";
    /// control-plane: `POST` a [`super::Observation`] (from the edge/runtime during staging).
    pub const OBSERVATIONS: &str = "/internal/v1/observations";
    /// session-host: `POST` a [`super::StartWorkspace`] → [`super::WorkspaceStarted`].
    pub const WORKSPACES: &str = "/internal/v1/workspaces";
    /// session-host: `POST` a [`super::WorkspaceRef`] → [`super::WorkspacePackage`].
    pub const WORKSPACE_PACKAGE: &str = "/internal/v1/workspaces/package";
    /// session-host: `POST` a [`super::WorkspaceRef`] → [`crate::FileTree`].
    pub const WORKSPACE_FILES: &str = "/internal/v1/workspaces/files";
    /// session-host: `POST` a [`super::WorkspaceFileRef`] → [`crate::FileContent`].
    pub const WORKSPACE_FILE: &str = "/internal/v1/workspaces/file";
    /// runtime-agent: `PUT` a [`super::SandboxSpec`] → [`super::SandboxStatus`] (want it running
    /// with this spec; a changed spec replaces the VM, the disk is kept); `GET` → status;
    /// `DELETE` → 204 (VM stopped, disk destroyed).
    pub const SANDBOX: &str = "/internal/v1/sandboxes/{s}";
    /// runtime-agent: `POST` → [`super::SandboxStatus`]: stop the VM, keep the disk.
    pub const SANDBOX_STOP: &str = "/internal/v1/sandboxes/{s}/stop";
    /// runtime-agent: `GET` WebSocket. The first message is a text [`crate::exec::ExecRequest`];
    /// then [`crate::exec::Frame`]s as binary messages both ways (see [`crate::exec`]).
    pub const SANDBOX_EXEC: &str = "/internal/v1/sandboxes/{s}/exec";
    /// runtime-agent: `GET` → [`crate::FileTree`] of [`super::SANDBOX_WORKSPACE`].
    pub const SANDBOX_FILES: &str = "/internal/v1/sandboxes/{s}/files";
    /// runtime-agent: `GET ?path=` (relative to the workspace) → [`crate::FileContent`].
    pub const SANDBOX_FILE: &str = "/internal/v1/sandboxes/{s}/file";
    /// session-host: `POST` a [`super::SandboxTerminal`] → [`crate::TerminalAccess`].
    pub const SANDBOX_TERMINALS: &str = "/internal/v1/sandbox-terminals";
    /// session-host: `POST` a [`super::SandboxChatStart`] → [`crate::SandboxChat`].
    pub const SANDBOX_CHATS: &str = "/internal/v1/sandbox-chats";
}

/// The persistent disk's mount point inside every sandbox, and the aho project there.
pub const SANDBOX_WORKSPACE: &str = "/workspace";
/// The registry repository (relative to the runtime's image-repo template) of sandbox images;
/// it has a `.`, so no project slug can name it.
pub const SANDBOX_IMAGE_REPO: &str = "aho.sandbox";

/// Everything the runtime agent needs to run one sandbox.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxSpec {
    pub slug: String,
    /// Substituted for `{project}` in the runtime's image-repo template.
    pub image_repo: String,
    /// `sha256:…`.
    pub image_digest: String,
    /// Added to the image environment of the VM and of every exec session (decrypted user vars).
    pub env: BTreeMap<String, String>,
    /// Hosts the VM may reach, enforced by the egress proxy (not by the Docker dev backend).
    pub egress: Vec<String>,
    /// Published to the edge (reported in [`SandboxStatus::address`]).
    #[serde(default)]
    pub port: Option<u16>,
    pub cpus: u8,
    pub mem_mib: u32,
    pub disk_mib: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxStatus {
    pub slug: String,
    pub state: crate::SandboxState,
    /// `ip:port` of [`SandboxSpec::port`] while running.
    #[serde(default)]
    pub address: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

/// Open a terminal for `user_id` in `sandbox`: the session host mints a short-lived, single-use
/// token bound to exactly this request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxTerminal {
    pub sandbox: String,
    pub user_id: String,
    pub exec: crate::exec::ExecRequest,
}

/// aho rpc to the aho inside `sandbox`, for `user_id` (resumes `session_id`, else the one this
/// user last had there).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxChatStart {
    pub sandbox: String,
    pub user_id: String,
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildJob {
    pub project: String,
    pub release: String,
    pub source: Source,
    pub build: Build,
    /// For `source.kind = archive`: where the builder fetches the bytes (control-plane blob URL).
    #[serde(default)]
    pub archive_url: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum BuildResult {
    Built { image_digest: String, log: String },
    Failed { reason: String, log: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Deployment {
    pub project: String,
    pub stage: Stage,
    pub release: String,
    pub image_digest: String,
    pub manifest: Manifest,
    /// Secret values by name, for this stage. Placeholders (`aho-cloud-placeholder:<NAME>`) in
    /// preview and staging.
    pub secrets: BTreeMap<String, String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Starting,
    Running,
    Degraded,
    Maintenance,
    Sleeping,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessStatus {
    pub name: String,
    pub replicas: u16,
    pub healthy: u16,
    /// `ip:port` of each healthy replica with a port.
    pub addresses: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentStatus {
    pub project: String,
    pub stage: Stage,
    pub release: String,
    pub state: RunState,
    pub processes: Vec<ProcessStatus>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Scale {
    pub process: String,
    pub replicas: u16,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Maintenance {
    pub on: bool,
}

/// Everything the edge needs to serve one hostname.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Site {
    pub hostname: String,
    pub project: String,
    pub stage: Stage,
    pub state: SiteState,
    /// `ip:port` of the public process's healthy replicas.
    pub upstreams: Vec<String>,
    pub edge: EdgeConfig,
    pub auth: Auth,
    /// Argon2id PHC string, for `auth.mode = password`.
    #[serde(default)]
    pub password_hash: Option<String>,
    /// For OIDC: client secret value (the client id is in `auth` for custom providers or in
    /// the edge's own config for Google/GitHub/Microsoft).
    #[serde(default)]
    pub oidc_client_secret: Option<String>,
    /// Webhook verifier secrets named in `edge.toml`, by name.
    #[serde(default)]
    pub verifier_secrets: BTreeMap<String, String>,
    /// Blake3 hex of each valid project service token (`bearer` routes).
    #[serde(default)]
    pub service_token_hashes: Vec<String>,
    /// Emails allowed through `edge`-mode routes in preview/staging (reviewers); empty = anyone
    /// who authenticates.
    #[serde(default)]
    pub allowed_emails: Vec<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SiteState {
    Active,
    Maintenance,
    Sleeping,
    /// Project deleted or domain detached: serve the tombstone page, never another tenant.
    Tombstoned,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sites {
    /// Monotonic; the edge passes it back as `since`.
    pub version: u64,
    pub sites: Vec<Site>,
}

/// What a staging run actually did, for the approval card's observed surface.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    pub project: String,
    pub release: String,
    pub listeners: Vec<u16>,
    pub egress: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StartWorkspace {
    pub project: String,
    pub user_id: String,
    /// Existing aho session to resume, if any.
    #[serde(default)]
    pub session_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceStarted {
    /// The aho session the workspace resumes; `None` until its first connection's `hello_ok`.
    #[serde(default)]
    pub session_id: Option<String>,
    /// `wss://…` the browser connects to.
    pub rpc_url: String,
    /// Workspace-scoped token (see session-host `token` module); short-lived.
    pub token: String,
    pub preview_url: String,
}

/// The workspace of `user_id` in `project` (the session host derives its id from the pair).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceRef {
    pub project: String,
    pub user_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceFileRef {
    pub project: String,
    pub user_id: String,
    /// `/`-separated, relative to the workspace root.
    pub path: String,
}

/// Everything a deploy of one workspace needs, packed by the session host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspacePackage {
    /// Deterministic `.tar.gz` of the workspace files, base64 (standard, padded).
    pub archive: String,
    /// `blake3:<hex>` of the archive bytes.
    pub archive_hash: String,
    pub files: u64,
    /// The newest aho session log in the workspace, chain-verified by the session host.
    #[serde(default)]
    pub session: Option<PackagedSession>,
    /// The workspace's `deploy.json`, when present and parseable.
    #[serde(default)]
    pub manifest: Option<Manifest>,
    /// Why `deploy.json` exists but could not be used.
    #[serde(default)]
    pub manifest_error: Option<String>,
    /// Drafted from `deploy.json`, the files and the session; `source` points at the archive.
    pub draft: cloud_contract::draft::Draft,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackagedSession {
    pub session_id: String,
    pub head_hash: String,
    pub entries: u64,
    /// The JSONL log, base64 (standard, padded).
    pub jsonl: String,
}
