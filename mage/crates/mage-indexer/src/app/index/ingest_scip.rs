//! UC `index.ingest_scip` (knowledge.md "Layer 2: Refs"): download a Learn run's `ScipIndex`
//! artifact, stream its documents, keep the ones that map to a manifest blob at the run's sha,
//! and store occurrences and symbols keyed by the artifact. Readers at other shas use the newest
//! index restricted to documents whose blob is unchanged (`KnowledgeStore::scip_occurrences`),
//! which is why every row carries the manifest blob oid.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use mage_core::domain::{ArtifactId, CommitSha, EntryKind, Layer, LayerEvent, LayerState, OrgId, RepoId, RepoPath, RepoVersionKey, RunId};
use mage_core::ports::blob::BlobKey;
use mage_core::ports::knowledge::{ManifestEntry, ScipDocRows, ScipDocument, ScipSymbolInfo};
use mage_core::ports::repos::RepoStoreError;
use mage_core::ports::{ArtifactStore, Clock, KnowledgeStore, RepoStore};

use super::JobDone;
use crate::error::AppError;
use crate::ports::scip::{ScipLimits, ScipReader};

/// Larger artifacts are refused (knowledge.md: cap 1 GiB).
pub const MAX_ARTIFACT_BYTES: u64 = 1024 * 1024 * 1024;
/// Documents read per batch.
pub const DOC_BATCH: usize = 500;

/// The `index.ingest_scip` handler.
pub struct IngestScip {
    repos: Arc<dyn RepoStore>,
    knowledge: Arc<dyn KnowledgeStore>,
    artifacts: Arc<dyn ArtifactStore>,
    scip: Arc<dyn ScipReader>,
    clock: Arc<dyn Clock>,
    /// `{indexer.data_dir}/scip`.
    scip_dir: PathBuf,
}

/// Removes the downloaded file however the ingest ends.
struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        if let Err(err) = std::fs::remove_file(&self.0) {
            if err.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(path = %self.0.display(), error = %err, "could not delete scip download");
            }
        }
    }
}

impl IngestScip {
    /// `data_dir` = `indexer.data_dir`; downloads go to `{data_dir}/scip/{artifact_id}.scip`.
    pub fn new(
        repos: Arc<dyn RepoStore>,
        knowledge: Arc<dyn KnowledgeStore>,
        artifacts: Arc<dyn ArtifactStore>,
        scip: Arc<dyn ScipReader>,
        clock: Arc<dyn Clock>,
        data_dir: &Path,
    ) -> Self {
        Self { repos, knowledge, artifacts, scip, clock, scip_dir: data_dir.join("scip") }
    }

