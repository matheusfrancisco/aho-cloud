//! HTTP surface: the internal workspace and sandbox routes (service token), the rpc WebSockets
//! (rpc token) and the terminal WebSocket (single-use terminal token).

use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use axum::extract::rejection::JsonRejection;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use cloud_api::internal::{
    SandboxChatStart, SandboxTerminal, StartWorkspace, WorkspaceFileRef, WorkspacePackage, WorkspaceRef, WorkspaceStarted,
};
use cloud_api::{ApiError, FileContent, FileTree, SandboxChat, TerminalAccess};
use serde::Deserialize;
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;

use crate::bridge::{self, Conn};
use crate::content;
use crate::limits::{Gate, Limits};
use crate::runtime::{RuntimeClient, SandboxLaunch};
use crate::spawn::{CommandTemplate, Launch, LocalLaunch, Spawner};
use crate::terminal::{self, Grant, Grants, TERMINAL_TOKEN_TTL};
use crate::token::{self, Claims, TerminalClaims, TokenKey};
use crate::workspace::{Workspace, WorkspaceError, WorkspaceStore, is_user_id};

/// `GET` with `?token=` → WebSocket speaking aho rpc, one text message per NDJSON line.
pub const RPC_ROUTE: &str = "/rpc/{workspace}";
/// `GET` with `?token=` → the same rpc WebSocket, to the aho inside a sandbox. `{chat}` is the
/// id of the `(sandbox, user)` chat record.
pub const SANDBOX_RPC_ROUTE: &str = "/sandbox-rpc/{chat}";
/// `GET` with `?token=` (single use) → WebSocket carrying binary exec frames to one terminal.
pub const TERMINAL_ROUTE: &str = "/terminal/{sandbox}";

/// Shortest accepted service token, in bytes.
pub const MIN_SERVICE_TOKEN: usize = 16;

/// How much longer than `kill_grace` a newer connection waits for the one it replaces.
const TAKEOVER_MARGIN: Duration = Duration::from_secs(2);

/// The sandbox routes. Without a runtime URL they answer 503 `sandboxes_disabled`.
#[derive(Clone, Debug)]
pub struct SandboxConfig {
    /// The runtime agent's `http://` or `https://` base URL; it is called with the service token.
    pub runtime_url: Option<String>,
    /// Program the sandbox chat runs inside the sandbox (`<command> --project /workspace rpc`).
    pub chat_command: String,
    /// Lifetime of terminal tokens, at most [`TERMINAL_TOKEN_TTL`].
    pub terminal_token_ttl: Duration,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self { runtime_url: None, chat_command: "aho".into(), terminal_token_ttl: TERMINAL_TOKEN_TTL }
    }
}

/// Everything the service needs; the binary fills it from flags and env. `Debug` redacts secrets.
#[derive(Clone)]
pub struct Config {
    /// Workspaces live under `<data_root>/workspaces`. Must be UTF-8 (it is passed as `{dir}`).
    pub data_root: PathBuf,
    /// Public base URL of this service as browsers reach it, `ws://` or `wss://`; `rpc_url` is
    /// `<public_url>/rpc/<workspace>`.
    pub public_url: String,
    /// Preview URL template; `{project}` is replaced by the project slug.
    pub preview_url: String,
    /// Bearer token the control plane presents on internal routes.
    pub service_token: String,
    pub token_key: TokenKey,
    pub token_ttl: Duration,
    pub command: CommandTemplate,
    pub limits: Limits,
    pub sandbox: SandboxConfig,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("data_root", &self.data_root)
            .field("public_url", &self.public_url)
            .field("preview_url", &self.preview_url)
            .field("service_token", &"..")
            .field("token_key", &self.token_key)
            .field("token_ttl", &self.token_ttl)
            .field("command", &self.command)
            .field("limits", &self.limits)
            .field("sandbox", &self.sandbox)
            .finish()
    }
}

