//! Sandboxes over HTTP: list and create in an org, then, for any member of the sandbox's org:
//! view, start, stop, delete, settings, the write-only environment, terminal, chat and files.

use std::collections::{BTreeMap, HashMap};

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use cloud_api::exec::{ExecRequest, PtySize};
use cloud_api::internal::{SANDBOX_WORKSPACE, SandboxChatStart, SandboxTerminal};
use cloud_api::{
    CreateSandbox, EnvVarView, FileContent, FileTree, Sandbox, SandboxChat, SandboxSettings, SandboxState, SetEnvVar, StartSandboxChat,
    TerminalAccess, TerminalRequest,
};
use cloud_contract::validate::{PROJECT_SLUG_MAX, is_project_slug};

use super::ApiJson;
use super::projects::valid_session_id;
use super::secrets::{MAX_VALUE_BYTES, valid_name};
use super::workspace::valid_path;
use crate::auth::{Caller, org_access, sandbox_access};
use crate::error::AppError;
use crate::ports::PortError;
use crate::sandboxes::{self, SlugGuard};
use crate::state::AppState;
use crate::store::{EnvVarRec, SandboxRec, UserRec};

/// Terminal sizes are clamped to `1..=` these.
const MAX_COLS: u16 = 1000;
const MAX_ROWS: u16 = 500;
/// Bounds of a terminal's `command`.
const MAX_ARGS: usize = 256;
const MAX_ARG_BYTES: usize = 4096;

/// The sandbox, re-read under its lock, if `user` belongs to its org.
async fn locked<'a>(st: &'a AppState, user: &UserRec, slug: &str) -> Result<(SandboxRec, SlugGuard<'a>), AppError> {
    let (rec, _) = sandbox_access(st, user, slug).await?;
    let turn = st.sandbox_locks.lock(&rec.slug).await;
    let rec = st.store.sandbox(&rec.slug).await?.ok_or_else(|| AppError::not_found(format!("sandbox {slug}")))?;
    Ok((rec, turn))
}

/// The sandbox with the runtime's live state, if `user` belongs to its org.
async fn live(st: &AppState, user: &UserRec, slug: &str) -> Result<SandboxRec, AppError> {
    let (mut rec, _) = sandbox_access(st, user, slug).await?;
    sandboxes::live(st, &mut rec).await;
    Ok(rec)
}

fn require_running(rec: &SandboxRec) -> Result<(), AppError> {
    match rec.state {
        SandboxState::Running => Ok(()),
        SandboxState::Starting => Err(AppError::conflict(format!("sandbox {} is still starting", rec.slug))),
        SandboxState::Stopped => Err(AppError::conflict(format!("sandbox {} is stopped; start it first", rec.slug))),
        SandboxState::Failed => Err(AppError::conflict(format!(
            "sandbox {} failed ({}); start it again",
            rec.slug,
            rec.error.as_deref().unwrap_or("no reason given")
        ))),
    }
}

pub async fn list(State(st): State<AppState>, Caller(user): Caller, Path(org): Path<String>) -> Result<Json<Vec<Sandbox>>, AppError> {
    let (org, _) = org_access(&st, &user, &org).await?;
    let mut out = Vec::new();
    for mut rec in st.store.org_sandboxes(&org.slug).await? {
        sandboxes::live(&st, &mut rec).await;
        out.push(sandboxes::view(&st, &rec).await?);
    }
    Ok(Json(out))
}

/// Creates the sandbox and boots it at once with its (validated or default) settings.
pub async fn create(
    State(st): State<AppState>,
    Caller(user): Caller,
    Path(org): Path<String>,
    ApiJson(body): ApiJson<CreateSandbox>,
) -> Result<(StatusCode, Json<Sandbox>), AppError> {
    let (org, _) = org_access(&st, &user, &org).await?;
    let image = sandboxes::image(&st)?.reference();
    if !is_project_slug(&body.slug) {
        return Err(AppError::invalid(format!(
            "sandbox slug: 1–{PROJECT_SLUG_MAX} lowercase letters, digits or `-`, starting with a letter"
        )));
    }
    let settings = sandboxes::validated(body.settings.unwrap_or_else(sandboxes::default_settings))?;
    let _turn = st.sandbox_locks.lock(&body.slug).await;
    let mut rec = SandboxRec {
        slug: body.slug,
        org: org.slug,
        created_by: user.id,
        created_at: st.now(),
        image,
        settings,
        state: SandboxState::Starting,
        error: None,
        stops_at: None,
        cpus: st.cfg.sandbox_cpus,
        mem_mib: st.cfg.sandbox_mem_mib,
        disk_mib: st.cfg.sandbox_disk_mib,
    };
    st.store.create_sandbox(&rec, st.cfg.max_sandboxes_per_org).await?;
    tracing::info!(sandbox = %rec.slug, org = %rec.org, user = %rec.created_by, "sandbox created");
    sandboxes::boot(&st, &mut rec).await?;
    Ok((StatusCode::CREATED, Json(sandboxes::view(&st, &rec).await?)))
}