    pub async fn run(&self, org_id: OrgId, repo_id: RepoId, sha: CommitSha, run_id: RunId, artifact_id: ArtifactId, key: &str) -> Result<JobDone, AppError> {
        let version_key = RepoVersionKey::new(repo_id, sha);
        // Step 1: the run's sha is indexed before the run is assigned; until Files is Ready, retry.
        let version = match self.repos.version(org_id, version_key).await {
            Ok(version) => version,
            Err(RepoStoreError::NotFound) => return Err(AppError::Unavailable(format!("version {version_key} not indexed yet"))),
            Err(err) => return Err(err.into()),
        };
        if !version.layer(Layer::Files).is_some_and(LayerState::is_ready) {
            return Err(AppError::Unavailable(format!("files not ready at {version_key}")));
        }
        let object = BlobKey::parse(key).map_err(|e| AppError::Invalid(format!("scip artifact key: {e}")))?;
        if object.org() != Some(org_id) {
            return Err(AppError::Invalid(format!("scip artifact key {key} is outside org {org_id}")));
        }

        // Step 2: start the layer (a Learn run produced refs, so a skipped/failed layer reopens).
        match version.layer(Layer::Refs).cloned().unwrap_or(LayerState::Pending) {
            LayerState::Ready { .. } => {}
            LayerState::Pending | LayerState::Running => {
                self.repos.apply_layer_event(org_id, version_key, Layer::Refs, LayerEvent::Start, &[]).await?;
            }
            LayerState::Failed { .. } | LayerState::Skipped { .. } => {
                self.repos.apply_layer_event(org_id, version_key, Layer::Refs, LayerEvent::Reset, &[]).await?;
                self.repos.apply_layer_event(org_id, version_key, Layer::Refs, LayerEvent::Start, &[]).await?;
            }
        }
        let meta = self.artifacts.head(&object).await?;
        if meta.bytes > MAX_ARTIFACT_BYTES {
            return Err(AppError::Invalid(format!("scip artifact is {} bytes (max {MAX_ARTIFACT_BYTES})", meta.bytes)));
        }
        tokio::fs::create_dir_all(&self.scip_dir).await.map_err(|e| AppError::Unavailable(format!("scip dir: {e}")))?;
        let file = TempFile(self.scip_dir.join(format!("{artifact_id}.scip")));
        self.artifacts.get_to_file(&object, &file.0).await?;

        // Steps 3-5: stream documents, map them to manifest blobs, store rows.
        let mut stream = self.scip.open(&file.0, ScipLimits::INGEST).await?;
        self.knowledge.begin_scip_index(org_id, &version_key, artifact_id, stream.metadata()).await?;
        let mut languages = BTreeSet::new();
        let mut documents = 0u32;
        loop {
            let batch = stream.next_documents(DOC_BATCH).await?;
            if batch.is_empty() {
                break;
            }
            let paths: Vec<RepoPath> = batch.iter().filter_map(|d| doc_path(&d.relative_path)).collect();
            let manifest: HashMap<RepoPath, ManifestEntry> = self
                .knowledge
                .manifest_entries(org_id, &version_key, &paths)
                .await?
                .into_iter()
                .filter(|e| e.kind == EntryKind::File)
                .map(|e| (e.path.clone(), e))
                .collect();
            let mut rows = Vec::new();
            let mut symbols = Vec::new();
            for mut doc in batch {
                let Some(row) = doc_rows(&mut doc, &manifest) else { continue };
                if !doc.language.is_empty() {
                    languages.insert(doc.language);
                }
                symbols.extend(doc.symbols.into_iter().filter(|s| !is_local(&s.symbol)));
                rows.push(row);
            }
            documents += rows.len() as u32;
            if !rows.is_empty() {
                self.knowledge.put_scip_documents(org_id, artifact_id, &rows).await?;
            }
            if !symbols.is_empty() {
                self.knowledge.put_scip_symbols(org_id, artifact_id, &symbols).await?;
            }
        }
        let external: Vec<ScipSymbolInfo> = stream.external_symbols().await?.into_iter().filter(|s| !is_local(&s.symbol)).collect();
        if !external.is_empty() {
            self.knowledge.put_scip_symbols(org_id, artifact_id, &external).await?;
        }
        let languages: Vec<String> = languages.into_iter().collect();
        self.knowledge.finish_scip_index(org_id, artifact_id, &languages, documents).await?;

        // Step 6.
        let complete = LayerEvent::Complete { at: self.clock.now() };
        self.repos.apply_layer_event(org_id, version_key, Layer::Refs, complete, &[]).await?;
        tracing::info!(org = %org_id, repo = %repo_id, sha = %sha, run = %run_id, artifact = %artifact_id, documents, "scip index ingested");
        drop(file);
        Ok(JobDone::done())
    }
}

/// `local N` symbols are document-scoped and never stored.
pub fn is_local(symbol: &str) -> bool {
    symbol.starts_with("local ")
}

/// A SCIP `relative_path` as a repo path: `./` prefixes dropped; absolute paths, `..`, and `.git`
/// components refused.
pub fn doc_path(relative: &str) -> Option<RepoPath> {
    let mut rest = relative;
    while let Some(stripped) = rest.strip_prefix("./") {
        rest = stripped;
    }
    let path = RepoPath::new(rest).ok()?;
    if path.segments().any(|s| s == ".git") {
        return None;
    }
    Some(path)
}

/// The rows of one document mapped onto its manifest blob, or `None` when the document is not a
/// file of the manifest (generated files, dependencies). Local symbols and occurrences outside
/// the blob's lines are dropped.
fn doc_rows(doc: &mut ScipDocument, manifest: &HashMap<RepoPath, ManifestEntry>) -> Option<ScipDocRows> {
    let path = doc_path(&doc.relative_path)?;
    let entry = manifest.get(&path)?;
    let lines = entry.line_count;
    let mut occurrences = std::mem::take(&mut doc.occurrences);
    occurrences.retain(|o| !is_local(&o.symbol) && o.range.start_line >= 1 && o.range.end_line <= lines);
    Some(ScipDocRows { path, blob: entry.blob, occurrences })
}

#[cfg(test)]
mod tests;
