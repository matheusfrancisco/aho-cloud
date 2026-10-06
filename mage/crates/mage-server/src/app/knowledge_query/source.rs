//! `SourceReader` (knowledge.md "`SourceReader` (shared with the evidence gate)"): the one place
//! the server turns a manifest path into lines, so `read_file` and the triage evidence gate see
//! identical text.
//!
//! `manifest_entries([path])` → `NotFound` | Submodule / `!is_text` / content not stored (binary)
//! → `NotText` | `size > 2 MiB` → `TooLarge` → `ArtifactStore::get(blob_content_key(org, blob))`
//! → split on `\n` (trailing `\r` stripped), UTF-8 lossy → slice. A trailing newline does not
//! start a new line, matching the indexer's `line_count`.

use std::sync::Arc;

use mage_core::domain::{BlobOid, EntryKind, LineRange, ManifestEntry, OrgId, RepoPath, RepoVersionKey};
use mage_core::ports::blob::{ArtifactStore, BlobError, BlobKey};
use mage_core::ports::knowledge::{KnowledgeError, KnowledgeStore};

use crate::error::AppError;

/// Largest blob whose content is stored and readable (knowledge.md "UC `index.manifest`").
pub const MAX_TEXT_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceLines {
    pub blob: BlobOid,
    pub total_lines: u32,
    /// Line number of `lines[0]` (1-based).
    pub first_line: u32,
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SourceReadError {
    #[error("path not found at this version")]
    NotFound,
    /// Submodule, symlink, binary, or content not stored.
    #[error("not a text file")]
    NotText,
    #[error("file larger than 2 MiB")]
    TooLarge,
    /// `start` beyond the last line.
    #[error("line range outside the file ({total_lines} lines)")]
    OutOfRange { total_lines: u32 },
    #[error("source unavailable: {0}")]
    Unavailable(String),
}

impl From<SourceReadError> for AppError {
    fn from(e: SourceReadError) -> Self {
        match e {
            SourceReadError::NotFound => AppError::NotFound("path not found at this version".into()),
            SourceReadError::NotText => AppError::Invalid("not a text file".into()),
            SourceReadError::TooLarge => AppError::Invalid("file larger than 2 MiB".into()),
            SourceReadError::OutOfRange { total_lines } => AppError::Invalid(format!("line range outside the file ({total_lines} lines)")),
            SourceReadError::Unavailable(m) => AppError::Unavailable(m),
        }
    }
}

fn store_error(e: KnowledgeError) -> SourceReadError {
    match e {
        KnowledgeError::NotFound => SourceReadError::NotFound,
        other => SourceReadError::Unavailable(other.to_string()),
    }
}

/// Text of a stored blob as lines; `\r\n` and `\n` both end a line, a final newline adds none.
pub fn split_lines(bytes: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(bytes);
    let body = text.strip_suffix('\n').unwrap_or(&text);
    if body.is_empty() && text.is_empty() {
        return Vec::new();
    }
    body.split('\n').map(|line| line.strip_suffix('\r').unwrap_or(line).to_owned()).collect()
}

#[derive(Clone)]
pub struct SourceReader {
    knowledge: Arc<dyn KnowledgeStore>,
    artifacts: Arc<dyn ArtifactStore>,
}

impl SourceReader {
    /// `artifacts` should be the LRU-decorated store the server composes (blob objects are
    /// immutable, so cached reads never go stale).
    pub fn new(knowledge: Arc<dyn KnowledgeStore>, artifacts: Arc<dyn ArtifactStore>) -> Self {
        Self { knowledge, artifacts }
    }

    /// The manifest entry of `path`, or `NotFound`.
    pub async fn entry(&self, org: OrgId, key: &RepoVersionKey, path: &RepoPath) -> Result<ManifestEntry, SourceReadError> {
        let entries = self.knowledge.manifest_entries(org, key, std::slice::from_ref(path)).await.map_err(store_error)?;
        entries.into_iter().find(|e| &e.path == path).ok_or(SourceReadError::NotFound)
    }

    /// Raw content of a stored blob (text blobs ≤ 2 MiB); symlink targets are stored the same way.
    pub async fn blob_bytes(&self, org: OrgId, oid: BlobOid) -> Result<Vec<u8>, SourceReadError> {
        self.artifacts.get(&BlobKey::blob_content(org, oid), MAX_TEXT_BYTES).await.map_err(|e| match e {
            BlobError::NotFound => SourceReadError::NotText,
            BlobError::TooLarge { .. } => SourceReadError::TooLarge,
            other => SourceReadError::Unavailable(other.to_string()),
        })
    }

    /// Every line of a text file.
    pub async fn read_all(&self, org: OrgId, key: &RepoVersionKey, path: &RepoPath) -> Result<SourceLines, SourceReadError> {
        let (entry, lines) = self.load(org, key, path).await?;
        let total_lines = u32::try_from(lines.len()).unwrap_or(u32::MAX);
        Ok(SourceLines { blob: entry.blob, total_lines, first_line: 1, lines })
    }

    /// Lines `lines.start..=min(lines.end, total)`; a start beyond the last line is `OutOfRange`.
    pub async fn read_lines(&self, org: OrgId, key: &RepoVersionKey, path: &RepoPath, lines: LineRange) -> Result<SourceLines, SourceReadError> {
        let (entry, all) = self.load(org, key, path).await?;
        let total_lines = u32::try_from(all.len()).unwrap_or(u32::MAX);
        if lines.start() > total_lines {
            return Err(SourceReadError::OutOfRange { total_lines });
        }
        let end = lines.end().min(total_lines);
        let slice = all[(lines.start() - 1) as usize..end as usize].to_vec();
        Ok(SourceLines { blob: entry.blob, total_lines, first_line: lines.start(), lines: slice })
    }

    async fn load(&self, org: OrgId, key: &RepoVersionKey, path: &RepoPath) -> Result<(ManifestEntry, Vec<String>), SourceReadError> {
        let entry = self.entry(org, key, path).await?;
        if entry.kind != EntryKind::File || !entry.is_text {
            return Err(SourceReadError::NotText);
        }
        if entry.size > MAX_TEXT_BYTES {
            return Err(SourceReadError::TooLarge);
        }
        let bytes = self.blob_bytes(org, entry.blob).await?;
        Ok((entry, split_lines(&bytes)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_lines_matches_line_count_rules() {
        assert_eq!(split_lines(b""), Vec::<String>::new());
        assert_eq!(split_lines(b"a\nb\n"), vec!["a", "b"]);
        assert_eq!(split_lines(b"a\r\nb"), vec!["a", "b"]);
        assert_eq!(split_lines(b"\n"), vec![""]);
        assert_eq!(split_lines(b"a\n\nb\n"), vec!["a", "", "b"]);
        assert_eq!(split_lines(&[0x66, 0xff, b'\n']), vec!["f\u{fffd}"]);
    }
}
