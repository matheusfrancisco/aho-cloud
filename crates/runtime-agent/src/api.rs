//! The internal HTTP API the control plane and the session host drive
//! (`cloud_api::internal::routes::DEPLOYMENT*` and `SANDBOX*`). Every request needs
//! `Authorization: Bearer <service token>`.
//!
//! [`routes::SANDBOX_EXEC`] is a WebSocket bridged to one exec session in the sandbox (see
//! [`cloud_api::exec`]): the first message is a text [`ExecRequest`], then binary frames both
//! ways; the socket closes after `Exit` or `Error`, and closing it ends the session. The file
//! routes run `cloud-guest files|file` in the sandbox as exec sessions and answer their JSON.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::rejection::JsonRejection;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use cloud_api::exec::{ExecRequest, Frame, MAX_FRAME};
use cloud_api::internal::{Deployment, DeploymentStatus, Maintenance, SANDBOX_WORKSPACE, SandboxSpec, SandboxStatus, Scale, routes};
use cloud_api::{ApiError, FileContent, FileTree};
use cloud_contract::ledger::Stage;
use cloud_contract::validate::{is_hostname, is_project_slug};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde::de::DeserializeOwned;

use crate::guest::exec::{self as session, SessionError, frame_name, read_frame, write_frame};
use crate::guest::{EXIT_BAD_PATH, EXIT_NOT_FOUND, is_env_name};
use crate::image::digest_hex;
use crate::supervisor::{Supervisor, SupervisorError};
use crate::vmm::Vmm;

/// Longest a sandbox file listing or read may take.
const FILES_TIMEOUT: Duration = Duration::from_secs(30);
/// Most stdout a file listing or read may print (a 1 MiB text file, JSON-escaped).
const FILES_MAX_OUTPUT: usize = 8 << 20;
/// How long an exec socket may wait for its request.
const EXEC_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

struct Api<V> {
    supervisor: Supervisor<V>,
    /// blake3 of the service token; `blake3::Hash` equality is constant-time.
    token: Option<blake3::Hash>,
}

type Shared<V> = Arc<Api<V>>;

/// A JSON [`ApiError`] with its status.
struct Error(StatusCode, ApiError);

impl Error {
    fn new(status: StatusCode, code: &str, message: impl Into<String>) -> Self {
        Error(status, ApiError::new(code, message))
    }
}

impl IntoResponse for Error {
    fn into_response(self) -> Response {
        (self.0, Json(self.1)).into_response()
    }
}

impl From<SupervisorError> for Error {
    fn from(e: SupervisorError) -> Self {
        match e {
            SupervisorError::NotFound(..) | SupervisorError::UnknownProcess(_) | SupervisorError::NoSandbox(_) => {
                Error::new(StatusCode::NOT_FOUND, "not_found", e.to_string())
            }
            SupervisorError::TooManyReplicas { .. } | SupervisorError::Invalid(_) => {
                Error::new(StatusCode::BAD_REQUEST, "invalid", e.to_string())
            }
            SupervisorError::NotRunning(_) => Error::new(StatusCode::CONFLICT, "not_running", e.to_string()),
            SupervisorError::Backend(_) => Error::new(StatusCode::BAD_GATEWAY, "guest_failed", e.to_string()),
        }
    }
}

impl From<JsonRejection> for Error {
    fn from(e: JsonRejection) -> Self {
        Error::new(StatusCode::BAD_REQUEST, "invalid", e.body_text())
    }
}

/// The router, authenticated with `service_token` (an empty token admits nobody).
pub fn router<V: Vmm>(supervisor: Supervisor<V>, service_token: &str) -> Router {
    let token = (!service_token.is_empty()).then(|| blake3::hash(service_token.as_bytes()));
    let api: Shared<V> = Arc::new(Api { supervisor, token });
    Router::new()
        .route(routes::DEPLOYMENT, put(put_deployment::<V>).get(get_deployment::<V>).delete(delete_deployment::<V>))
        .route(routes::DEPLOYMENT_RESTART, post(restart::<V>))
        .route(routes::DEPLOYMENT_SCALE, post(scale::<V>))
        .route(routes::DEPLOYMENT_MAINTENANCE, post(maintenance::<V>))
        .route(routes::DEPLOYMENT_SLEEP, post(sleep::<V>))
        .route(routes::SANDBOX, put(put_sandbox::<V>).get(get_sandbox::<V>).delete(delete_sandbox::<V>))
        .route(routes::SANDBOX_STOP, post(stop_sandbox::<V>))
        .route(routes::SANDBOX_FILES, get(sandbox_files::<V>))
        .route(routes::SANDBOX_FILE, get(sandbox_file::<V>))
        .route(routes::SANDBOX_EXEC, get(sandbox_exec::<V>))
        .route_layer(middleware::from_fn_with_state(api.clone(), authenticate::<V>))
        .with_state(api)
}

