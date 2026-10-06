//! Indexer-side use case of the internal API `/internal/v1` (knowledge.md "Text search, blame,
//! and diff on Cloud Run"): live text search over materialized checkouts, blame, diff and log
//! on the mirror. Token verification and the claims-vs-body check happen in the driving adapter
//! (`adapters::driving::http`) before these methods run.

pub mod checkout;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use mage_core::domain::{CommitSha, FileClass, Layer, LayerState, OrgId, RepoId, RepoPath, RepoVersionKey};
use mage_core::ports::repos::RepoStoreError;
use mage_core::ports::{KnowledgeStore, RepoStore};
use mage_core::wire::indexer::{
    BlameRequest, BlameResponse, DiffRequest, DiffResponse, HealthResponse, LogRequest, LogResponse, MAX_DIFF_BYTES_PER_FILE, MAX_DIFF_PATHS,
    MAX_LOG_COMMITS, SearchRequest, TextHit, TextHits, Truncation,
};
use serde::Deserialize;
use tokio::sync::Semaphore;

pub use self::checkout::{CheckoutCache, CheckoutLease};
use crate::error::AppError;
use crate::ports::git::{GitRepoReader, MirrorRef};
use crate::ports::search::{Checkout, TextSearch};

/// `[indexer]` settings of the internal API use case.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct LiveConfig {
    /// `indexer.data_dir`: checkouts live in `{data_dir}/checkouts`.
    pub data_dir: PathBuf,
    /// `indexer.checkout_cache_bytes` (default 20 GiB).
    #[serde(default = "default_checkout_cache_bytes")]
    pub checkout_cache_bytes: u64,
    /// Searches in flight; excess requests get `Unavailable` (503).
    #[serde(default = "default_max_searches")]
    pub max_searches: usize,
}

fn default_checkout_cache_bytes() -> u64 {
    20 * 1024 * 1024 * 1024
}

fn default_max_searches() -> usize {
    4
}

impl LiveConfig {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self { data_dir: data_dir.into(), checkout_cache_bytes: default_checkout_cache_bytes(), max_searches: default_max_searches() }
    }
}

/// Classes dropped from search hits unless `include_generated`.
const GENERATED: [FileClass; 3] = [FileClass::Generated, FileClass::Vendored, FileClass::Binary];

pub struct LiveService {
    repos: Arc<dyn RepoStore>,
    knowledge: Arc<dyn KnowledgeStore>,
    git: Arc<dyn GitRepoReader>,
    search: Arc<dyn TextSearch>,
    checkouts: CheckoutCache,
    searches: Semaphore,
}

impl LiveService {
    /// Opens the checkout cache (registering checkouts left by an earlier process).
    pub fn new(
        repos: Arc<dyn RepoStore>,
        knowledge: Arc<dyn KnowledgeStore>,
        git: Arc<dyn GitRepoReader>,
        search: Arc<dyn TextSearch>,
        config: &LiveConfig,
    ) -> Self {
        Self {
            repos,
            knowledge,
            git,
            search,
            checkouts: CheckoutCache::open(config.data_dir.join("checkouts"), config.checkout_cache_bytes),
            searches: Semaphore::new(config.max_searches.max(1)),
        }
    }

    /// The version must exist with `Files` Ready.
    async fn require_files(&self, org: OrgId, key: RepoVersionKey) -> Result<(), AppError> {
        let version = match self.repos.version(org, key).await {
            Ok(version) => version,
            Err(RepoStoreError::NotFound) => return Err(AppError::NotFound(format!("version {key} is not indexed"))),
            Err(err) => return Err(err.into()),
        };
        if !version.layer(Layer::Files).is_some_and(LayerState::is_ready) {
            return Err(AppError::Unavailable(format!("files are not ready at {key}")));
        }
        Ok(())
    }

    async fn require_commits(&self, mirror: &MirrorRef, shas: &[CommitSha]) -> Result<(), AppError> {
        for sha in shas {
            if !self.git.has_commit(mirror, *sha).await? {
                return Err(AppError::NotFound(format!("commit {sha} is not in the mirror")));
            }
        }
        Ok(())
    }

    /// Materialize (or reuse) the checkout of a version, e.g. eagerly after `index.manifest`.
    pub async fn checkout(&self, org: OrgId, repo: RepoId, sha: CommitSha) -> Result<CheckoutLease, AppError> {
        self.checkouts.lease(&*self.git, org, repo, sha).await
    }

