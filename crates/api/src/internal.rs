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
