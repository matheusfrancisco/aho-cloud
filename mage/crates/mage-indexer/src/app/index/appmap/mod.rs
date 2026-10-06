//! UC `index.app_map` (knowledge.md "Layer 4: App map"): detect frameworks from dependency
//! manifests, extract app facts for blobs under a framework root (cached per blob at
//! `AppMapRev`), link them into the version's app graph ([`link`]) and replace `app_nodes` /
//! `app_edges`.

pub mod link;

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Arc;
use std::time::Duration;

use mage_core::domain::{BlobOid, CommitSha, EntryKind, FileClass, IndexPriority, Language, Layer, LayerEvent, LayerState, ManifestEntry, OrgId, RepoId, RepoVersionKey};
use mage_core::ports::knowledge::{AppFactRecord, AppGraph, BlobInput, BlobStage, FrameworkId, FrameworkScope, PageReq, ParseLimits};
use mage_core::ports::{Clock, KnowledgeStore, RepoStore};

use self::link::{LinkOptions, link};
use super::layers::{self, Started};
use super::{IndexTarget, JobDone, errors};
use crate::error::AppError;
use crate::ports::git::{GitRepoReader, MirrorRef};
use crate::ports::parser::Parser;

/// Dependency manifests read for framework detection (any depth, not Vendored).
pub const MANIFEST_FILES: &[&str] =
    &["go.mod", "package.json", "deps.edn", "shadow-cljs.edn", "project.clj", "pyproject.toml", "requirements.txt", "Cargo.toml"];

/// Skip reason when no framework root exists.
pub const NO_FRAMEWORK: &str = "no supported framework";

/// Blobs per `Parser::app_facts` call and its byte budget.
const BATCH_BLOBS: usize = 256;
const BATCH_BYTES: u64 = 8 * 1024 * 1024;
/// Larger blobs get no app facts (same cap as parsing).
const MAX_BLOB_BYTES: u64 = 1024 * 1024;
const MANIFEST_PAGE: u32 = 1000;

const APP_LIMITS: ParseLimits =
    ParseLimits { max_bytes: MAX_BLOB_BYTES, timeout: Duration::from_secs(2), max_symbols: 20_000, max_refs: 100_000, max_literals: 5_000 };

/// The `index.app_map` handler.
pub struct IndexAppMap {
    repos: Arc<dyn RepoStore>,
    knowledge: Arc<dyn KnowledgeStore>,
    git: Arc<dyn GitRepoReader>,
    parser: Arc<dyn Parser>,
    clock: Arc<dyn Clock>,
}

/// One blob to extract: its first path, size and the frameworks of every root containing it.
struct Candidate<'m> {
    entry: &'m ManifestEntry,
    frameworks: BTreeSet<FrameworkId>,
}

impl IndexAppMap {
    pub fn new(
        repos: Arc<dyn RepoStore>,
        knowledge: Arc<dyn KnowledgeStore>,
        git: Arc<dyn GitRepoReader>,
        parser: Arc<dyn Parser>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self { repos, knowledge, git, parser, clock }
    }

    pub async fn run(&self, org_id: OrgId, repo_id: RepoId, sha: CommitSha, prio: IndexPriority) -> Result<JobDone, AppError> {
        let t = IndexTarget::new(org_id, repo_id, sha, prio);
        let key = t.key();
        let version = self.repos.version(org_id, key).await?;
        let state = version.layer(Layer::AppMap).cloned().unwrap_or(LayerState::Pending);
        let app_rev = self.parser.app_rev();
        match &state {
            LayerState::Failed { .. } | LayerState::Skipped { .. } => return Ok(JobDone::done()),
            LayerState::Ready { .. } if version.revs.is_some_and(|r| r.appmap == app_rev) => return Ok(JobDone::done()),
            _ => {}
        }
        if !version.layer(Layer::Symbols).is_some_and(LayerState::is_ready) {
            return Err(AppError::Unavailable(format!("symbols not ready at {key}")));
        }

        let manifest = self.manifest(org_id, &key).await?;
        let scopes = self.detect(t.mirror(), &manifest).await?;
        if scopes.is_empty() && state == LayerState::Pending {
            let skip = LayerEvent::Skip { reason: NO_FRAMEWORK.into() };
            self.repos.apply_layer_event(org_id, key, Layer::AppMap, skip, &[]).await?;
            return Ok(JobDone::done());
        }
        let started = layers::start(&*self.repos, org_id, key, Layer::AppMap, Some(&state)).await?;
        if started == Started::Closed {
            return Ok(JobDone::done());
        }

        let graph = if scopes.is_empty() {
            AppGraph::default()
        } else {
            self.extract(org_id, t.mirror(), &manifest, &scopes).await?;
            let facts = self.knowledge.app_facts_at(org_id, &key, app_rev).await?;
            let reframe = scopes.iter().any(|s| s.framework == FrameworkId::ReFrame);
            let outcome = link(&*self.knowledge, org_id, &key, &facts, LinkOptions { reframe }).await?;
            let c = outcome.coverage;
            tracing::info!(
                org = %org_id, repo = %repo_id, sha = %sha,
                endpoints = c.endpoints, endpoints_with_handler = c.endpoints_with_handler,
                client_calls = c.client_calls, client_calls_linked = c.client_calls_linked,
                pages = c.pages, pages_with_component = c.pages_with_component,
                "app map coverage"
            );
            outcome.graph
        };
        self.knowledge.replace_app_graph(org_id, &key, &graph).await?;
        let complete = LayerEvent::Complete { at: self.clock.now() };
        self.repos.apply_layer_event(org_id, key, Layer::AppMap, complete, &[]).await?;
        let current = mage_core::domain::IndexRevs { parser: self.parser.rev(), errors: errors::ERRORS_REV, appmap: app_rev };
        layers::switch_rev(&*self.repos, org_id, key, current, |revs| revs.appmap = app_rev).await?;
        Ok(JobDone::done())
    }

