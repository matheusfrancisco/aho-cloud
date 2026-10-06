//! `RepoStore` port (storage.md "`RepoStore`"; method names merge intake.md and knowledge.md).
//! Repos, repo versions with their reasons, layer status and batches, index revisions, source
//! bundles, recipes and the deployment history.
//!
//! Semantics every implementation follows (contract suite in `mage_core::testing`):
//!
//! - Methods with `jobs: &[NewJob]` (and `on_last`) enqueue them in the same transaction as the
//!   write, with `JobQueue::enqueue` semantics (a duplicate is silent). An invalid job fails the
//!   whole call and nothing is written.
//! - A reference to a missing repo (or a version of one) is `Invalid("dangling reference: …")`.
//! - `add_version_reason` is idempotent. `Release(tag)` and `OnDemand(ticket)` are additive.
//!   `DefaultHead` (one per repo) and `Deployed(env)` (one per env) move: the previous holder's
//!   reason gets `ended_at = now`. `RepoVersion.reasons` holds current reasons only.
//! - `apply_layer_event` applies `LayerState::apply` (an invalid transition is `Invalid`).
//! - `begin_layer_batches(n)` replaces the layer's batch set (existing batches of `(key, layer)`
//!   are deleted, batches `1..=n` created open) and applies `Start` unless the layer is `Ready`
//!   (a shadow rebuild keeps serving the `Ready` layer). `finish_layer_batch` marks one batch done
//!   idempotently and, when the last one finishes, applies `Complete { at: now }` (`Ready` stays
//!   `Ready` with the new time) and enqueues `on_last`, exactly once.

use async_trait::async_trait;
use time::OffsetDateTime;

use crate::domain::customer::Deployment;
use crate::domain::ids::{CommitSha, OrgId, RecipeHash, RepoId, RepoVersionKey, TreeOid};
use crate::domain::recipe::{Recipe, RecipeStatus};
use crate::domain::repo::{IndexRevs, Layer, LayerEvent, LayerState, Repo, RepoAccess, RepoVersion, SourceBundleRef, VersionReason};
use crate::ports::common::{Page, PageRequest};
use crate::ports::jobs::{JobError, NewJob};

/// One `repo_version_reasons` row: a reason held by a sha, current while `ended_at` is `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionReasonRecord {
    pub sha: CommitSha,
    pub reason: VersionReason,
    pub added_at: OffsetDateTime,
    /// Set when a singleton reason (`DefaultHead`, `Deployed(env)`) moved to another sha.
    pub ended_at: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RepoStoreError {
    #[error("not found")]
    NotFound,
    #[error("conflict")]
    Conflict,
    #[error("invalid: {0}")]
    Invalid(String),
    #[error("repo store unavailable: {0}")]
    Unavailable(String),
}

impl From<JobError> for RepoStoreError {
    /// Outbox inserts: a job error fails the store call.
    fn from(err: JobError) -> Self {
        match err {
            JobError::NotFound => Self::NotFound,
            JobError::Conflict | JobError::LeaseLost => Self::Conflict,
            JobError::Invalid(msg) => Self::Invalid(msg),
            JobError::Unavailable(msg) => Self::Unavailable(msg),
        }
    }
}

#[async_trait]
pub trait RepoStore: Send + Sync {
    // ---- repos ----
    /// Insert, or update by `(org_id, external_id)` (owner, name, default branch, installation,
    /// access); the stored id and `created_at` win. Returns the stored repo.
    async fn upsert_repo(&self, repo: &Repo, jobs: &[NewJob]) -> Result<Repo, RepoStoreError>;
    async fn repo(&self, org: OrgId, id: RepoId) -> Result<Repo, RepoStoreError>;
    async fn get_by_external_id(&self, org: OrgId, external_id: u64) -> Result<Option<Repo>, RepoStoreError>;
    /// Case-insensitive on owner and name (GitHub names are).
    async fn repo_by_name(&self, org: OrgId, owner: &str, name: &str) -> Result<Option<Repo>, RepoStoreError>;
    /// Ordered by lowercase owner, lowercase name.
    async fn list_repos(&self, org: OrgId) -> Result<Vec<Repo>, RepoStoreError>;
    async fn set_access(&self, org: OrgId, repo: RepoId, access: RepoAccess) -> Result<(), RepoStoreError>;
    /// Platform listing (reconcile, gc): every `Granted` repo of every org, ordered.
    async fn active_repos_system(&self) -> Result<Vec<(OrgId, RepoId)>, RepoStoreError>;

