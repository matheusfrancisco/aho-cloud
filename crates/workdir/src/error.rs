//! Why a directory could not be packed, listed, read, attested or drafted from.

use std::io;
use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum WorkdirError {
    #[error("{}: {source}", path.display())]
    Io { path: PathBuf, source: io::Error },
    /// Part of the directory could not be walked (permissions, an entry vanishing mid-walk).
    /// Paths in the message are relative to the directory.
    #[error("reading the workspace: {0}")]
    Walk(String),
    /// A name the archive cannot carry; relative to the directory.
    #[error("{} is not UTF-8; rename it or ignore it", .0.display())]
    NotUtf8(PathBuf),
    /// The compressed archive would exceed the cap given to [`crate::pack_within`].
    #[error("the workspace archive is larger than {limit} bytes; ignore build outputs and large files in .gitignore")]
    TooLarge { limit: u64 },
    /// `deploy.json` exists but does not parse as a manifest.
    #[error("{}: not a deploy manifest: {reason}", path.display())]
    Manifest { path: PathBuf, reason: String },
    #[error("{} is not UTF-8", .0.display())]
    SessionNotUtf8(PathBuf),
    #[error("{} has no session id in its name", .0.display())]
    SessionName(PathBuf),
    /// The session log does not verify (tampered, empty, undecodable).
    #[error("session log {}: {source}", path.display())]
    Session { path: PathBuf, source: cloud_draft::Error },
    /// Not a relative, `/`-separated path without `.`, `..`, empty segments, NUL or `\`.
    #[error("`{0}` is not a workspace path (relative, `/`-separated, no `.` or `..` segments)")]
    BadPath(String),
    /// Inside `.git` or `.aho`, which never leave the directory this way.
    #[error("`{0}` is private: `.git` and `.aho` are never listed or served")]
    Private(String),
    /// Resolves (through symlinks) to somewhere outside the directory.
    #[error("`{0}` resolves outside the workspace")]
    Escapes(String),
    #[error("no `{0}` in the workspace")]
    NotFound(String),
    /// A directory, socket, device or FIFO.
    #[error("`{0}` is not a regular file")]
    NotAFile(String),
}

pub(crate) fn io(path: impl Into<PathBuf>) -> impl FnOnce(io::Error) -> WorkdirError {
    let path = path.into();
    move |source| WorkdirError::Io { path, source }
}
