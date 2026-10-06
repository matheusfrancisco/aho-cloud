//! `index.*` pipeline per commit (knowledge.md "Use cases"): which shas get indexed, the job DAG
//! (`index.version` → `index.fetch` → `index.manifest` / `index.history` / `index.bundle_source`;
//! `index.parse` batches → `index.error_sites` / `index.app_map`), blob-hash caching, layer state
//! transitions, SourceBundle, reconcile and GC.
//!
//! Every handler is idempotent: a retried job re-reads state and skips done work. Successors are
//! enqueued either through the store outbox (`jobs` arguments of `RepoStore` methods, same
//! transaction as the final write) or as follow-ups of `JobQueue::complete` ([`JobDone::Complete`]).
//! `appmap`, `history` and `ingest_scip` are IndexerGraph's use cases; the worker dispatches to them.

pub mod bundle_source;
pub mod error_sites;
pub mod errors;
pub mod fetch;
pub mod gc;
pub mod layers;
pub mod manifest;
pub mod parse;
pub mod reconcile;
pub mod version;

#[cfg(test)]
mod tests;
pub mod appmap;
pub mod history;
pub mod ingest_scip;

use std::path::PathBuf;
use std::sync::Arc;

use mage_core::domain::{CommitSha, IndexPriority, IndexRevs, JobPayload, OrgId, RepoId, RepoVersionKey};
use mage_core::ports::jobs::NewJob;
use mage_core::ports::{ArtifactStore, Clock, IdGen, JobQueue, KnowledgeStore, RepoStore, SourceHost};
use serde::Deserialize;
use time::OffsetDateTime;

use crate::app::live::LiveService;
use crate::ports::git::{GitRepoReader, MirrorRef};
use crate::ports::parser::Parser;

/// How a handled job ends (the worker maps it onto `JobQueue::complete` / `defer`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobDone {
    /// Mark succeeded and enqueue these follow-ups in the same transaction.
    Complete(Vec<NewJob>),
    /// Requeue at this time without consuming the attempt.
    Defer(OffsetDateTime),
}

impl JobDone {
    pub fn done() -> Self {
        Self::Complete(Vec::new())
    }
}

/// `[indexer]` settings the pipeline reads.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct IndexConfig {
    /// `{data_dir}/tmp` stages bundles.
    pub data_dir: PathBuf,
    /// `indexer.bundle_max_bytes`: larger bundles fail permanently.
    #[serde(default = "default_bundle_max_bytes")]
    pub bundle_max_bytes: u64,
    /// Rev rebuilds `index.reconcile` enqueues per tick.
    #[serde(default = "default_rebuilds_per_tick")]
    pub rebuilds_per_tick: u32,
}

fn default_bundle_max_bytes() -> u64 {
    2 * 1024 * 1024 * 1024
}

fn default_rebuilds_per_tick() -> u32 {
    20
}

impl IndexConfig {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self { data_dir: data_dir.into(), bundle_max_bytes: default_bundle_max_bytes(), rebuilds_per_tick: default_rebuilds_per_tick() }
    }
}

/// Ports the pipeline drives.
#[derive(Clone)]
pub struct IndexDeps {
    pub repos: Arc<dyn RepoStore>,
    /// Dead-letter listing (`index.reconcile`).
    pub jobs: Arc<dyn JobQueue>,
    pub knowledge: Arc<dyn KnowledgeStore>,
    pub artifacts: Arc<dyn ArtifactStore>,
    pub source: Arc<dyn SourceHost>,
    pub git: Arc<dyn GitRepoReader>,
    pub parser: Arc<dyn Parser>,
    pub clock: Arc<dyn Clock>,
    pub ids: Arc<dyn IdGen>,
}

/// The `index.*` handlers owned by this module (one method per job kind, each in its module).
pub struct IndexPipeline {
    deps: IndexDeps,
    config: IndexConfig,
    /// Checkout cache warmed after `index.manifest` for DefaultHead / Deployed versions.
    checkouts: Option<Arc<LiveService>>,
}

impl IndexPipeline {
    pub fn new(deps: IndexDeps, config: IndexConfig) -> Self {
        Self { deps, config, checkouts: None }
    }

    /// Materialize DefaultHead and Deployed versions eagerly after `index.manifest`
    /// (knowledge.md "Text search on Cloud Run", step 3).
    pub fn with_checkouts(mut self, live: Arc<LiveService>) -> Self {
        self.checkouts = Some(live);
        self
    }

    /// Revisions this deployment writes facts at.
    pub fn current_revs(&self) -> IndexRevs {
        IndexRevs { parser: self.deps.parser.rev(), errors: errors::ERRORS_REV, appmap: self.deps.parser.app_rev() }
    }
}

/// `(org, repo, sha, prio)` of one commit's DAG job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IndexTarget {
    pub org: OrgId,
    pub repo: RepoId,
    pub sha: CommitSha,
    pub prio: IndexPriority,
}

impl IndexTarget {
    pub const fn new(org: OrgId, repo: RepoId, sha: CommitSha, prio: IndexPriority) -> Self {
        Self { org, repo, sha, prio }
    }

    pub const fn key(&self) -> RepoVersionKey {
        RepoVersionKey::new(self.repo, self.sha)
    }

    pub const fn mirror(&self) -> MirrorRef {
        MirrorRef::new(self.org, self.repo)
    }

    pub fn fetch_job(&self) -> NewJob {
        NewJob::new(JobPayload::IndexFetch { org_id: self.org, repo_id: self.repo, sha: self.sha, prio: self.prio })
    }

    pub fn manifest_job(&self) -> NewJob {
        NewJob::new(JobPayload::IndexManifest { org_id: self.org, repo_id: self.repo, sha: self.sha, prio: self.prio })
    }

    pub fn history_job(&self) -> NewJob {
        NewJob::new(JobPayload::IndexHistory { org_id: self.org, repo_id: self.repo, sha: self.sha, prio: self.prio })
    }

    pub fn bundle_job(&self) -> NewJob {
        NewJob::new(JobPayload::IndexBundleSource { org_id: self.org, repo_id: self.repo, sha: self.sha, prio: self.prio })
    }

    pub fn parse_job(&self, batch: u32, oids: Vec<mage_core::domain::BlobOid>) -> NewJob {
        NewJob::new(JobPayload::IndexParse { org_id: self.org, repo_id: self.repo, sha: self.sha, prio: self.prio, batch, oids })
    }

    pub fn error_sites_job(&self) -> NewJob {
        NewJob::new(JobPayload::IndexErrorSites { org_id: self.org, repo_id: self.repo, sha: self.sha, prio: self.prio })
    }

    pub fn app_map_job(&self) -> NewJob {
        NewJob::new(JobPayload::IndexAppMap { org_id: self.org, repo_id: self.repo, sha: self.sha, prio: self.prio })
    }

    /// Successors of the last `index.parse` batch (or of a manifest with nothing to parse).
    pub fn after_symbols(&self) -> Vec<NewJob> {
        vec![self.error_sites_job(), self.app_map_job()]
    }

    /// `runs.source_ready` (dedupe `runs.source_ready:{repo_id}:{sha}`).
    pub fn source_ready_job(&self) -> NewJob {
        NewJob::new(JobPayload::RunsSourceReady { org_id: self.org, repo_id: self.repo, sha: self.sha })
    }
}

