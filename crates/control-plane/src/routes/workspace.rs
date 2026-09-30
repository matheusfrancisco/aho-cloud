//! The caller's own workspace in a project (the session host keys it by `(project, user)`): its
//! files, and deploying them as a release.

use std::collections::HashMap;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use cloud_api::internal::{WorkspaceFileRef, WorkspaceRef};
use cloud_api::{FileContent, FileTree, ProposeRelease, WorkspaceDeploy, WorkspaceDeployed};
use cloud_contract::draft::Draft;
use cloud_contract::ledger::{Proposer, Provenance};
use cloud_contract::manifest::Source;
use serde_json::Value;

use super::projects::{record_session, store_archive, verify_session};
use super::releases::propose_release;
use crate::auth::{Caller, project_access};
use crate::blobs::content_hash;
use crate::error::AppError;
use crate::ports::PortError;
use crate::state::AppState;

/// Longest workspace path forwarded to the session host.
const MAX_PATH: usize = 4096;

fn workspace_of(project: &str, user_id: &str) -> WorkspaceRef {
    WorkspaceRef { project: project.to_owned(), user_id: user_id.to_owned() }
}

/// A session host "not found" for the whole workspace, worded for the caller.
fn no_workspace(project: &str) -> impl FnOnce(PortError) -> AppError + '_ {
    move |e| match e {
        PortError::NotFound { .. } => AppError::not_found(format!("you have no workspace in `{project}` yet; open one first")),
        e => e.into(),
    }
}

pub async fn files(State(st): State<AppState>, Caller(user): Caller, Path(p): Path<String>) -> Result<Json<FileTree>, AppError> {
    let (project, _) = project_access(&st, &user, &p).await?;
    let tree = st.services.session_host.files(workspace_of(&project.slug, &user.id)).await.map_err(no_workspace(&project.slug))?;
    Ok(Json(tree))
}

/// Workspace-relative and `/`-separated, never absolute, never climbing out, no NUL. The
/// session host confines paths itself; the obvious escapes do not leave this process.
fn valid_path(path: &str) -> bool {
    !path.is_empty() && path.len() <= MAX_PATH && !path.starts_with('/') && !path.contains('\0') && path.split('/').all(|s| s != "..")
}

