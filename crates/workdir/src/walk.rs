//! The one walk the archive and the file tree share, so the console shows what a deploy ships.
//!
//! Kept: everything `.gitignore` (and `.git/info/exclude`) does not ignore, whether or not the
//! directory is a git repository. Pruned at any depth: `.git`, and `.aho` (session logs travel
//! separately, verified). Symlinks are entries of their own and are never followed. Siblings come
//! in byte order of their names, depth first, so paths are ordered component by component
//! (`src`, `src/main.rs`, `src.rs`).

use std::path::{MAIN_SEPARATOR, Path};

use cloud_api::FileKind;
use ignore::WalkBuilder;

use crate::error::WorkdirError;

/// Names that are pruned from every walk and refused by [`crate::read_file`].
pub(crate) const PRIVATE: [&str; 2] = [".git", ".aho"];

pub(crate) struct Entry {
    pub dent: ignore::DirEntry,
    /// `/`-separated, relative to the root.
    pub rel: String,
    pub kind: FileKind,
}

/// Every kept entry under `root` (canonical), the root itself excluded.
pub(crate) fn walk(root: &Path) -> impl Iterator<Item = Result<Entry, WorkdirError>> + '_ {
    WalkBuilder::new(root)
        .hidden(false)
        .ignore(false)
        .parents(false)
        .git_global(false)
        .git_ignore(true)
        .git_exclude(true)
        .require_git(false)
        .follow_links(false)
        .sort_by_file_name(|a, b| a.cmp(b))
        .filter_entry(|e| !e.file_name().to_str().is_some_and(|n| PRIVATE.contains(&n)))
        .build()
        .filter_map(move |item| {
            let dent = match item {
                Ok(d) => d,
                Err(e) => return Some(Err(walk_error(root, &e))),
            };
            if dent.depth() == 0 {
                return None;
            }
            let file_type = dent.file_type()?;
            let kind = if file_type.is_dir() {
                FileKind::Dir
            } else if file_type.is_symlink() {
                FileKind::Symlink
            } else {
                FileKind::File
            };
            Some(relative(root, dent.path()).map(|rel| Entry { dent, rel, kind }))
        })
}

fn relative(root: &Path, path: &Path) -> Result<String, WorkdirError> {
    let rel = path.strip_prefix(root).map_err(|_| WorkdirError::Walk(format!("{} is outside the workspace", path.display())))?;
    let name = rel.to_str().ok_or_else(|| WorkdirError::NotUtf8(rel.to_owned()))?;
    Ok(name.replace(MAIN_SEPARATOR, "/"))
}

/// The walker's message with `root` taken out of its paths.
pub(crate) fn walk_error(root: &Path, e: &ignore::Error) -> WorkdirError {
    WorkdirError::Walk(e.to_string().replace(&format!("{}{MAIN_SEPARATOR}", root.display()), ""))
}