/// A configuration the service refuses to start with.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("data root {0} is not valid UTF-8")]
    DataRootNotUtf8(PathBuf),
    #[error("data root {path}: {source}")]
    DataRoot { path: PathBuf, source: io::Error },
    #[error("public url `{0}` must start with ws:// or wss://")]
    PublicUrl(String),
    #[error("service token must be at least {MIN_SERVICE_TOKEN} bytes")]
    ServiceToken,
    #[error("token ttl must be positive")]
    TokenTtl,
    #[error("the session command is empty")]
    Program,
    #[error("`--set {0}` is not KEY=VALUE")]
    Set(String),
    #[error("limit `{0}` must be positive")]
    Limit(&'static str),
    #[error("runtime url `{0}` must start with http:// or https://")]
    RuntimeUrl(String),
    #[error("the sandbox chat command is empty")]
    ChatCommand,
    #[error("terminal token ttl must be between 1 second and {TERMINAL_TOKEN_TTL:?}")]
    TerminalTokenTtl,
}

/// The running service: workspaces, limits, and how to start `ahoc`. Cheap to clone.
#[derive(Clone)]
pub struct SessionHost {
    inner: Arc<Inner>,
}

struct Inner {
    public_url: String,
    preview_url: String,
    service_token: String,
    token_key: TokenKey,
    token_ttl: Duration,
    limits: Limits,
    template: Arc<CommandTemplate>,
    store: WorkspaceStore,
    gate: Arc<Gate>,
    spawner: Arc<dyn Spawner>,
    /// `None` when sandboxes are disabled.
    runtime: Option<Arc<RuntimeClient>>,
    chat_command: String,
    terminal_token_ttl: Duration,
    terminals: Grants,
    chats: WorkspaceStore,
}

impl SessionHost {
    /// Validates `config`; nothing touches the disk until the first workspace starts.
    pub fn new(config: Config, spawner: Arc<dyn Spawner>) -> Result<Self, ConfigError> {
        let data_root =
            std::path::absolute(&config.data_root).map_err(|source| ConfigError::DataRoot { path: config.data_root.clone(), source })?;
        if data_root.to_str().is_none() {
            return Err(ConfigError::DataRootNotUtf8(data_root));
        }
        if !(config.public_url.starts_with("ws://") || config.public_url.starts_with("wss://")) {
            return Err(ConfigError::PublicUrl(config.public_url));
        }
        if config.service_token.len() < MIN_SERVICE_TOKEN {
            return Err(ConfigError::ServiceToken);
        }
        if config.token_ttl.is_zero() {
            return Err(ConfigError::TokenTtl);
        }
        if config.command.program.as_os_str().is_empty() {
            return Err(ConfigError::Program);
        }
        if let Some(bad) = config.command.sets.iter().find(|kv| !kv.split_once('=').is_some_and(|(k, _)| !k.is_empty())) {
            return Err(ConfigError::Set(bad.clone()));
        }
        let l = &config.limits;
        for (name, value) in [
            ("max_connections", l.max_connections),
            ("max_per_workspace", l.max_per_workspace),
            ("max_frame_bytes", l.max_frame_bytes),
            ("max_line_bytes", l.max_line_bytes),
            ("max_tree_entries", l.max_tree_entries),
        ] {
            if value == 0 {
                return Err(ConfigError::Limit(name));
            }
        }
        for (name, value) in [("max_archive_bytes", l.max_archive_bytes), ("max_file_bytes", l.max_file_bytes)] {
            if value == 0 {
                return Err(ConfigError::Limit(name));
            }
        }
        if l.idle_timeout.is_zero() {
            return Err(ConfigError::Limit("idle_timeout"));
        }
        let sandbox = config.sandbox;
        let runtime = match sandbox.runtime_url {
            Some(url) => {
                Some(Arc::new(RuntimeClient::new(&url, config.service_token.clone()).ok_or_else(|| ConfigError::RuntimeUrl(url.clone()))?))
            }
            None => None,
        };
        if sandbox.chat_command.is_empty() {
            return Err(ConfigError::ChatCommand);
        }
        if sandbox.terminal_token_ttl < Duration::from_secs(1) || sandbox.terminal_token_ttl > TERMINAL_TOKEN_TTL {
            return Err(ConfigError::TerminalTokenTtl);
        }
        Ok(Self {
            inner: Arc::new(Inner {
                public_url: config.public_url.trim_end_matches('/').to_owned(),
                preview_url: config.preview_url,
                service_token: config.service_token,
                token_key: config.token_key,
                token_ttl: config.token_ttl,
                gate: Gate::new(&config.limits),
                limits: config.limits,
                template: Arc::new(config.command),
                store: WorkspaceStore::new(&data_root),
                spawner,
                runtime,
                chat_command: sandbox.chat_command,
                terminal_token_ttl: sandbox.terminal_token_ttl,
                terminals: Grants::default(),
                chats: WorkspaceStore::sandbox_chats(&data_root),
            }),
        })
    }

