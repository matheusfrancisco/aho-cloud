//! A project: view, deletion, uploads, the ledger, workspaces.

use std::collections::HashMap;

use aho_log::{LogStore, MemoryLog};
use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use cloud_api::internal::StartWorkspace;
use cloud_api::{ArchiveUploaded, LedgerEntry, Project, SessionUploaded, Workspace};
use serde::Deserialize;

use crate::auth::{Caller, project_access, require_owner};
use crate::error::AppError;
use crate::ops::ledger_state;
use crate::state::AppState;
use crate::store::{BlobKind, BlobRec, SessionRec};
use crate::view;

pub async fn get(State(st): State<AppState>, Caller(user): Caller, Path(p): Path<String>) -> Result<Json<Project>, AppError> {
    let (project, _) = project_access(&st, &user, &p).await?;
    let registry = st.registry.lock().await.clone();
    Ok(Json(view::project(&project.slug, &project.org, &ledger_state(&st, &project.slug)?, &registry)))
}

/// Deletes the project: every hostname it held is tombstoned (never served to another tenant
/// until a new binding verifies), its deployments are put to sleep, and the slug is retired.
pub async fn delete(State(st): State<AppState>, Caller(user): Caller, Path(p): Path<String>) -> Result<StatusCode, AppError> {
    let (project, member) = project_access(&st, &user, &p).await?;
    require_owner(&member, "deleting a project")?;
    let now = st.now();
    {
        let mut registry = st.registry.lock().await;
        registry.remove_project(&project.slug, now);
        st.save_registry(&registry).await?;
    }
    st.store.delete_project(&project.slug, now).await?;
    let active = ledger_state(&st, &project.slug)?.active;
    for stage in active.keys() {
        if let Err(error) = st.services.runtime.sleep(&project.slug, *stage).await {
            tracing::warn!(project = %project.slug, stage = stage.as_str(), %error, "could not stop deployment of deleted project");
        }
    }
    Ok(StatusCode::NO_CONTENT)
}

pub async fn upload_archive(
    State(st): State<AppState>,
    Caller(user): Caller,
    Path(p): Path<String>,
    body: Bytes,
) -> Result<Json<ArchiveUploaded>, AppError> {
    let (project, _) = project_access(&st, &user, &p).await?;
    Ok(Json(store_archive(&st, &project.slug, &body).await?))
}

/// Stores `bytes` as a source archive of `project`, addressed by its content hash.
pub(super) async fn store_archive(st: &AppState, project: &str, bytes: &[u8]) -> Result<ArchiveUploaded, AppError> {
    if bytes.is_empty() {
        return Err(AppError::invalid("empty archive"));
    }
    let hash = st.blobs.put(bytes).await?;
    let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
    st.store
        .put_blob(&BlobRec { project: project.to_owned(), hash: hash.clone(), kind: BlobKind::Archive, size, created_at: st.now() })
        .await?;
    Ok(ArchiveUploaded { hash, bytes: size })
}

/// What a verified session log proves.
#[derive(Debug, PartialEq, Eq)]
pub struct VerifiedLog {
    pub head_hash: String,
    pub entries: u64,
}

/// Replays an aho JSONL session log, checking every entry's hash and, for every branch head,
/// the parent linkage back to the root. The head is the log's current head.
pub fn verify_session_log(text: &str) -> Result<VerifiedLog, String> {
    let log = MemoryLog::from_jsonl(text).map_err(|e| e.to_string())?;
    let head = log.head().ok_or("empty session log")?;
    for h in log.heads() {
        log.read_branch(h).map_err(|e| e.to_string())?;
    }
    let entry = log.get(head).ok_or("head entry missing")?;
    let entries = u64::try_from(log.read_from(None).len()).unwrap_or(u64::MAX);
    Ok(VerifiedLog { head_hash: entry.hash.to_string(), entries })
}

fn valid_session_id(id: &str) -> bool {
    (1..=128).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

pub async fn upload_session(
    State(st): State<AppState>,
    Caller(user): Caller,
    Path(p): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    body: Bytes,
) -> Result<Json<SessionUploaded>, AppError> {
    let (project, _) = project_access(&st, &user, &p).await?;
    let session_id = q.get("session_id").map(String::as_str).unwrap_or_default();
    if !valid_session_id(session_id) {
        return Err(AppError::invalid("`?session_id=` is required (1–128 of [A-Za-z0-9_-])"));
    }
    let verified = verify_session(session_id, &body)?;
    Ok(Json(record_session(&st, &project.slug, session_id, &body, verified).await?))
}

/// Checks an uploaded session log: a well-formed id, UTF-8, and an intact hash chain.
pub(super) fn verify_session(session_id: &str, log: &[u8]) -> Result<VerifiedLog, AppError> {
    if !valid_session_id(session_id) {
        return Err(AppError::invalid("a session id is 1–128 of [A-Za-z0-9_-]"));
    }
    let text = std::str::from_utf8(log).map_err(|_| AppError::invalid("session log is not UTF-8"))?;
    verify_session_log(text).map_err(|e| AppError::invalid(format!("session log refused: {e}")))
}

/// Records a log [`verify_session`] accepted for `project`, so releases may name it as attested
/// provenance.
pub(super) async fn record_session(
    st: &AppState,
    project: &str,
    session_id: &str,
    log: &[u8],
    verified: VerifiedLog,
) -> Result<SessionUploaded, AppError> {
    let blob_hash = st.blobs.put(log).await?;
    let size = u64::try_from(log.len()).unwrap_or(u64::MAX);
    st.store
        .put_blob(&BlobRec { project: project.to_owned(), hash: blob_hash.clone(), kind: BlobKind::Session, size, created_at: st.now() })
        .await?;
    st.store
        .put_session(&SessionRec {
            project: project.to_owned(),
            session_id: session_id.to_owned(),
            head_hash: verified.head_hash.clone(),
            blob_hash,
            entries: verified.entries,
            created_at: st.now(),
        })
        .await?;
    Ok(SessionUploaded { session_id: session_id.to_owned(), head_hash: verified.head_hash, entries: verified.entries })
}

pub async fn ledger(State(st): State<AppState>, Caller(user): Caller, Path(p): Path<String>) -> Result<Json<Vec<LedgerEntry>>, AppError> {
    let (project, _) = project_access(&st, &user, &p).await?;
    Ok(Json(st.ledgers.get(&project.slug)?.entries()?))
}

#[derive(Default, Deserialize)]
struct WorkspaceRequest {
    #[serde(default)]
    session_id: Option<String>,
}

pub async fn workspace(
    State(st): State<AppState>,
    Caller(user): Caller,
    Path(p): Path<String>,
    body: Bytes,
) -> Result<Json<Workspace>, AppError> {
    let (project, _) = project_access(&st, &user, &p).await?;
    let req: WorkspaceRequest = if body.iter().all(u8::is_ascii_whitespace) {
        WorkspaceRequest::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| AppError::invalid(format!("workspace request: {e}")))?
    };
    if req.session_id.as_deref().is_some_and(|s| !valid_session_id(s)) {
        return Err(AppError::invalid("session_id: 1–128 of [A-Za-z0-9_-]"));
    }
    let started =
        st.services.session_host.start(StartWorkspace { project: project.slug, user_id: user.id, session_id: req.session_id }).await?;
    Ok(Json(Workspace { rpc_url: started.rpc_url, token: started.token, session_id: started.session_id, preview_url: started.preview_url }))
}