pub async fn get(State(st): State<AppState>, Caller(user): Caller, Path(s): Path<String>) -> Result<Json<Sandbox>, AppError> {
    let rec = live(&st, &user, &s).await?;
    Ok(Json(sandboxes::view(&st, &rec).await?))
}

/// Destroys the VM and its disk, then forgets the sandbox and its environment.
pub async fn delete(State(st): State<AppState>, Caller(user): Caller, Path(s): Path<String>) -> Result<StatusCode, AppError> {
    let (rec, _turn) = locked(&st, &user, &s).await?;
    st.services.runtime.delete_sandbox(&rec.slug).await?;
    st.store.delete_sandbox(&rec.slug).await?;
    tracing::info!(sandbox = %rec.slug, org = %rec.org, user = %user.id, "sandbox deleted");
    Ok(StatusCode::NO_CONTENT)
}

/// Boots the sandbox (again) from the current image with its disk, settings and environment;
/// on a running one this restarts the auto-stop clock.
pub async fn start(State(st): State<AppState>, Caller(user): Caller, Path(s): Path<String>) -> Result<Json<Sandbox>, AppError> {
    let (mut rec, _turn) = locked(&st, &user, &s).await?;
    rec.image = sandboxes::image(&st)?.reference();
    sandboxes::boot(&st, &mut rec).await?;
    Ok(Json(sandboxes::view(&st, &rec).await?))
}

pub async fn stop(State(st): State<AppState>, Caller(user): Caller, Path(s): Path<String>) -> Result<Json<Sandbox>, AppError> {
    let (mut rec, _turn) = locked(&st, &user, &s).await?;
    sandboxes::stop(&st, &mut rec).await?;
    Ok(Json(sandboxes::view(&st, &rec).await?))
}

/// Replaces the settings; a running (or starting) sandbox is restarted with them.
pub async fn settings(
    State(st): State<AppState>,
    Caller(user): Caller,
    Path(s): Path<String>,
    ApiJson(body): ApiJson<SandboxSettings>,
) -> Result<Json<Sandbox>, AppError> {
    let (mut rec, _turn) = locked(&st, &user, &s).await?;
    rec.settings = sandboxes::validated(body)?;
    if matches!(rec.state, SandboxState::Running | SandboxState::Starting) {
        sandboxes::boot(&st, &mut rec).await?;
    } else {
        st.store.update_sandbox(&rec).await?;
    }
    Ok(Json(sandboxes::view(&st, &rec).await?))
}

fn env_view(var: &EnvVarRec) -> EnvVarView {
    EnvVarView { name: var.name.clone(), fingerprint: var.fingerprint.clone() }
}

pub async fn env(State(st): State<AppState>, Caller(user): Caller, Path(s): Path<String>) -> Result<Json<Vec<EnvVarView>>, AppError> {
    let (rec, _) = sandbox_access(&st, &user, &s).await?;
    Ok(Json(st.store.sandbox_env(&rec.slug).await?.iter().map(env_view).collect()))
}

/// Stores the value sealed; it reaches the VM at its next start.
pub async fn set_env(
    State(st): State<AppState>,
    Caller(user): Caller,
    Path(s): Path<String>,
    ApiJson(body): ApiJson<SetEnvVar>,
) -> Result<Json<EnvVarView>, AppError> {
    let (rec, _turn) = locked(&st, &user, &s).await?;
    if !valid_name(&body.name) {
        return Err(AppError::invalid("environment variable names are [A-Z][A-Z0-9_]*, at most 128 characters"));
    }
    if body.value.is_empty() || body.value.len() > MAX_VALUE_BYTES {
        return Err(AppError::invalid(format!("environment variable values are 1–{MAX_VALUE_BYTES} bytes")));
    }
    let var = EnvVarRec {
        sandbox: rec.slug.clone(),
        name: body.name.clone(),
        ciphertext: st.secrets.seal_env(&rec.slug, &body.name, &body.value)?,
        fingerprint: st.secrets.fingerprint(&body.value),
        updated_at: st.now(),
    };
    st.store.put_sandbox_env(&var).await?;
    Ok(Json(env_view(&var)))
}

pub async fn delete_env(
    State(st): State<AppState>,
    Caller(user): Caller,
    Path((s, name)): Path<(String, String)>,
) -> Result<StatusCode, AppError> {
    let (rec, _turn) = locked(&st, &user, &s).await?;
    if st.store.delete_sandbox_env(&rec.slug, &name).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::not_found(format!("sandbox {} has no variable {name}", rec.slug)))
    }
}