    /// `POST` [`cloud_api::internal::routes::WORKSPACES`], `WORKSPACE_PACKAGE`, `WORKSPACE_FILES`,
    /// `WORKSPACE_FILE`, `SANDBOX_TERMINALS`, `SANDBOX_CHATS`, and `GET` [`RPC_ROUTE`],
    /// [`SANDBOX_RPC_ROUTE`], [`TERMINAL_ROUTE`].
    pub fn router(&self) -> Router {
        use cloud_api::internal::routes;
        Router::new()
            .route(routes::WORKSPACES, post(start_workspace))
            .route(routes::WORKSPACE_PACKAGE, post(workspace_package))
            .route(routes::WORKSPACE_FILES, post(workspace_files))
            .route(routes::WORKSPACE_FILE, post(workspace_file))
            .route(RPC_ROUTE, get(rpc))
            .route(routes::SANDBOX_TERMINALS, post(sandbox_terminal))
            .route(routes::SANDBOX_CHATS, post(sandbox_chat))
            .route(TERMINAL_ROUTE, get(terminal_socket))
            .route(SANDBOX_RPC_ROUTE, get(sandbox_rpc))
            .with_state(self.clone())
    }

    /// Serves until the listener fails or `shutdown` resolves.
    pub async fn serve(self, listener: TcpListener, shutdown: impl Future<Output = ()> + Send + 'static) -> io::Result<()> {
        axum::serve(listener, self.router()).with_graceful_shutdown(shutdown).await
    }
}

/// An [`ApiError`] with its status.
pub(crate) struct HttpError(StatusCode, ApiError);

impl HttpError {
    pub(crate) fn new(status: StatusCode, code: &str, message: impl Into<String>) -> Self {
        Self(status, ApiError::new(code, message))
    }

    fn unauthorized() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "unauthorized", "missing or invalid token")
    }

    fn sandboxes_disabled() -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, "sandboxes_disabled", "this session host has no runtime agent configured")
    }
}

impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        (self.0, Json(self.1)).into_response()
    }
}

impl From<WorkspaceError> for HttpError {
    fn from(e: WorkspaceError) -> Self {
        match e {
            WorkspaceError::InvalidProject(_) | WorkspaceError::InvalidUser | WorkspaceError::InvalidSession(_) => {
                Self::new(StatusCode::BAD_REQUEST, "invalid", e.to_string())
            }
            WorkspaceError::NotFound(_) => Self::new(StatusCode::NOT_FOUND, "not_found", e.to_string()),
            WorkspaceError::Corrupt { .. } | WorkspaceError::Io { .. } => {
                tracing::error!("workspace: {e}");
                Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", "workspace storage failed")
            }
        }
    }
}

fn now_unix() -> Result<u64, HttpError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .map_err(|_| HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", "system clock is before 1970"))
}

/// The control plane's bearer, compared in constant time.
fn authorize(host: &Inner, headers: &HeaderMap) -> Result<(), HttpError> {
    let presented =
        headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).unwrap_or_default();
    if !bool::from(presented.as_bytes().ct_eq(host.service_token.as_bytes())) {
        return Err(HttpError::unauthorized());
    }
    Ok(())
}