async fn authenticate<V: Vmm>(State(api): State<Shared<V>>, request: Request, next: Next) -> Response {
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .filter(|t| !t.is_empty());
    let ok = match (presented, api.token) {
        (Some(t), Some(expected)) => blake3::hash(t.as_bytes()) == expected,
        _ => false,
    };
    if !ok {
        return Error::new(StatusCode::UNAUTHORIZED, "unauthorized", "missing or invalid service token").into_response();
    }
    next.run(request).await
}

fn stage(s: &str) -> Result<Stage, Error> {
    serde_json::from_value(serde_json::Value::String(s.to_owned()))
        .map_err(|_| Error::new(StatusCode::NOT_FOUND, "not_found", format!("no stage `{s}`")))
}

type Reply = Result<Json<DeploymentStatus>, Error>;

async fn put_deployment<V: Vmm>(
    State(api): State<Shared<V>>,
    Path((project, stage_name)): Path<(String, String)>,
    body: Result<Json<Deployment>, JsonRejection>,
) -> Reply {
    let stage = stage(&stage_name)?;
    let Json(d) = body?;
    if d.project != project || d.stage != stage {
        return Err(Error::new(
            StatusCode::BAD_REQUEST,
            "invalid",
            format!("body is for {}/{}, path is {project}/{stage_name}", d.project, d.stage.as_str()),
        ));
    }
    if let Err(refusals) = cloud_contract::validate(&d.manifest) {
        let mut error = ApiError::new("refused", "the manifest does not pass the gate");
        error.refusals = refusals;
        return Err(Error(StatusCode::BAD_REQUEST, error));
    }
    digest_hex(&d.image_digest).map_err(|e| Error::new(StatusCode::BAD_REQUEST, "invalid", e.to_string()))?;
    Ok(Json(api.supervisor.apply(d)?))
}

async fn get_deployment<V: Vmm>(State(api): State<Shared<V>>, Path((project, stage_name)): Path<(String, String)>) -> Reply {
    Ok(Json(api.supervisor.status(&project, stage(&stage_name)?)?))
}

async fn delete_deployment<V: Vmm>(
    State(api): State<Shared<V>>,
    Path((project, stage_name)): Path<(String, String)>,
) -> Result<StatusCode, Error> {
    api.supervisor.remove(&project, stage(&stage_name)?)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn restart<V: Vmm>(State(api): State<Shared<V>>, Path((project, stage_name)): Path<(String, String)>) -> Reply {
    Ok(Json(api.supervisor.restart(&project, stage(&stage_name)?)?))
}

async fn scale<V: Vmm>(
    State(api): State<Shared<V>>,
    Path((project, stage_name)): Path<(String, String)>,
    body: Result<Json<Scale>, JsonRejection>,
) -> Reply {
    let stage = stage(&stage_name)?;
    let Json(s) = body?;
    Ok(Json(api.supervisor.scale(&project, stage, &s.process, s.replicas)?))
}

async fn maintenance<V: Vmm>(
    State(api): State<Shared<V>>,
    Path((project, stage_name)): Path<(String, String)>,
    body: Result<Json<Maintenance>, JsonRejection>,
) -> Reply {
    let stage = stage(&stage_name)?;
    let Json(m) = body?;
    Ok(Json(api.supervisor.maintenance(&project, stage, m.on)?))
}

async fn sleep<V: Vmm>(State(api): State<Shared<V>>, Path((project, stage_name)): Path<(String, String)>) -> Reply {
    Ok(Json(api.supervisor.sleep(&project, stage(&stage_name)?)?))
}

/// A sandbox slug from the path; anything else cannot name a sandbox.
fn slug(s: &str) -> Result<&str, Error> {
    if is_project_slug(s) { Ok(s) } else { Err(Error::new(StatusCode::NOT_FOUND, "not_found", format!("no sandbox `{s}`"))) }
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(StatusCode::BAD_REQUEST, "invalid", message)
}

/// What the runtime refuses to run: a body for another slug, an unpinned image, a repo that is
/// not a relative repository path, unusable egress entries, environment or sizes.
fn check_sandbox(path_slug: &str, s: &SandboxSpec) -> Result<(), Error> {
    if s.slug != path_slug {
        return Err(invalid(format!("body is for sandbox `{}`, path is `{path_slug}`", s.slug)));
    }
    digest_hex(&s.image_digest).map_err(|e| invalid(e.to_string()))?;
    let repo_ok = !s.image_repo.is_empty()
        && s.image_repo.split('/').all(|part| {
            !part.is_empty()
                && !part.starts_with('.')
                && part.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(&b))
        });
    if !repo_ok {
        return Err(invalid(format!("`{}` is not an image repository", s.image_repo)));
    }
    if let Some(bad) = s.egress.iter().find(|h| !is_hostname(h, true)) {
        return Err(invalid(format!("egress entry `{bad}` is not a hostname or `*.` wildcard")));
    }
    if let Some((key, _)) = s.env.iter().find(|(k, v)| !is_env_name(k) || v.contains('\0')) {
        return Err(invalid(format!("environment variable `{key}` cannot be set")));
    }
    if s.cpus == 0 || s.mem_mib == 0 || s.disk_mib == 0 || s.port == Some(0) {
        return Err(invalid("cpus, mem_mib, disk_mib and port must not be zero"));
    }
    Ok(())
}

