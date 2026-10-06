//! Builds the evidence gate's `ManifestView` (triage.md "Evidence gate algorithm": one
//! `manifest_entries` query for the cited paths, then `SourceReader` full-file text for each text
//! entry). Shared with fix (brief re-check) so every evidence check sees the text `read_file`
//! showed the agent.

use std::collections::BTreeSet;

use mage_core::domain::evidence::ManifestView;
use mage_core::domain::{EntryKind, OrgId, RepoPath, RepoVersionKey};
use mage_core::ports::KnowledgeStore;

use crate::app::knowledge_query::{SourceReadError, SourceReader};
use crate::app::triage::knowledge_error;
use crate::error::AppError;

/// Manifest entries of `paths` at `key` plus the full text of every stored text file among them.
///
/// Missing paths are simply absent (the check reports `PathNotFound`); text the reader refuses
/// (`NotText`, `TooLarge`) is left out, so the check reports `NotText`. Only an unavailable store
/// fails the call: the gate must not consume an attempt on infrastructure errors.
pub async fn load_manifest_view(
    knowledge: &dyn KnowledgeStore,
    reader: &SourceReader,
    org: OrgId,
    key: RepoVersionKey,
    paths: &[RepoPath],
) -> Result<ManifestView, AppError> {
    let distinct: Vec<RepoPath> = paths.iter().cloned().collect::<BTreeSet<_>>().into_iter().collect();
    if distinct.is_empty() {
        return Ok(ManifestView::new(key, Vec::new(), Vec::new()));
    }
    let entries = knowledge.manifest_entries(org, &key, &distinct).await.map_err(knowledge_error)?;
    let mut loaded = BTreeSet::new();
    let mut texts = Vec::new();
    for entry in &entries {
        if entry.kind != EntryKind::File || !entry.is_text || entry.line_count == 0 || loaded.contains(&entry.blob) {
            continue;
        }
        match reader.read_all(org, &key, &entry.path).await {
            Ok(source) => {
                loaded.insert(source.blob);
                texts.push((source.blob, source.lines));
            }
            Err(SourceReadError::NotFound | SourceReadError::NotText | SourceReadError::TooLarge | SourceReadError::OutOfRange { .. }) => {}
            Err(SourceReadError::Unavailable(message)) => return Err(AppError::Unavailable(message)),
        }
    }
    Ok(ManifestView::new(key, entries, texts))
}