fn body<T>(body: Result<Json<T>, JsonRejection>) -> Result<T, HttpError> {
    body.map(|Json(t)| t).map_err(|e| HttpError::new(StatusCode::BAD_REQUEST, "invalid", e.body_text()))
}

async fn start_workspace(
    State(host): State<SessionHost>,
    headers: HeaderMap,
    req: Result<Json<StartWorkspace>, JsonRejection>,
) -> Result<Json<WorkspaceStarted>, HttpError> {
    let host = &host.inner;
    authorize(host, &headers)?;
    let req = body(req)?;
    let ws = host.store.start(&req.project, &req.user_id, req.session_id.as_deref()).await?;
    let claims = Claims { workspace: ws.id.clone(), user: ws.user_id.clone(), exp: now_unix()? + host.token_ttl.as_secs().max(1) };
    let rpc_path = cloud_api::routes::fill(RPC_ROUTE, &[("workspace", &ws.id)]);
    Ok(Json(WorkspaceStarted {
        // Unknown until the first connection's `hello_ok`; see the crate docs.
        session_id: ws.session_id().await,
        rpc_url: format!("{}{rpc_path}", host.public_url),
        token: token::mint(&host.token_key, &claims),
        preview_url: cloud_api::routes::fill(&host.preview_url, &[("project", &ws.project)]),
    }))
}

/// The workspace of `(project, user_id)`, which must have been started; these routes never
/// create one.
async fn existing(host: &Inner, project: &str, user_id: &str) -> Result<Arc<Workspace>, HttpError> {
    host.store
        .find(project, user_id)
        .await?
        .ok_or_else(|| HttpError::new(StatusCode::NOT_FOUND, "not_found", format!("no workspace for this user in project `{project}`")))
}

/// Runs filesystem work off the async runtime.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, HttpError> + Send + 'static) -> Result<T, HttpError> {
    tokio::task::spawn_blocking(f).await.unwrap_or_else(|e| {
        tracing::error!("workspace task: {e}");
        Err(HttpError::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", "the workspace task failed"))
    })
}

async fn workspace_package(
    State(host): State<SessionHost>,
    headers: HeaderMap,
    req: Result<Json<WorkspaceRef>, JsonRejection>,
) -> Result<Json<WorkspacePackage>, HttpError> {
    let host = &host.inner;
    authorize(host, &headers)?;
    let req = body(req)?;
    let ws = existing(host, &req.project, &req.user_id).await?;
    let recorded = ws.session_id().await;
    let max = host.limits.max_archive_bytes;
    let package = blocking(move || content::package(&ws.project_dir, &ws.project, recorded.as_deref(), max)).await?;
    Ok(Json(package))
}

async fn workspace_files(
    State(host): State<SessionHost>,
    headers: HeaderMap,
    req: Result<Json<WorkspaceRef>, JsonRejection>,
) -> Result<Json<FileTree>, HttpError> {
    let host = &host.inner;
    authorize(host, &headers)?;
    let req = body(req)?;
    let ws = existing(host, &req.project, &req.user_id).await?;
    let max = host.limits.max_tree_entries;
    Ok(Json(blocking(move || Ok(cloud_workdir::list_files(&ws.project_dir, max)?)).await?))
}

async fn workspace_file(
    State(host): State<SessionHost>,
    headers: HeaderMap,
    req: Result<Json<WorkspaceFileRef>, JsonRejection>,
) -> Result<Json<FileContent>, HttpError> {
    let host = &host.inner;
    authorize(host, &headers)?;
    let req = body(req)?;
    let ws = existing(host, &req.project, &req.user_id).await?;
    let max = host.limits.max_file_bytes;
    Ok(Json(blocking(move || Ok(cloud_workdir::read_file(&ws.project_dir, &req.path, max)?)).await?))
}

#[derive(Deserialize)]
struct RpcQuery {
    #[serde(default)]
    token: Option<String>,
}