    // ---- versions ----
    /// `true` = the version was created (every layer `Pending`).
    async fn add_version_reason(&self, org: OrgId, repo: RepoId, sha: CommitSha, reason: VersionReason) -> Result<bool, RepoStoreError>;
    /// Sha holding the current `DefaultHead` reason.
    async fn default_head(&self, org: OrgId, repo: RepoId) -> Result<Option<CommitSha>, RepoStoreError>;
    /// `add_version_reason(org, repo, sha, DefaultHead)`.
    async fn set_default_head(&self, org: OrgId, repo: RepoId, sha: CommitSha) -> Result<bool, RepoStoreError>;
    async fn version(&self, org: OrgId, key: RepoVersionKey) -> Result<RepoVersion, RepoStoreError>;
    /// Ordered by `created_at`, then sha.
    async fn versions(&self, org: OrgId, repo: RepoId) -> Result<Vec<RepoVersion>, RepoStoreError>;
    /// Current and ended reasons, ordered by `added_at`, then sha (`index.gc` retention).
    async fn version_reasons(&self, org: OrgId, repo: RepoId) -> Result<Vec<VersionReasonRecord>, RepoStoreError>;
    /// Removes the version, its reasons, layer status and batches; absent is `Ok`.
    /// `KnowledgeStore::delete_version_facts` runs first.
    async fn delete_version(&self, org: OrgId, key: RepoVersionKey) -> Result<(), RepoStoreError>;

    // ---- layers (knowledge.md) ----
    async fn apply_layer_event(
        &self,
        org: OrgId,
        key: RepoVersionKey,
        layer: Layer,
        event: LayerEvent,
        jobs: &[NewJob],
    ) -> Result<LayerState, RepoStoreError>;
    /// `n >= 1`; replaces the batch set, `Start` unless `Ready` (see the module docs).
    async fn begin_layer_batches(&self, org: OrgId, key: RepoVersionKey, layer: Layer, n: u32, jobs: &[NewJob]) -> Result<(), RepoStoreError>;
    /// Returns the batches still open; `NotFound` for a batch `begin_layer_batches` never created.
    async fn finish_layer_batch(&self, org: OrgId, key: RepoVersionKey, layer: Layer, batch: u32, on_last: &[NewJob]) -> Result<u32, RepoStoreError>;
    async fn set_index_revs(&self, org: OrgId, key: RepoVersionKey, revs: &IndexRevs) -> Result<(), RepoStoreError>;

    // ---- source bundles ----
    async fn set_source_bundle(&self, org: OrgId, key: RepoVersionKey, bundle: &SourceBundleRef, jobs: &[NewJob]) -> Result<(), RepoStoreError>;
    /// A bundle recorded for any version of `repo` with this tree.
    async fn bundle_for_tree(&self, org: OrgId, repo: RepoId, tree: TreeOid) -> Result<Option<SourceBundleRef>, RepoStoreError>;

    // ---- recipes ----
    /// Insert if absent by `(org, repo, hash)`; `true` = inserted.
    async fn put_recipe(&self, org: OrgId, recipe: &Recipe) -> Result<bool, RepoStoreError>;
    async fn get_recipe(&self, org: OrgId, repo: RepoId, hash: RecipeHash) -> Result<Recipe, RepoStoreError>;
    /// Latest `Validated` (by validation time), else latest `Draft` (by insertion); never `Broken`.
    async fn latest_recipe(&self, org: OrgId, repo: RepoId) -> Result<Option<Recipe>, RepoStoreError>;
    /// Newest inserted first.
    async fn list_recipes(&self, org: OrgId, repo: RepoId, page: PageRequest) -> Result<Page<Recipe>, RepoStoreError>;
    /// Writes `status` and `validated_at_sha` when the stored status equals `expect`
    /// (`Conflict` otherwise).
    async fn update_recipe(&self, org: OrgId, recipe: &Recipe, expect: &RecipeStatus, jobs: &[NewJob]) -> Result<(), RepoStoreError>;

    // ---- deployments (append-only history) ----
    /// `false` if `(repo, environment, observed_at)` was already recorded.
    async fn record_deployment(&self, org: OrgId, deployment: &Deployment) -> Result<bool, RepoStoreError>;
    /// Newest observation with `observed_at <= as_of`.
    async fn latest_deployment(&self, org: OrgId, repo: RepoId, environment: &str, as_of: OffsetDateTime) -> Result<Option<Deployment>, RepoStoreError>;
    /// Newest `observed_at` first.
    async fn list_deployments(&self, org: OrgId, repo: RepoId, page: PageRequest) -> Result<Page<Deployment>, RepoStoreError>;
}