/// What the terminal runs: the request's command, else a login shell, in `/workspace`, on a PTY
/// of the requested size (clamped), or on pipes with `no_pty`.
fn terminal_exec(req: TerminalRequest) -> Result<ExecRequest, AppError> {
    let argv = match req.command {
        None => vec!["bash".to_owned(), "-l".to_owned()],
        Some(argv) => {
            if argv.first().is_none_or(String::is_empty) {
                return Err(AppError::invalid("command: name a program to run"));
            }
            if argv.len() > MAX_ARGS || argv.iter().any(|a| a.len() > MAX_ARG_BYTES || a.contains('\0')) {
                return Err(AppError::invalid(format!(
                    "command: at most {MAX_ARGS} arguments of at most {MAX_ARG_BYTES} bytes, without NUL"
                )));
            }
            argv
        }
    };
    Ok(ExecRequest {
        argv,
        pty: (!req.no_pty).then(|| PtySize { cols: req.cols.clamp(1, MAX_COLS), rows: req.rows.clamp(1, MAX_ROWS) }),
        env: BTreeMap::from([("TERM".to_owned(), "xterm-256color".to_owned())]),
        cwd: Some(SANDBOX_WORKSPACE.to_owned()),
    })
}

/// A single-use terminal socket for the caller, bound to exactly this command.
pub async fn terminal(
    State(st): State<AppState>,
    Caller(user): Caller,
    Path(s): Path<String>,
    ApiJson(body): ApiJson<TerminalRequest>,
) -> Result<Json<TerminalAccess>, AppError> {
    let rec = live(&st, &user, &s).await?;
    let exec = terminal_exec(body)?;
    require_running(&rec)?;
    let access = st.services.session_host.sandbox_terminal(SandboxTerminal { sandbox: rec.slug, user_id: user.id, exec }).await?;
    Ok(Json(access))
}

/// aho rpc to the aho inside, for the caller (resuming `session_id` when given). An empty body
/// is a new or the last chat.
pub async fn chat(
    State(st): State<AppState>,
    Caller(user): Caller,
    Path(s): Path<String>,
    body: Bytes,
) -> Result<Json<SandboxChat>, AppError> {
    let rec = live(&st, &user, &s).await?;
    let req: StartSandboxChat = if body.iter().all(u8::is_ascii_whitespace) {
        StartSandboxChat::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| AppError::invalid(format!("chat request: {e}")))?
    };
    if req.session_id.as_deref().is_some_and(|id| !valid_session_id(id)) {
        return Err(AppError::invalid("session_id: 1–128 of [A-Za-z0-9_-]"));
    }
    require_running(&rec)?;
    let chat =
        st.services.session_host.sandbox_chat(SandboxChatStart { sandbox: rec.slug, user_id: user.id, session_id: req.session_id }).await?;
    Ok(Json(chat))
}

pub async fn files(State(st): State<AppState>, Caller(user): Caller, Path(s): Path<String>) -> Result<Json<FileTree>, AppError> {
    let rec = live(&st, &user, &s).await?;
    require_running(&rec)?;
    let tree = st.services.runtime.sandbox_files(&rec.slug).await.map_err(|e| match e {
        PortError::NotFound { .. } | PortError::Rejected { .. } => AppError::conflict(format!("sandbox {} is not running", rec.slug)),
        e => e.into(),
    })?;
    Ok(Json(tree))
}

pub async fn file(
    State(st): State<AppState>,
    Caller(user): Caller,
    Path(s): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<FileContent>, AppError> {
    let rec = live(&st, &user, &s).await?;
    let path = q.get("path").map(String::as_str).unwrap_or_default();
    if !valid_path(path) {
        return Err(AppError::invalid("`?path=` is required: relative to the workspace root, `/`-separated, without `..`"));
    }
    require_running(&rec)?;
    let content = st.services.runtime.sandbox_file(&rec.slug, path).await.map_err(|e| match e {
        PortError::NotFound { .. } => AppError::not_found(format!("`{path}` is not in the sandbox's workspace")),
        e => e.into(),
    })?;
    Ok(Json(content))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(cols: u16, rows: u16, command: Option<&[&str]>) -> TerminalRequest {
        TerminalRequest { cols, rows, command: command.map(|c| c.iter().map(|s| (*s).to_owned()).collect()), no_pty: false }
    }

    #[test]
    fn terminal_sizes_are_clamped_and_commands_bounded() {
        assert_eq!(terminal_exec(req(0, 0, None)).unwrap().pty, Some(PtySize { cols: 1, rows: 1 }));
        assert_eq!(terminal_exec(req(u16::MAX, u16::MAX, None)).unwrap().pty, Some(PtySize { cols: MAX_COLS, rows: MAX_ROWS }));
        let long = "x".repeat(MAX_ARG_BYTES + 1);
        let many = vec!["a"; MAX_ARGS + 1];
        let bad: [&[&str]; 5] = [&[], &[""], &["ls", "a\0b"], &["echo", long.as_str()], many.as_slice()];
        for bad in bad {
            assert!(terminal_exec(req(80, 24, Some(bad))).is_err(), "{bad:?}");
        }
    }
}