async fn rpc(
    State(host): State<SessionHost>,
    Path(workspace): Path<String>,
    Query(query): Query<RpcQuery>,
    upgrade: WebSocketUpgrade,
) -> Result<Response, HttpError> {
    let inner = &host.inner;
    let presented = query.token.ok_or_else(HttpError::unauthorized)?;
    let claims = token::verify(&inner.token_key, &presented, &workspace, now_unix()?).map_err(|e| {
        tracing::info!(%workspace, "rpc token refused: {e}");
        HttpError::unauthorized()
    })?;
    let ws = inner.store.open(&workspace).await?;
    let launch = LocalLaunch {
        spawner: Arc::clone(&inner.spawner),
        template: Arc::clone(&inner.template),
        dir: ws.project_dir.to_string_lossy().into_owned(),
        label: ws.id.clone(),
    };
    let gate_key = ws.id.clone();
    admit_rpc(inner, ws, claims.user, &gate_key, Box::new(launch), upgrade).await
}

/// The rpc socket for `user` (from a verified token) on the record `ws`, holding the gate slot
/// `gate_key`.
async fn admit_rpc(
    inner: &Inner,
    ws: Arc<Workspace>,
    user: String,
    gate_key: &str,
    launch: Box<dyn Launch>,
    upgrade: WebSocketUpgrade,
) -> Result<Response, HttpError> {
    if user != ws.user_id {
        return Err(HttpError::unauthorized());
    }
    // Oversized messages up to twice the limit still reach the bridge and get a clean 1009;
    // beyond that the transport refuses them outright.
    let transport_max = inner.limits.max_frame_bytes.saturating_mul(2);
    let upgrade = upgrade.max_message_size(transport_max).max_frame_size(transport_max);
    // Newest connection wins: a reloaded page or second tab takes the workspace over. The old
    // connection's child gets `kill_grace` to exit, so wait a little longer than that.
    let response = match inner.gate.acquire(gate_key, inner.limits.kill_grace + TAKEOVER_MARGIN).await {
        Ok(permit) => {
            let conn = Conn { workspace: ws, user, launch, limits: inner.limits, permit };
            upgrade.on_upgrade(move |socket| bridge::run(socket, conn))
        }
        Err(denied) => {
            tracing::info!(workspace = %ws.id, "rpc connection refused: {denied}");
            upgrade.on_upgrade(move |socket| bridge::refuse(socket, denied))
        }
    };
    Ok(response)
}

/// The runtime agent, after the service token: unauthenticated callers learn nothing about
/// the configuration.
fn runtime(inner: &Inner) -> Result<&Arc<RuntimeClient>, HttpError> {
    inner.runtime.as_ref().ok_or_else(HttpError::sandboxes_disabled)
}

fn check_sandbox_and_user(sandbox: &str, user_id: &str) -> Result<(), HttpError> {
    if !cloud_contract::validate::is_project_slug(sandbox) {
        return Err(HttpError::new(StatusCode::BAD_REQUEST, "invalid", format!("`{sandbox}` is not a sandbox slug")));
    }
    if !is_user_id(user_id) {
        return Err(WorkspaceError::InvalidUser.into());
    }
    Ok(())
}

async fn sandbox_terminal(
    State(host): State<SessionHost>,
    headers: HeaderMap,
    req: Result<Json<SandboxTerminal>, JsonRejection>,
) -> Result<Json<TerminalAccess>, HttpError> {
    let inner = &host.inner;
    authorize(inner, &headers)?;
    runtime(inner)?;
    let req = body(req)?;
    check_sandbox_and_user(&req.sandbox, &req.user_id)?;
    if req.exec.argv.first().is_none_or(String::is_empty) {
        return Err(HttpError::new(StatusCode::BAD_REQUEST, "invalid", "exec argv must name a program"));
    }
    let exp = now_unix()? + inner.terminal_token_ttl.as_secs();
    let request = terminal::digest(&req.exec);
    let grant = Grant { sandbox: req.sandbox.clone(), user: req.user_id.clone(), exec: req.exec, exp };
    let nonce = inner.terminals.issue(grant, now_unix()?).ok_or_else(|| {
        HttpError::new(StatusCode::SERVICE_UNAVAILABLE, "busy", "too many terminals are waiting to be opened; try again shortly")
    })?;
    let claims = TerminalClaims { sandbox: req.sandbox, user: req.user_id, nonce, request, exp };
    let path = cloud_api::routes::fill(TERMINAL_ROUTE, &[("sandbox", &claims.sandbox)]);
    Ok(Json(TerminalAccess { url: format!("{}{path}", inner.public_url), token: token::mint_terminal(&inner.token_key, &claims) }))
}

