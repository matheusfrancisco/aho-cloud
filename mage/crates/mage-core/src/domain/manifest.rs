//! Manifest entries (knowledge.md "Shared types"). Defined in the domain because the evidence
//! gate's `ManifestView` is built from them; `mage_core::ports::knowledge` re-exports them.

use serde::{Deserialize, Serialize};

use crate::domain::ids::BlobOid;
use crate::domain::path::{FileClass, Language, RepoPath};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    File,
    Symlink,
    Submodule,
}

/// One tree entry at a commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestEntry {
    pub path: RepoPath,
    pub kind: EntryKind,
    pub executable: bool,
    /// For `Submodule`: the gitlink commit id bytes.
    pub blob: BlobOid,
    pub lang: Option<Language>,
    pub class: FileClass,
    pub size: u64,
    pub is_text: bool,
    /// 0 when `!is_text` or `Submodule`.
    pub line_count: u32,
}