    /// `POST /internal/v1/search`: hits in Generated/Vendored/Binary files are dropped unless
    /// `include_generated`, and the page is refilled from the following hits.
    pub async fn search(&self, req: &SearchRequest) -> Result<TextHits, AppError> {
        req.query.check_limits().map_err(AppError::Invalid)?;
        let key = RepoVersionKey::new(req.repo_id, req.sha);
        self.require_files(req.org_id, key).await?;
        let _permit = self.searches.try_acquire().map_err(|_| AppError::Unavailable("search concurrency exhausted".into()))?;
        let lease = self.checkout(req.org_id, req.repo_id, req.sha).await?;
        let checkout = Checkout { root: lease.path().to_path_buf(), sha: req.sha };
        if req.include_generated {
            return Ok(self.search.search(&checkout, &req.query).await?);
        }

        let deadline = Instant::now() + req.query.timeout;
        let max = req.query.max_hits as usize;
        let mut query = req.query.clone();
        let mut kept: Vec<TextHit> = Vec::new();
        let mut files_scanned = 0u32;
        loop {
            query.timeout = deadline.saturating_duration_since(Instant::now());
            let page = self.search.search(&checkout, &query).await?;
            files_scanned += page.files_scanned;
            let dropped = self.generated_paths(req.org_id, &key, &page.hits).await?;
            let mut hits = page.hits.into_iter().filter(|h| !dropped.contains(&h.path)).peekable();
            while let Some(hit) = hits.next() {
                kept.push(hit);
                if kept.len() == max {
                    let more = hits.peek().is_some() || page.next.is_some();
                    let next = more.then(|| kept.last().map(|h| (h.path.clone(), h.line))).flatten();
                    let truncated = more.then_some(Truncation::Limit);
                    return Ok(TextHits { hits: kept, next, truncated, files_scanned });
                }
            }
            match (page.truncated, page.next) {
                (Some(Truncation::Limit), Some(next)) if Instant::now() < deadline => query.after = Some(next),
                (Some(Truncation::Limit), Some(next)) => {
                    return Ok(TextHits { hits: kept, next: Some(next), truncated: Some(Truncation::Timeout), files_scanned });
                }
                (truncated, next) => return Ok(TextHits { hits: kept, next, truncated, files_scanned }),
            }
        }
    }

    /// Hit paths whose manifest class is Generated, Vendored or Binary.
    async fn generated_paths(&self, org: OrgId, key: &RepoVersionKey, hits: &[TextHit]) -> Result<Vec<RepoPath>, AppError> {
        let mut paths: Vec<RepoPath> = hits.iter().map(|h| h.path.clone()).collect();
        paths.dedup();
        if paths.is_empty() {
            return Ok(paths);
        }
        let entries = self.knowledge.manifest_entries(org, key, &paths).await?;
        Ok(entries.into_iter().filter(|e| GENERATED.contains(&e.class)).map(|e| e.path).collect())
    }

    /// `POST /internal/v1/blame` (at most 200 lines).
    pub async fn blame(&self, req: &BlameRequest) -> Result<BlameResponse, AppError> {
        let lines = req.lines().map_err(AppError::Invalid)?;
        self.require_files(req.org_id, RepoVersionKey::new(req.repo_id, req.sha)).await?;
        let hunks = self.git.blame(&MirrorRef::new(req.org_id, req.repo_id), req.sha, &req.path, lines).await?;
        Ok(BlameResponse { hunks })
    }

    /// `POST /internal/v1/diff` (at most 20 paths, 8 KiB per file); both shas in the mirror.
    pub async fn diff(&self, req: &DiffRequest) -> Result<DiffResponse, AppError> {
        if req.opts.paths.len() > MAX_DIFF_PATHS {
            return Err(AppError::Invalid(format!("{} paths (max {MAX_DIFF_PATHS})", req.opts.paths.len())));
        }
        if req.opts.max_bytes_per_file > MAX_DIFF_BYTES_PER_FILE {
            return Err(AppError::Invalid(format!("max_bytes_per_file {} (max {MAX_DIFF_BYTES_PER_FILE})", req.opts.max_bytes_per_file)));
        }
        let mirror = MirrorRef::new(req.org_id, req.repo_id);
        self.require_commits(&mirror, &[req.from, req.to]).await?;
        let files = self.git.diff(&mirror, req.from, req.to, &req.opts).await?;
        Ok(DiffResponse { files })
    }

    /// `POST /internal/v1/log` (`max` in `1..=200`); every include/exclude sha in the mirror.
    pub async fn log(&self, req: &LogRequest) -> Result<LogResponse, AppError> {
        let q = &req.query;
        if q.max == 0 || q.max > MAX_LOG_COMMITS {
            return Err(AppError::Invalid(format!("max {} (1..={MAX_LOG_COMMITS})", q.max)));
        }
        if q.include.is_empty() {
            return Err(AppError::Invalid("log needs at least one include sha".into()));
        }
        let mirror = MirrorRef::new(req.org_id, req.repo_id);
        self.require_commits(&mirror, &q.include).await?;
        self.require_commits(&mirror, &q.exclude).await?;
        let commits = self.git.log(&mirror, q).await?;
        Ok(LogResponse { commits })
    }

    /// `GET /internal/v1/health`: mirrors on disk and bytes held by checkouts; `ok = false` when
    /// the mirrors cannot be listed.
    pub async fn health(&self) -> HealthResponse {
        let checkouts_bytes = self.checkouts.bytes();
        match self.git.list_mirrors().await {
            Ok(mirrors) => HealthResponse { ok: true, mirrors: mirrors.len() as u32, checkouts_bytes },
            Err(err) => {
                tracing::warn!(error = %err, "health: listing mirrors failed");
                HealthResponse { ok: false, mirrors: 0, checkouts_bytes }
            }
        }
    }
}

#[cfg(test)]
mod tests;