async fn terminal_socket(
    State(host): State<SessionHost>,
    Path(sandbox): Path<String>,
    Query(query): Query<RpcQuery>,
    upgrade: WebSocketUpgrade,
) -> Result<Response, HttpError> {
    let inner = &host.inner;
    let runtime = Arc::clone(runtime(inner)?);
    let presented = query.token.ok_or_else(HttpError::unauthorized)?;
    let claims = token::verify_terminal(&inner.token_key, &presented, &sandbox, now_unix()?).map_err(|e| {
        tracing::info!(%sandbox, "terminal token refused: {e}");
        HttpError::unauthorized()
    })?;
    let grant = inner.terminals.redeem(&claims).ok_or_else(|| {
        tracing::info!(%sandbox, user = %claims.user, "terminal token refused: already used");
        HttpError::unauthorized()
    })?;
    let upgrade = upgrade.max_message_size(terminal::TRANSPORT_MAX).max_frame_size(terminal::TRANSPORT_MAX);
    // Terminals never replace each other: each holds its own slot, so only the host limit applies.
    let response = match inner.gate.try_acquire(&format!("terminal/{}", claims.nonce)) {
        Ok(permit) => {
            let limits = inner.limits;
            upgrade.on_upgrade(move |socket| terminal::relay(socket, runtime, grant, limits, permit))
        }
        Err(denied) => {
            tracing::info!(%sandbox, "terminal refused: {denied}");
            upgrade.on_upgrade(move |socket| terminal::refuse(socket, denied))
        }
    };
    Ok(response)
}

async fn sandbox_chat(
    State(host): State<SessionHost>,
    headers: HeaderMap,
    req: Result<Json<SandboxChatStart>, JsonRejection>,
) -> Result<Json<SandboxChat>, HttpError> {
    let inner = &host.inner;
    authorize(inner, &headers)?;
    runtime(inner)?;
    let req = body(req)?;
    check_sandbox_and_user(&req.sandbox, &req.user_id)?;
    let chat = inner.chats.start(&req.sandbox, &req.user_id, req.session_id.as_deref()).await?;
    let claims = Claims { workspace: chat.id.clone(), user: chat.user_id.clone(), exp: now_unix()? + inner.token_ttl.as_secs().max(1) };
    let path = cloud_api::routes::fill(SANDBOX_RPC_ROUTE, &[("chat", &chat.id)]);
    Ok(Json(SandboxChat {
        rpc_url: format!("{}{path}", inner.public_url),
        token: token::mint_chat(&inner.token_key, &claims),
        session_id: chat.session_id().await,
    }))
}

async fn sandbox_rpc(
    State(host): State<SessionHost>,
    Path(chat): Path<String>,
    Query(query): Query<RpcQuery>,
    upgrade: WebSocketUpgrade,
) -> Result<Response, HttpError> {
    let inner = &host.inner;
    let runtime = Arc::clone(runtime(inner)?);
    let presented = query.token.ok_or_else(HttpError::unauthorized)?;
    let claims = token::verify_chat(&inner.token_key, &presented, &chat, now_unix()?).map_err(|e| {
        tracing::info!(%chat, "sandbox rpc token refused: {e}");
        HttpError::unauthorized()
    })?;
    let record = inner.chats.open(&chat).await?;
    let launch = SandboxLaunch { runtime, sandbox: record.project.clone(), command: inner.chat_command.clone() };
    let gate_key = format!("sandbox-chat/{}", record.id);
    admit_rpc(inner, record, claims.user, &gate_key, Box::new(launch), upgrade).await
}
