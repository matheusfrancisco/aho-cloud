//! Workspaces on disk.
//!
//! A workspace is one user's working copy of one project plus the aho session that works on it:
//!
//! ```text
//! <data_root>/workspaces/<id>/workspace.json   {"project", "user_id", "session_id"}
//! <data_root>/workspaces/<id>/project/         the `{dir}` handed to `ahoc --project`
//! ```
//!
//! The id is derived from `(project, user)` so starting the same pair again resumes it, and it is
//! URL- and path-safe by construction (`<project slug>-<16 hex>`). The session id is learned from
//! the first `hello_ok` and persisted, so the next connection resumes the same aho session.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use serde::{Deserialize, Serialize};

use cloud_contract::validate::{PROJECT_SLUG_MAX, is_project_slug};

/// Longest accepted user id, in bytes.
pub const USER_MAX: usize = 256;
const HASH_HEX: usize = 16;
const STATE_FILE: &str = "workspace.json";
const STATE_TMP: &str = "workspace.json.tmp";
const PROJECT_DIR: &str = "project";

/// An aho session id as `hello_ok` reports it: lowercase hex of a `u128`.
pub fn is_session_id(s: &str) -> bool {
    (1..=32).contains(&s.len()) && s.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

fn is_user_id(s: &str) -> bool {
    !s.is_empty() && s.len() <= USER_MAX && !s.chars().any(char::is_control)
}

/// The workspace id for `(project, user_id)`: `<project>-<first 16 hex of BLAKE3(domain ‖ project
/// ‖ 0 ‖ user)>`.
pub fn workspace_id(project: &str, user_id: &str) -> String {
    let mut h = blake3::Hasher::new();
    h.update(b"aho-cloud/workspace/v1\0");
    h.update(project.as_bytes());
    h.update(b"\0");
    h.update(user_id.as_bytes());
    let hex = h.finalize().to_hex();
    format!("{project}-{}", &hex[..HASH_HEX])
}

/// Whether `s` has the shape [`workspace_id`] produces. Anything else never touches the disk.
pub fn is_workspace_id(s: &str) -> bool {
    if !s.is_ascii() {
        return false;
    }
    let Some(split) = s.len().checked_sub(HASH_HEX + 1) else { return false };
    let (project, rest) = s.split_at(split);
    let Some(hash) = rest.strip_prefix('-') else { return false };
    is_project_slug(project) && hash.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

/// Why a workspace could not be started or opened.
#[derive(Debug, thiserror::Error)]
pub enum WorkspaceError {
    #[error("`{0}` is not a project slug (lowercase letters, digits and `-`, at most {PROJECT_SLUG_MAX})")]
    InvalidProject(String),
    #[error("user id must be 1..={USER_MAX} bytes without control characters")]
    InvalidUser,
    #[error("`{0}` is not an aho session id (lowercase hex, at most 32 digits)")]
    InvalidSession(String),
    #[error("no workspace `{0}`")]
    NotFound(String),
    #[error("workspace `{id}` state is unusable: {reason}")]
    Corrupt { id: String, reason: String },
    #[error("{path}: {source}")]
    Io { path: PathBuf, source: io::Error },
}

#[derive(Debug, Serialize, Deserialize)]
struct State {
    project: String,
    user_id: String,
    #[serde(default)]
    session_id: Option<String>,
}

/// One open workspace. Shared by every connection to it; the session id is the only mutable part.
#[derive(Debug)]
pub struct Workspace {
    pub id: String,
    pub project: String,
    pub user_id: String,
    /// `<data_root>/workspaces/<id>`.
    pub root: PathBuf,
    /// `<root>/project`, the directory `ahoc` works in.
    pub project_dir: PathBuf,
    /// Also serialises writes of `workspace.json`.
    session: tokio::sync::Mutex<Option<String>>,
}

impl Workspace {
    /// The aho session this workspace resumes, once known.
    pub async fn session_id(&self) -> Option<String> {
        self.session.lock().await.clone()
    }

    /// Records (and persists) the session the next connection resumes. No-op if unchanged.
    pub async fn set_session_id(&self, session_id: &str) -> Result<(), WorkspaceError> {
        if !is_session_id(session_id) {
            return Err(WorkspaceError::InvalidSession(session_id.to_owned()));
        }
        let mut current = self.session.lock().await;
        if current.as_deref() == Some(session_id) {
            return Ok(());
        }
        let state = State { project: self.project.clone(), user_id: self.user_id.clone(), session_id: Some(session_id.to_owned()) };
        write_state(&self.id, &self.root, &state).await?;
        *current = Some(session_id.to_owned());
        Ok(())
    }
}

/// All workspaces under one data root.
#[derive(Debug)]
pub struct WorkspaceStore {
    root: PathBuf,
    open: Mutex<HashMap<String, Arc<Workspace>>>,
}

impl WorkspaceStore {
    /// Workspaces live in `<data_root>/workspaces`; nothing is created until [`Self::start`].
    pub fn new(data_root: &Path) -> Self {
        Self { root: data_root.join("workspaces"), open: Mutex::new(HashMap::new()) }
    }

    /// Creates or resumes the workspace for `(project, user_id)`. A `session_id` replaces the one
    /// the workspace would otherwise resume.
    pub async fn start(&self, project: &str, user_id: &str, session_id: Option<&str>) -> Result<Arc<Workspace>, WorkspaceError> {
        if !is_project_slug(project) {
            return Err(WorkspaceError::InvalidProject(project.to_owned()));
        }
        if !is_user_id(user_id) {
            return Err(WorkspaceError::InvalidUser);
        }
        if let Some(s) = session_id.filter(|s| !is_session_id(s)) {
            return Err(WorkspaceError::InvalidSession(s.to_owned()));
        }
        let id = workspace_id(project, user_id);
        let ws = match self.cached(&id) {
            Some(ws) => ws,
            None => {
                let root = self.root.join(&id);
                let project_dir = root.join(PROJECT_DIR);
                tokio::fs::create_dir_all(&project_dir).await.map_err(|source| WorkspaceError::Io { path: project_dir.clone(), source })?;
                let state = match read_state(&root).await? {
                    Some(state) if state.project == project && state.user_id == user_id => state,
                    Some(_) => {
                        return Err(WorkspaceError::Corrupt { id, reason: "recorded project/user do not match the id".into() });
                    }
                    None => {
                        let state = State { project: project.to_owned(), user_id: user_id.to_owned(), session_id: None };
                        write_state(&id, &root, &state).await?;
                        state
                    }
                };
                self.remember(id, root, state)
            }
        };
        if let Some(s) = session_id {
            ws.set_session_id(s).await?;
        }
        Ok(ws)
    }

    /// Opens an existing workspace by id (after a restart it is read back from disk).
    pub async fn open(&self, id: &str) -> Result<Arc<Workspace>, WorkspaceError> {
        if !is_workspace_id(id) {
            return Err(WorkspaceError::NotFound(id.to_owned()));
        }
        if let Some(ws) = self.cached(id) {
            return Ok(ws);
        }
        let root = self.root.join(id);
        let state = read_state(&root).await?.ok_or_else(|| WorkspaceError::NotFound(id.to_owned()))?;
        if workspace_id(&state.project, &state.user_id) != id {
            return Err(WorkspaceError::Corrupt { id: id.to_owned(), reason: "recorded project/user do not match the id".into() });
        }
        if let Some(s) = state.session_id.as_deref().filter(|s| !is_session_id(s)) {
            return Err(WorkspaceError::Corrupt { id: id.to_owned(), reason: format!("recorded session id `{s}` is not hex") });
        }
        Ok(self.remember(id.to_owned(), root, state))
    }

    fn cached(&self, id: &str) -> Option<Arc<Workspace>> {
        self.open.lock().unwrap_or_else(PoisonError::into_inner).get(id).cloned()
    }

    /// Inserts unless a concurrent caller got there first; either way returns the shared one.
    fn remember(&self, id: String, root: PathBuf, state: State) -> Arc<Workspace> {
        let mut open = self.open.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(open.entry(id.clone()).or_insert_with(|| {
            Arc::new(Workspace {
                id,
                project: state.project,
                user_id: state.user_id,
                project_dir: root.join(PROJECT_DIR),
                root,
                session: tokio::sync::Mutex::new(state.session_id),
            })
        }))
    }
}

async fn read_state(root: &Path) -> Result<Option<State>, WorkspaceError> {
    let path = root.join(STATE_FILE);
    let bytes = match tokio::fs::read(&path).await {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(WorkspaceError::Io { path, source }),
    };
    serde_json::from_slice(&bytes).map(Some).map_err(|e| WorkspaceError::Corrupt {
        id: root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        reason: e.to_string(),
    })
}

/// Write-then-rename, so a crash never leaves a half-written state file.
async fn write_state(id: &str, root: &Path, state: &State) -> Result<(), WorkspaceError> {
    let bytes = serde_json::to_vec_pretty(state).map_err(|e| WorkspaceError::Corrupt { id: id.to_owned(), reason: e.to_string() })?;
    let tmp = root.join(STATE_TMP);
    tokio::fs::write(&tmp, bytes).await.map_err(|source| WorkspaceError::Io { path: tmp.clone(), source })?;
    let path = root.join(STATE_FILE);
    tokio::fs::rename(&tmp, &path).await.map_err(|source| WorkspaceError::Io { path, source })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_stable_per_pair_and_distinct_across_pairs() {
        let a = workspace_id("shop", "u1");
        assert_eq!(a, workspace_id("shop", "u1"));
        assert_ne!(a, workspace_id("shop", "u2"));
        assert_ne!(a, workspace_id("shops", "u1"));
        // The separator keeps `("ab", "c")` and `("a", "bc")`-style splits apart.
        assert_ne!(workspace_id("a", "b\0c"), workspace_id("a", "b"));
        assert!(a.starts_with("shop-") && is_workspace_id(&a), "{a}");
    }

    #[test]
    fn only_derived_shapes_are_workspace_ids() {
        assert!(is_workspace_id("my-app-0123456789abcdef"));
        for bad in [
            "",
            "0123456789abcdef",
            "-0123456789abcdef",
            "../../etc-0123456789abcdef",
            "Shop-0123456789abcdef",
            "shop-0123456789ABCDEF",
            "shop_0123456789abcdef",
            "shop-0123456789abcde",
            "shop-0123456789abcdef0",
            "shop--0123456789abcdef",
            "xé0123456789abcdef",
        ] {
            assert!(!is_workspace_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn session_ids_are_lowercase_hex_u128() {
        assert!(is_session_id("0") && is_session_id("5e55100") && is_session_id(&"f".repeat(32)));
        for bad in ["", "5E55", "--help", "12 3", "g", &"f".repeat(33)] {
            assert!(!is_session_id(bad), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn start_creates_the_project_dir_and_resume_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let store = WorkspaceStore::new(dir.path());
        let ws = store.start("shop", "u1", None).await.unwrap();
        assert!(ws.project_dir.is_dir());
        assert_eq!(ws.session_id().await, None);
        ws.set_session_id("abc123").await.unwrap();

        let restarted = WorkspaceStore::new(dir.path());
        let again = restarted.open(&ws.id).await.unwrap();
        assert_eq!((again.project.as_str(), again.user_id.as_str()), ("shop", "u1"));
        assert_eq!(again.session_id().await.as_deref(), Some("abc123"));
        // Starting the same pair resumes the recorded session; an explicit one overrides it.
        let started = restarted.start("shop", "u1", None).await.unwrap();
        assert_eq!(started.session_id().await.as_deref(), Some("abc123"));
        let switched = restarted.start("shop", "u1", Some("ff")).await.unwrap();
        assert_eq!(switched.session_id().await.as_deref(), Some("ff"));
        assert_eq!(WorkspaceStore::new(dir.path()).open(&ws.id).await.unwrap().session_id().await.as_deref(), Some("ff"));
    }

    #[tokio::test]
    async fn bad_input_is_refused_before_touching_disk() {
        let dir = tempfile::tempdir().unwrap();
        let store = WorkspaceStore::new(dir.path());
        assert!(matches!(store.start("Bad", "u1", None).await, Err(WorkspaceError::InvalidProject(_))));
        assert!(matches!(store.start("shop", "", None).await, Err(WorkspaceError::InvalidUser)));
        assert!(matches!(store.start("shop", "u\n1", None).await, Err(WorkspaceError::InvalidUser)));
        assert!(matches!(store.start("shop", "u1", Some("--help")).await, Err(WorkspaceError::InvalidSession(_))));
        assert!(matches!(store.open("../x").await, Err(WorkspaceError::NotFound(_))));
        assert!(matches!(store.open(&workspace_id("shop", "nobody")).await, Err(WorkspaceError::NotFound(_))));
        assert!(!dir.path().join("workspaces").exists());
        let ws = store.start("shop", "u1", None).await.unwrap();
        assert!(matches!(ws.set_session_id("-x").await, Err(WorkspaceError::InvalidSession(_))));
    }
}