pub async fn file(
    State(st): State<AppState>,
    Caller(user): Caller,
    Path(p): Path<String>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Json<FileContent>, AppError> {
    let (project, _) = project_access(&st, &user, &p).await?;
    let path = q.get("path").map(String::as_str).unwrap_or_default();
    if !valid_path(path) {
        return Err(AppError::invalid("`?path=` is required: relative to the workspace root, `/`-separated, without `..`"));
    }
    let file = WorkspaceFileRef { project: project.slug.clone(), user_id: user.id.clone(), path: path.to_owned() };
    let content = st.services.session_host.file(file).await.map_err(|e| match e {
        PortError::NotFound { .. } => AppError::not_found(format!("`{path}` is not in your workspace")),
        e => e.into(),
    })?;
    Ok(Json(content))
}

/// The workspace's draft with `/source` pinned to the packed archive and `/project` to this
/// project, both declared (nothing to confirm). Whatever the workspace drafted there, including
/// deeper pointers such as `/source/hash`, is replaced.
fn pinned(mut draft: Draft, archive_hash: &str, project: &str) -> Result<Draft, AppError> {
    let source = serde_json::to_value(Source::Archive { hash: archive_hash.to_owned() }).map_err(|e| AppError::internal(e.to_string()))?;
    for (pointer, value) in [("/source", source), ("/project", Value::String(project.to_owned()))] {
        let nested = format!("{pointer}/");
        draft.fields.retain(|k, _| !k.starts_with(&nested));
        draft.declare(pointer, value);
    }
    Ok(draft)
}

fn decode(what: &str, b64: &str, max: usize) -> Result<Vec<u8>, AppError> {
    let bytes = STANDARD.decode(b64).map_err(|e| AppError::Upstream(format!("session host: the packed {what} is not base64: {e}")))?;
    if bytes.len() > max {
        return Err(AppError::TooLarge(format!("the workspace {what} is {} bytes; at most {max} are accepted", bytes.len())));
    }
    Ok(bytes)
}

/// Ships the caller's workspace files as a release: the session host packs them, the archive is
/// stored like an upload (after its hash is re-checked), the newest aho session is chain-verified
/// and recorded like an upload (making the release attested), and the manifest — the request's,
/// else the workspace's `deploy.json` — goes through the same pipeline as `POST releases` with
/// `source` forced to the archive. With neither, the answer is the draft to confirm.
pub async fn deploy(
    State(st): State<AppState>,
    Caller(user): Caller,
    Path(p): Path<String>,
    body: Bytes,
) -> Result<(StatusCode, Json<WorkspaceDeployed>), AppError> {
    let (project, _) = project_access(&st, &user, &p).await?;
    let req: WorkspaceDeploy = if body.iter().all(u8::is_ascii_whitespace) {
        WorkspaceDeploy::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| AppError::invalid(format!("deploy request: {e}")))?
    };
    let package = st.services.session_host.package(workspace_of(&project.slug, &user.id)).await.map_err(no_workspace(&project.slug))?;

    // Nothing the session host says is taken on faith: the archive must hash to what it named,
    // and the session log must verify here exactly as an uploaded one does.
    let max = st.cfg.max_upload_bytes;
    let archive = decode("archive", &package.archive, max)?;
    let hash = content_hash(&archive);
    if hash != package.archive_hash {
        return Err(AppError::Upstream(format!(
            "session host: the packed archive hashes to {hash}, not the {} it named",
            package.archive_hash
        )));
    }
    let session = match &package.session {
        Some(s) => {
            let log = decode("session log", &s.jsonl, max)?;
            let verified = verify_session(&s.session_id, &log)?;
            if verified.head_hash != s.head_hash {
                return Err(AppError::Upstream(format!(
                    "session host: session {} verifies to head {}, not the {} it named",
                    s.session_id, verified.head_hash, s.head_hash
                )));
            }
            Some((s.session_id.as_str(), log, verified))
        }
        None => None,
    };

    let mut manifest = match (req.manifest, package.manifest) {
        (Some(m), _) | (None, Some(m)) => m,
        (None, None) => match package.manifest_error {
            Some(message) => return Err(AppError::Refused { message, refusals: Vec::new() }),
            None => {
                let draft = pinned(package.draft, &hash, &project.slug)?;
                return Ok((StatusCode::OK, Json(WorkspaceDeployed::NeedsManifest { draft })));
            }
        },
    };
    manifest.source = Source::Archive { hash };

    store_archive(&st, &project.slug, &archive).await?;
    let provenance = match session {
        Some((session_id, log, verified)) => {
            let recorded = record_session(&st, &project.slug, session_id, &log, verified).await?;
            Provenance::Attested { session_id: recorded.session_id, head_hash: recorded.head_hash }
        }
        None => Provenance::Unattested { commit: None, ci_run: None },
    };
    let proposal = ProposeRelease { manifest, edge_toml: None, provenance };
    let release = propose_release(&st, &project.slug, Proposer::User { user_id: user.id }, proposal).await?;
    Ok((StatusCode::CREATED, Json(WorkspaceDeployed::Proposed { release: Box::new(release) })))
}

#[cfg(test)]
mod tests {
    use super::valid_path;

    #[test]
    fn paths_stay_inside_the_workspace() {
        for ok in ["deploy.json", "src/main.rs", "a/.hidden", "a/b..c", "..a"] {
            assert!(valid_path(ok), "{ok}");
        }
        for bad in ["", "/etc/passwd", "..", "../x", "a/../../x", "a/..", "a\0b", &"a".repeat(4097)] {
            assert!(!valid_path(bad), "{bad:?}");
        }
    }
}