type SandboxReply = Result<Json<SandboxStatus>, Error>;

async fn put_sandbox<V: Vmm>(
    State(api): State<Shared<V>>,
    Path(s): Path<String>,
    body: Result<Json<SandboxSpec>, JsonRejection>,
) -> SandboxReply {
    let Json(spec) = body?;
    if !is_project_slug(&s) {
        return Err(invalid(format!("`{s}` is not a sandbox slug")));
    }
    check_sandbox(&s, &spec)?;
    Ok(Json(api.supervisor.put_sandbox(spec)?))
}

async fn get_sandbox<V: Vmm>(State(api): State<Shared<V>>, Path(s): Path<String>) -> SandboxReply {
    Ok(Json(api.supervisor.sandbox_status(slug(&s)?)?))
}

async fn delete_sandbox<V: Vmm>(State(api): State<Shared<V>>, Path(s): Path<String>) -> Result<StatusCode, Error> {
    api.supervisor.remove_sandbox(slug(&s)?)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn stop_sandbox<V: Vmm>(State(api): State<Shared<V>>, Path(s): Path<String>) -> SandboxReply {
    Ok(Json(api.supervisor.stop_sandbox(slug(&s)?)?))
}

/// Runs `cloud-guest <args>` in the sandbox and parses what it printed.
async fn guest_json<V: Vmm, T: DeserializeOwned>(api: &Api<V>, slug: &str, args: &[&str]) -> Result<T, Error> {
    let failed = |message: String| Error::new(StatusCode::BAD_GATEWAY, "guest_failed", message);
    let conn = api.supervisor.sandbox_exec(slug).await?;
    let mut argv = vec![api.supervisor.guest_program().to_owned()];
    argv.extend(args.iter().map(|a| (*a).to_owned()));
    let request = ExecRequest { argv, pty: None, env: Default::default(), cwd: None };
    let out = tokio::time::timeout(FILES_TIMEOUT, session::collect(conn, request, FILES_MAX_OUTPUT))
        .await
        .map_err(|_| failed(format!("the guest did not answer within {} s", FILES_TIMEOUT.as_secs())))?
        .map_err(|e: SessionError| failed(e.to_string()))?;
    let message = || String::from_utf8_lossy(&out.stderr).trim().to_owned();
    match out.code {
        0 => serde_json::from_slice(&out.stdout).map_err(|e| failed(format!("unreadable guest answer: {e}"))),
        EXIT_BAD_PATH => Err(invalid(message())),
        EXIT_NOT_FOUND => Err(Error::new(StatusCode::NOT_FOUND, "not_found", message())),
        code => Err(failed(format!("cloud-guest exited with {code}: {}", message()))),
    }
}

async fn sandbox_files<V: Vmm>(State(api): State<Shared<V>>, Path(s): Path<String>) -> Result<Json<FileTree>, Error> {
    Ok(Json(guest_json(&api, slug(&s)?, &["files", "--root", SANDBOX_WORKSPACE]).await?))
}

#[derive(Deserialize)]
struct FileQuery {
    path: String,
}

async fn sandbox_file<V: Vmm>(
    State(api): State<Shared<V>>,
    Path(s): Path<String>,
    query: Result<Query<FileQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<FileContent>, Error> {
    let Query(q) = query.map_err(|e| invalid(e.body_text()))?;
    // `--path=…` keeps a path that looks like a flag a value.
    let path = format!("--path={}", q.path);
    Ok(Json(guest_json(&api, slug(&s)?, &["file", "--root", SANDBOX_WORKSPACE, &path]).await?))
}

/// Refuses before upgrading when the sandbox is unknown (404) or not running (409).
async fn sandbox_exec<V: Vmm>(State(api): State<Shared<V>>, Path(s): Path<String>, ws: WebSocketUpgrade) -> Result<Response, Error> {
    let s = slug(&s)?.to_owned();
    let status = api.supervisor.sandbox_status(&s)?;
    if status.state != cloud_api::SandboxState::Running {
        return Err(SupervisorError::NotRunning(s).into());
    }
    Ok(ws.max_message_size(MAX_FRAME).max_frame_size(MAX_FRAME).on_upgrade(move |socket| bridge(api, s, socket)))
}

/// Sends an `Error` frame and closes.
async fn fail(socket: &mut WebSocket, message: String) {
    let _ = socket.send(Message::Binary(Frame::Error(message).encode().into())).await;
    let _ = socket.send(Message::Close(None)).await;
}

async fn bridge<V: Vmm>(api: Shared<V>, slug: String, mut socket: WebSocket) {
    let request = match tokio::time::timeout(EXEC_REQUEST_TIMEOUT, socket.recv()).await {
        Ok(Some(Ok(Message::Text(text)))) => match serde_json::from_str::<ExecRequest>(text.as_str()) {
            Ok(r) => r,
            Err(e) => return fail(&mut socket, format!("bad exec request: {e}")).await,
        },
        Ok(Some(Ok(Message::Close(_))) | None | Some(Err(_))) => return,
        Ok(Some(Ok(_))) => return fail(&mut socket, "the first message must be a text exec request".into()).await,
        Err(_) => return fail(&mut socket, "no exec request".into()).await,
    };
    let conn = match api.supervisor.sandbox_exec(&slug).await {
        Ok(conn) => conn,
        Err(e) => return fail(&mut socket, e.to_string()).await,
    };
    let (mut guest_rx, mut guest_tx) = tokio::io::split(conn);
    if let Err(e) = write_frame(&mut guest_tx, &Frame::Request(request)).await {
        return fail(&mut socket, format!("guest: {e}")).await;
    }
    let (mut client_tx, mut client_rx) = socket.split();
    // Guest → client until `Exit`/`Error` (sent, then the socket closes) or the guest is gone.
    let down = async {
        loop {
            let frame = match read_frame(&mut guest_rx).await {
                Ok(Some(frame)) => frame,
                Ok(None) => Frame::Error("the guest ended the session".into()),
                Err(e) => Frame::Error(format!("guest: {e}")),
            };
            let last = matches!(frame, Frame::Exit(_) | Frame::Error(_));
            if client_tx.send(Message::Binary(frame.encode().into())).await.is_err() || last {
                return None;
            }
        }
    };
    // Client → guest: only what a client may send. `Some` is why the session is refused.
    let up = async {
        while let Some(message) = client_rx.next().await {
            let bytes = match message {
                Ok(Message::Binary(bytes)) => bytes,
                Ok(Message::Ping(_) | Message::Pong(_)) => continue,
                Ok(Message::Close(_)) => return None,
                // Oversized messages land here; a vanished client never reads the answer.
                Err(e) => return Some(e.to_string()),
                Ok(Message::Text(_)) => return Some("only binary frames follow the request".to_owned()),
            };
            let frame = match Frame::decode(&bytes) {
                Ok(frame @ (Frame::Stdin(_) | Frame::Resize(_) | Frame::CloseStdin)) => frame,
                Ok(other) => return Some(format!("a client may not send {} frames", frame_name(&other))),
                Err(e) => return Some(e.to_string()),
            };
            if write_frame(&mut guest_tx, &frame).await.is_err() {
                return None;
            }
        }
        None
    };
    let refused = tokio::select! {
        r = down => r,
        r = up => r,
    };
    if let Some(message) = refused {
        let _ = client_tx.send(Message::Binary(Frame::Error(message).encode().into())).await;
    }
    let _ = client_tx.send(Message::Close(None)).await;
    // Dropping the guest stream ends the session: the guest hangs its process group up.
}
