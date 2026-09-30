//! What is in a workspace, for the control plane: the package a "Deploy to preview" ships, the
//! file tree, one file. All of it is `cloud-workdir` run on the workspace's project directory
//! (the same code `ahocloud deploy` runs on a laptop), so what the console shows is what ships.
//!
//! Errors, as [`HttpError`]s: `400 invalid` for paths that are malformed, private (`.git`,
//! `.aho`), escape the workspace through a symlink or are not regular files; `404 not_found` for
//! files that are not there; `413 too_large` for an archive over the cap; `422 invalid_session`
//! when the session log that would attest the package does not verify (no package is returned:
//! a deploy never silently loses its attestation); `422 invalid_workspace` for trees the archive
//! cannot carry (non-UTF-8 names, unreadable directories). Messages name paths relative to the
//! workspace, never where the host keeps it.

use std::path::Path;

use axum::http::StatusCode;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use cloud_api::internal::{PackagedSession, WorkspacePackage};
use cloud_workdir::WorkdirError;
use serde_json::json;

use crate::server::HttpError;

/// Everything a deploy of the workspace at `dir` (belonging to `project`) needs.
///
/// The session is `recorded`'s log (the id the workspace resumes) when it is in
/// `.aho/sessions`, else the newest log there; it must verify. `deploy.json` that does not parse
/// is reported in `manifest_error`, not refused. The draft is `deploy.json` over the files over
/// the session, with `/source` declared as the archive and `/project` as `project`.
pub(crate) fn package(dir: &Path, project: &str, recorded: Option<&str>, max_archive_bytes: u64) -> Result<WorkspacePackage, HttpError> {
    let session_path = match recorded {
        Some(id) => cloud_workdir::session_log(dir, id).map_err(|e| session_error(dir, e))?,
        None => None,
    };
    let session_path = match session_path {
        Some(p) => Some(p),
        None => cloud_workdir::newest_session(dir).map_err(|e| session_error(dir, e))?,
    };
    let session = session_path.as_deref().map(cloud_workdir::load_session).transpose().map_err(|e| session_error(dir, e))?;
    let (manifest, manifest_error) = match cloud_workdir::read_manifest(dir) {
        Ok(m) => (m, None),
        Err(e) => (None, Some(manifest_problem(e))),
    };
    let archive = cloud_workdir::pack_within(dir, max_archive_bytes)?;

    let assembled = cloud_workdir::assemble(dir, manifest.as_ref(), session.as_ref().map(|s| &s.draft));
    if !assembled.problems.is_empty() {
        tracing::info!(%project, problems = ?assembled.problems, "workspace files the importers could not read");
    }
    let mut draft = assembled.draft;
    archive.declare_source(&mut draft);
    cloud_workdir::declare_subtree(&mut draft, "/project", json!(project));

    let files = archive.files.len() as u64;
    let archive_hash = archive.hash;
    let archive = STANDARD.encode(archive.bytes);
    let session = session.map(|s| PackagedSession {
        session_id: s.summary.session_id,
        head_hash: s.summary.head_hash,
        entries: s.summary.entries,
        jsonl: STANDARD.encode(s.bytes),
    });
    Ok(WorkspacePackage { archive, archive_hash, files, session, manifest, manifest_error, draft })
}

/// `path` as the workspace owner knows it.
fn shown(dir: &Path, path: &Path) -> String {
    path.strip_prefix(dir).unwrap_or(path).display().to_string()
}

/// Anything wrong with the session that would attest the package is `422 invalid_session`,
/// except the host's own I/O failing.
fn session_error(dir: &Path, e: WorkdirError) -> HttpError {
    let message = match e {
        WorkdirError::Io { .. } => return e.into(),
        WorkdirError::Session { path, source } => format!("session log {}: {source}", shown(dir, &path)),
        WorkdirError::SessionNotUtf8(path) => format!("session log {} is not UTF-8", shown(dir, &path)),
        WorkdirError::SessionName(path) => format!("session log {} has no session id in its name", shown(dir, &path)),
        other => other.to_string(),
    };
    HttpError::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_session", message)
}

fn manifest_problem(e: WorkdirError) -> String {
    match e {
        WorkdirError::Manifest { reason, .. } => format!("{}: not a deploy manifest: {reason}", cloud_workdir::MANIFEST),
        WorkdirError::Io { source, .. } => format!("{}: {source}", cloud_workdir::MANIFEST),
        other => other.to_string(),
    }
}

impl From<WorkdirError> for HttpError {
    fn from(e: WorkdirError) -> Self {
        match e {
            WorkdirError::BadPath(_) | WorkdirError::Private(_) | WorkdirError::Escapes(_) | WorkdirError::NotAFile(_) => {
                Self::new(StatusCode::BAD_REQUEST, "invalid", e.to_string())
            }
            WorkdirError::NotFound(_) => Self::new(StatusCode::NOT_FOUND, "not_found", e.to_string()),
            WorkdirError::TooLarge { .. } => Self::new(StatusCode::PAYLOAD_TOO_LARGE, "too_large", e.to_string()),
            WorkdirError::Walk(_) | WorkdirError::NotUtf8(_) | WorkdirError::Manifest { .. } => {
                Self::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_workspace", e.to_string())
            }
            WorkdirError::Session { .. } | WorkdirError::SessionNotUtf8(_) | WorkdirError::SessionName(_) => {
                Self::new(StatusCode::UNPROCESSABLE_ENTITY, "invalid_session", e.to_string())
            }
            WorkdirError::Io { .. } => {
                tracing::error!("workspace files: {e}");
                Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", "reading the workspace failed")
            }
        }
    }
}
