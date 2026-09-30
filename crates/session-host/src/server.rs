//! HTTP surface: the internal workspace routes (service token) and the rpc WebSocket (rpc token).

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
use cloud_api::internal::{StartWorkspace, WorkspaceFileRef, WorkspacePackage, WorkspaceRef, WorkspaceStarted};
use cloud_api::{ApiError, FileContent, FileTree};
use serde::Deserialize;
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;

use crate::bridge::{self, Conn};
use crate::content;
use crate::limits::{Gate, Limits};
use crate::spawn::{CommandTemplate, Spawner};
use crate::token::{self, Claims, TokenKey};
use crate::workspace::{Workspace, WorkspaceError, WorkspaceStore};

/// `GET` with `?token=` → WebSocket speaking aho rpc, one text message per NDJSON line.
pub const RPC_ROUTE: &str = "/rpc/{workspace}";

/// Shortest accepted service token, in bytes.
pub const MIN_SERVICE_TOKEN: usize = 16;

/// How much longer than `kill_grace` a newer connection waits for the one it replaces.
const TAKEOVER_MARGIN: Duration = Duration::from_secs(2);

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
            }),
        })
    }

    /// `POST` [`cloud_api::internal::routes::WORKSPACES`], `WORKSPACE_PACKAGE`, `WORKSPACE_FILES`,
    /// `WORKSPACE_FILE`, and `GET` [`RPC_ROUTE`].
    pub fn router(&self) -> Router {
        use cloud_api::internal::routes;
        Router::new()
            .route(routes::WORKSPACES, post(start_workspace))
            .route(routes::WORKSPACE_PACKAGE, post(workspace_package))
            .route(routes::WORKSPACE_FILES, post(workspace_files))
            .route(routes::WORKSPACE_FILE, post(workspace_file))
            .route(RPC_ROUTE, get(rpc))
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
    if claims.user != ws.user_id {
        return Err(HttpError::unauthorized());
    }
    // Oversized messages up to twice the limit still reach the bridge and get a clean 1009;
    // beyond that the transport refuses them outright.
    let transport_max = inner.limits.max_frame_bytes.saturating_mul(2);
    let upgrade = upgrade.max_message_size(transport_max).max_frame_size(transport_max);
    // Newest connection wins: a reloaded page or second tab takes the workspace over. The old
    // connection's child gets `kill_grace` to exit, so wait a little longer than that.
    let response = match inner.gate.acquire(&ws.id, inner.limits.kill_grace + TAKEOVER_MARGIN).await {
        Ok(permit) => {
            let conn = Conn {
                workspace: ws,
                user: claims.user,
                spawner: Arc::clone(&inner.spawner),
                template: Arc::clone(&inner.template),
                limits: inner.limits,
                permit,
            };
            upgrade.on_upgrade(move |socket| bridge::run(socket, conn))
        }
        Err(denied) => {
            tracing::info!(%workspace, "rpc connection refused: {denied}");
            upgrade.on_upgrade(move |socket| bridge::refuse(socket, denied))
        }
    };
    Ok(response)
}
