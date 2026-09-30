//! The aho session that worked on the directory: aho writes one `<session id>.jsonl` per session
//! under `.aho/sessions`. Picked logs must be regular files inside the directory, so a symlink
//! cannot make a directory attest with somebody else's session.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use cloud_contract::draft::Draft;
use cloud_draft::SessionSummary;

use crate::error::{WorkdirError, io};

/// A read and chain-verified session log.
#[derive(Clone, Debug)]
pub struct Session {
    /// aho's session id: the log's file stem.
    pub id: String,
    pub summary: SessionSummary,
    /// What the session says about deploying (see `cloud_draft`).
    pub draft: Draft,
    /// The log as it is on disk, for upload.
    pub bytes: Vec<u8>,
}

/// `<dir>/.aho/sessions`.
pub fn sessions_dir(dir: &Path) -> PathBuf {
    dir.join(".aho").join("sessions")
}

/// Newest `*.jsonl` regular file in `<dir>/.aho/sessions` by modification time (then name).
/// Symlinked logs are skipped; a sessions directory that resolves outside `dir` is
/// [`WorkdirError::Escapes`].
pub fn newest_session(dir: &Path) -> Result<Option<PathBuf>, WorkdirError> {
    let Some(sessions) = contained_sessions(dir)? else { return Ok(None) };
    let entries = std::fs::read_dir(&sessions).map_err(io(&sessions))?;
    let mut best: Option<(SystemTime, PathBuf)> = None;
    for entry in entries {
        let entry = entry.map_err(io(&sessions))?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") || !entry.file_type().map_err(io(&path))?.is_file() {
            continue;
        }
        let modified = entry.metadata().and_then(|m| m.modified()).map_err(io(&path))?;
        if best.as_ref().is_none_or(|(t, p)| (modified, &path) > (*t, p)) {
            best = Some((modified, path));
        }
    }
    Ok(best.map(|(_, p)| p))
}

/// The log of session `id` in `<dir>/.aho/sessions`, if it is there as a regular file. Ids are
/// file stems, so anything but `[A-Za-z0-9_-]` is never looked up.
pub fn session_log(dir: &Path, id: &str) -> Result<Option<PathBuf>, WorkdirError> {
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')) {
        return Ok(None);
    }
    let Some(sessions) = contained_sessions(dir)? else { return Ok(None) };
    let path = sessions.join(format!("{id}.jsonl"));
    match std::fs::symlink_metadata(&path) {
        Ok(m) if m.is_file() => Ok(Some(path)),
        Ok(_) => Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io(path)(e)),
    }
}

/// `<dir>/.aho/sessions` if it exists, provided it resolves inside `dir`.
fn contained_sessions(dir: &Path) -> Result<Option<PathBuf>, WorkdirError> {
    let sessions = sessions_dir(dir);
    let real = match sessions.canonicalize() {
        Ok(p) => p,
        Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => return Ok(None),
        Err(e) => return Err(io(sessions)(e)),
    };
    let root = dir.canonicalize().map_err(io(dir))?;
    if !real.starts_with(&root) {
        return Err(WorkdirError::Escapes(".aho/sessions".into()));
    }
    Ok(Some(sessions))
}