    async fn manifest(&self, org: OrgId, key: &RepoVersionKey) -> Result<Vec<ManifestEntry>, AppError> {
        let mut entries = Vec::new();
        let mut page = PageReq::first(MANIFEST_PAGE);
        loop {
            let got = self.knowledge.manifest_all(org, key, &page).await?;
            entries.extend(got.items);
            match got.next {
                Some(next) => page.after = Some(next),
                None => return Ok(entries),
            }
        }
    }

    /// Step 2: `Parser::detect_frameworks` over the non-vendored dependency manifests.
    async fn detect(&self, mirror: MirrorRef, manifest: &[ManifestEntry]) -> Result<Vec<FrameworkScope>, AppError> {
        let files: Vec<&ManifestEntry> = manifest
            .iter()
            .filter(|e| e.kind == EntryKind::File && e.class != FileClass::Vendored && MANIFEST_FILES.contains(&e.path.file_name()))
            .collect();
        if files.is_empty() {
            return Ok(Vec::new());
        }
        let oids: Vec<BlobOid> = files.iter().map(|e| e.blob).collect();
        let reads = self.git.read_blobs(&mirror, &oids, MAX_BLOB_BYTES).await?;
        let inputs: Vec<BlobInput> = files
            .iter()
            .zip(reads)
            .filter_map(|(entry, read)| {
                let bytes = read.bytes?;
                Some(BlobInput { oid: entry.blob, path: entry.path.clone(), lang: entry.lang.unwrap_or(Language::Other), bytes })
            })
            .collect();
        Ok(self.parser.detect_frameworks(inputs).await?)
    }

    /// Step 3: app facts for blobs under a framework root, skipping blobs already done at this rev.
    async fn extract(&self, org: OrgId, mirror: MirrorRef, manifest: &[ManifestEntry], scopes: &[FrameworkScope]) -> Result<(), AppError> {
        let languages = self.parser.languages();
        let mut by_oid: BTreeMap<BlobOid, Candidate<'_>> = BTreeMap::new();
        let mut order: Vec<BlobOid> = Vec::new();
        for entry in manifest {
            let Some(lang) = entry.lang else { continue };
            if entry.kind != EntryKind::File || entry.class != FileClass::Source || entry.size > MAX_BLOB_BYTES || !languages.contains(&lang) {
                continue;
            }
            let frameworks: BTreeSet<FrameworkId> = scopes
                .iter()
                .filter(|s| s.root.as_ref().is_none_or(|root| entry.path.is_within(root.as_str())))
                .map(|s| s.framework)
                .collect();
            if frameworks.is_empty() {
                continue;
            }
            match by_oid.get_mut(&entry.blob) {
                Some(candidate) => candidate.frameworks.extend(frameworks),
                None => {
                    order.push(entry.blob);
                    by_oid.insert(entry.blob, Candidate { entry, frameworks });
                }
            }
        }
        if order.is_empty() {
            return Ok(());
        }
        let app_rev = self.parser.app_rev();
        let todo: HashSet<BlobOid> = self.knowledge.missing(org, BlobStage::AppFacts(app_rev), &order).await?.into_iter().collect();
        let mut batch: Vec<&Candidate<'_>> = Vec::new();
        let mut batch_bytes = 0u64;
        for oid in order.iter().filter(|oid| todo.contains(*oid)) {
            let Some(candidate) = by_oid.get(oid) else { continue };
            if !batch.is_empty() && (batch.len() == BATCH_BLOBS || batch_bytes + candidate.entry.size > BATCH_BYTES) {
                self.extract_batch(org, mirror, &batch).await?;
                batch.clear();
                batch_bytes = 0;
            }
            batch_bytes += candidate.entry.size;
            batch.push(candidate);
        }
        if !batch.is_empty() {
            self.extract_batch(org, mirror, &batch).await?;
        }
        Ok(())
    }

    async fn extract_batch(&self, org: OrgId, mirror: MirrorRef, batch: &[&Candidate<'_>]) -> Result<(), AppError> {
        let oids: Vec<BlobOid> = batch.iter().map(|c| c.entry.blob).collect();
        let reads = self.git.read_blobs(&mirror, &oids, MAX_BLOB_BYTES).await?;
        let mut inputs = Vec::new();
        let mut results: Vec<(BlobOid, Vec<AppFactRecord>)> = Vec::with_capacity(batch.len());
        let mut parsed_oids = Vec::new();
        for (candidate, read) in batch.iter().zip(reads) {
            let entry = candidate.entry;
            match read.bytes {
                Some(bytes) => {
                    let lang = entry.lang.unwrap_or(Language::Other);
                    inputs.push((BlobInput { oid: entry.blob, path: entry.path.clone(), lang, bytes }, candidate.frameworks.iter().copied().collect()));
                    parsed_oids.push(entry.blob);
                }
                // Too large to extract: the empty marker keeps it from being retried at this rev.
                None => results.push((entry.blob, Vec::new())),
            }
        }
        let facts = self.parser.app_facts(inputs, APP_LIMITS).await;
        for (oid, result) in parsed_oids.into_iter().zip(facts) {
            match result {
                Ok(records) => results.push((oid, records)),
                Err(err) => {
                    tracing::debug!(%oid, error = %err, "app facts failed for blob");
                    results.push((oid, Vec::new()));
                }
            }
        }
        self.knowledge.put_app_facts(org, self.parser.app_rev(), &results).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
