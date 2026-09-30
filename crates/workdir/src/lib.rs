//! A project directory as the cloud sees it. The CLI (`ahocloud deploy` on a laptop) and the
//! session host (a cloud workspace's "Deploy to preview") ship the same thing through this crate:
//!
//! - [`pack`]: the deterministic source archive; [`list_files`] and [`read_file`]: the tree and
//!   the file contents a console shows. Both walk by one set of rules (`.gitignore` honored,
//!   `.git` and `.aho` pruned, symlinks never followed); the only difference is that the tree
//!   lists the root `deploy.json`, which the archive cannot contain (the manifest names the
//!   archive's hash).
//! - [`newest_session`], [`session_log`] and [`load_session`]: the aho session log that attests
//!   the work, chain verified.
//! - [`read_manifest`] and [`assemble`]: the draft manifest, `deploy.json` merged over the repo
//!   files merged over the session.
//!
//! Nothing here prints; callers say what was read.

#![forbid(unsafe_code)]

mod archive;
mod assemble;
mod error;
mod session;
mod tree;
mod walk;

pub use archive::{Archive, hash, pack, pack_within};
pub use assemble::{Assembled, assemble, declare_subtree, read_manifest};
pub use error::WorkdirError;
pub use session::{Session, load_session, newest_session, session_log, sessions_dir};
pub use tree::{SNIFF_BYTES, list_files, read_file};

/// The manifest's file name, at the directory root.
pub const MANIFEST: &str = "deploy.json";

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use aho_log::{EntryKind, LogStore, MemoryLog};
    use aho_types::Principal;

    /// A workspace with ignored, private and manifest files around the ones that ship.
    pub(crate) fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        for (f, body) in [
            ("src/main.rs", "fn main() {}"),
            (".gitignore", "target/\n*.log\n/secret.txt\n"),
            ("secret.txt", "x"),
            ("sub/secret.txt", "kept: the pattern is anchored"),
            ("target/debug/app", "bin"),
            ("run.log", "log"),
            (".git/HEAD", "ref"),
            (".aho/sessions/1.jsonl", "{}"),
            ("deploy.json", "{}"),
            (".env", "K=v"),
            (".env.example", "K="),
            ("sub/.gitignore", "local.txt\n"),
            ("sub/local.txt", "x"),
        ] {
            fs::create_dir_all(p.join(f).parent().unwrap()).unwrap();
            fs::write(p.join(f), body).unwrap();
        }
        dir
    }

    /// Writes an aho-format session log of operator notes to `<dir>/.aho/sessions/<id>.jsonl`.
    pub(crate) fn write_session(dir: &Path, id: &str, notes: &[&str]) -> PathBuf {
        let log = MemoryLog::new();
        for n in notes {
            log.append(None, Principal::Operator, EntryKind::Note { text: (*n).to_owned() }).unwrap();
        }
        let mut jsonl = String::new();
        for e in log.read_from(None) {
            jsonl.push_str(&serde_json::to_string(&e).unwrap());
            jsonl.push('\n');
        }
        let path = crate::sessions_dir(dir).join(format!("{id}.jsonl"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, jsonl).unwrap();
        path
    }
}