/// Reads and verifies a session log; its id is the file stem, as aho names them.
pub fn load_session(path: &Path) -> Result<Session, WorkdirError> {
    let bytes = std::fs::read(path).map_err(io(path))?;
    let text = std::str::from_utf8(&bytes).map_err(|_| WorkdirError::SessionNotUtf8(path.to_owned()))?;
    let id = path.file_stem().and_then(|s| s.to_str()).ok_or_else(|| WorkdirError::SessionName(path.to_owned()))?.to_owned();
    let session_err = |source| WorkdirError::Session { path: path.to_owned(), source };
    let entries = cloud_draft::verify_jsonl(text).map_err(session_err)?;
    let summary = SessionSummary::of(&id, &entries).ok_or_else(|| session_err(cloud_draft::Error::Empty))?;
    let draft = cloud_draft::draft_from_entries(&id, &entries);
    Ok(Session { id, summary, draft, bytes })
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;
    use crate::tests::write_session;

    #[test]
    fn loads_and_verifies_a_real_log() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_session(dir.path(), "5e55", &["built the server", "ready to ship"]);
        let s = load_session(&path).unwrap();
        assert_eq!((s.id.as_str(), s.summary.session_id.as_str(), s.summary.entries), ("5e55", "5e55", 2));
        assert_eq!(s.bytes, fs::read(&path).unwrap());
        let entries = cloud_draft::verify_jsonl(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(s.summary.head_hash, entries.last().unwrap().hash.to_hex());
    }

    #[test]
    fn a_tampered_empty_or_binary_log_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_session(dir.path(), "5e55", &["built the server", "ready to ship"]);
        let tampered = fs::read_to_string(&path).unwrap().replace("ready to ship", "ready to shop");
        fs::write(&path, tampered).unwrap();
        assert!(matches!(load_session(&path), Err(WorkdirError::Session { source: cloud_draft::Error::Tampered(_), .. })));
        fs::write(&path, "").unwrap();
        assert!(matches!(load_session(&path), Err(WorkdirError::Session { source: cloud_draft::Error::Empty, .. })));
        fs::write(&path, b"\xff\xfe").unwrap();
        assert!(matches!(load_session(&path), Err(WorkdirError::SessionNotUtf8(_))));
    }

    #[test]
    fn newest_is_by_mtime_and_ignores_other_files() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(newest_session(dir.path()).unwrap(), None, "no .aho yet");
        let old = write_session(dir.path(), "a", &["one"]);
        let new = write_session(dir.path(), "b", &["two"]);
        let t = SystemTime::now();
        fs::File::options().write(true).open(&old).unwrap().set_modified(t - std::time::Duration::from_secs(60)).unwrap();
        fs::File::options().write(true).open(&new).unwrap().set_modified(t).unwrap();
        // aho keeps an index next to the logs; only `.jsonl` files are sessions.
        fs::write(sessions_dir(dir.path()).join("index.sqlite3"), "db").unwrap();
        fs::File::options()
            .write(true)
            .open(sessions_dir(dir.path()).join("index.sqlite3"))
            .unwrap()
            .set_modified(t + std::time::Duration::from_secs(60))
            .unwrap();
        assert_eq!(newest_session(dir.path()).unwrap(), Some(new.clone()));
        fs::File::options().write(true).open(&old).unwrap().set_modified(t + std::time::Duration::from_secs(60)).unwrap();
        assert_eq!(newest_session(dir.path()).unwrap(), Some(old));
    }

    #[test]
    fn session_log_finds_a_recorded_id_and_never_builds_a_path_from_junk() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_session(dir.path(), "5e55", &["one"]);
        assert_eq!(session_log(dir.path(), "5e55").unwrap(), Some(path));
        assert_eq!(session_log(dir.path(), "ffff").unwrap(), None);
        for junk in ["", "../5e55", "5e55/..", "a.b", "a\0"] {
            assert_eq!(session_log(dir.path(), junk).unwrap(), None, "{junk:?}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn logs_reached_through_symlinks_do_not_count() {
        use std::os::unix::fs::symlink;
        let other = tempfile::tempdir().unwrap();
        let foreign = write_session(other.path(), "f00d", &["somebody else's work"]);

        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(sessions_dir(dir.path())).unwrap();
        symlink(&foreign, sessions_dir(dir.path()).join("f00d.jsonl")).unwrap();
        assert_eq!(newest_session(dir.path()).unwrap(), None);
        assert_eq!(session_log(dir.path(), "f00d").unwrap(), None);

        let linked = tempfile::tempdir().unwrap();
        symlink(other.path().join(".aho"), linked.path().join(".aho")).unwrap();
        assert!(matches!(newest_session(linked.path()), Err(WorkdirError::Escapes(_))));
        assert!(matches!(session_log(linked.path(), "f00d"), Err(WorkdirError::Escapes(_))));
    }
}
