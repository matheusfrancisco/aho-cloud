//! `MemRepoStore`: in-memory `RepoStore` (storage.md "`RepoStore`"). Outbox jobs go to a shared
//! [`MemJobQueue`] inside the same lock scope as the write, after validation, so a store call and
//! its jobs land together or not at all.

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use parking_lot::Mutex;
use time::OffsetDateTime;

use crate::domain::customer::Deployment;
use crate::domain::ids::{CommitSha, OrgId, RecipeHash, RepoId, RepoVersionKey, TreeOid};
use crate::domain::recipe::{Recipe, RecipeData, RecipeStatus};
use crate::domain::repo::{
    IndexRevs, Layer, LayerEvent, LayerMap, LayerState, Repo, RepoAccess, RepoVersion, SourceBundleRef, VersionReason,
};
use crate::ports::clock::Clock;
use crate::ports::common::{Cursor, Page, PageRequest};
use crate::ports::jobs::NewJob;
use crate::ports::repos::{RepoStore, RepoStoreError, VersionReasonRecord};
use crate::testing::jobs::MemJobQueue;
use crate::testing::page::keyset_page;

pub struct MemRepoStore {
    clock: Arc<dyn Clock>,
    jobs: Arc<MemJobQueue>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    repos: BTreeMap<(OrgId, RepoId), Repo>,
    versions: BTreeMap<(OrgId, RepoVersionKey), VersionRow>,
    reasons: Vec<ReasonRow>,
    /// `done_at` per batch.
    batches: BTreeMap<(OrgId, RepoVersionKey, Layer, u32), Option<OffsetDateTime>>,
    recipes: BTreeMap<(OrgId, RepoId, RecipeHash), RecipeRow>,
    deployments: BTreeMap<(OrgId, RepoId, String, OffsetDateTime), Deployment>,
}

struct VersionRow {
    layers: LayerMap,
    bundle: Option<SourceBundleRef>,
    revs: Option<IndexRevs>,
    created_at: OffsetDateTime,
}

struct ReasonRow {
    org: OrgId,
    key: RepoVersionKey,
    reason: VersionReason,
    added_at: OffsetDateTime,
    ended_at: Option<OffsetDateTime>,
}

struct RecipeRow {
    recipe: Recipe,
    created_at: OffsetDateTime,
}

fn dangling(what: &str) -> RepoStoreError {
    RepoStoreError::Invalid(format!("dangling reference: {what}"))
}

fn cross_tenant() -> RepoStoreError {
    RepoStoreError::Invalid("row belongs to another org".into())
}

fn bad_cursor(cursor: &Cursor) -> RepoStoreError {
    RepoStoreError::Invalid(format!("bad cursor {:?}", cursor.0))
}

/// Whether two reasons occupy the same singleton slot (`DefaultHead`, `Deployed(env)`).
fn same_singleton(a: &VersionReason, b: &VersionReason) -> bool {
    match (a, b) {
        (VersionReason::DefaultHead, VersionReason::DefaultHead) => true,
        (VersionReason::Deployed(x), VersionReason::Deployed(y)) => x == y,
        _ => false,
    }
}

impl State {
    fn require_repo(&self, org: OrgId, repo: RepoId) -> Result<&Repo, RepoStoreError> {
        self.repos.get(&(org, repo)).ok_or_else(|| dangling("repo"))
    }

    fn version_row(&mut self, org: OrgId, key: RepoVersionKey) -> Result<&mut VersionRow, RepoStoreError> {
        self.versions.get_mut(&(org, key)).ok_or(RepoStoreError::NotFound)
    }

    fn current_reasons(&self, org: OrgId, key: RepoVersionKey) -> BTreeSet<VersionReason> {
        self.reasons.iter().filter(|r| r.org == org && r.key == key && r.ended_at.is_none()).map(|r| r.reason.clone()).collect()
    }

    fn view(&self, org: OrgId, key: RepoVersionKey, row: &VersionRow) -> RepoVersion {
        RepoVersion {
            org_id: org,
            repo_id: key.repo_id,
            sha: key.sha,
            reasons: self.current_reasons(org, key),
            layers: row.layers.clone(),
            bundle: row.bundle.clone(),
            revs: row.revs,
            created_at: row.created_at,
        }
    }

    fn add_reason(&mut self, org: OrgId, key: RepoVersionKey, reason: VersionReason, now: OffsetDateTime) -> Result<bool, RepoStoreError> {
        self.require_repo(org, key.repo_id)?;
        let created = !self.versions.contains_key(&(org, key));
        if created {
            let fresh = RepoVersion::new(org, key, reason.clone(), now);
            self.versions.insert((org, key), VersionRow { layers: fresh.layers, bundle: None, revs: None, created_at: now });
        }
        let current = self.reasons.iter().any(|r| r.org == org && r.key == key && r.ended_at.is_none() && r.reason == reason);
        if current {
            return Ok(created);
        }
        for held in self.reasons.iter_mut() {
            let moves = held.org == org && held.key.repo_id == key.repo_id && held.key.sha != key.sha && held.ended_at.is_none();
            if moves && same_singleton(&held.reason, &reason) {
                held.ended_at = Some(now);
            }
        }
        self.reasons.push(ReasonRow { org, key, reason, added_at: now, ended_at: None });
        Ok(created)
    }
}

/// Validated recipe sort key: `validated_at DESC`, then hash.
fn validated_at(recipe: &Recipe) -> Option<OffsetDateTime> {
    match recipe.status {
        RecipeStatus::Validated { at } => Some(at),
        RecipeStatus::Draft | RecipeStatus::Broken { .. } => None,
    }
}

type RecipeKey = (Reverse<i128>, RecipeHash);
type DeploymentKey = (Reverse<i128>, String);

fn encode_recipe(key: &RecipeKey) -> String {
    format!("{}/{}", (key.0).0, key.1)
}

fn decode_recipe(cursor: &Cursor) -> Result<RecipeKey, RepoStoreError> {
    let (nanos, hash) = cursor.0.split_once('/').ok_or_else(|| bad_cursor(cursor))?;
    Ok((Reverse(nanos.parse().map_err(|_| bad_cursor(cursor))?), hash.parse().map_err(|_| bad_cursor(cursor))?))
}

fn encode_deployment(key: &DeploymentKey) -> String {
    format!("{}/{}", (key.0).0, key.1)
}

fn decode_deployment(cursor: &Cursor) -> Result<DeploymentKey, RepoStoreError> {
    let (nanos, env) = cursor.0.split_once('/').ok_or_else(|| bad_cursor(cursor))?;
    Ok((Reverse(nanos.parse().map_err(|_| bad_cursor(cursor))?), env.to_owned()))
}

impl MemRepoStore {
    pub fn new(clock: Arc<dyn Clock>, jobs: Arc<MemJobQueue>) -> Self {
        Self { clock, jobs, state: Mutex::new(State::default()) }
    }

    /// The version's `IndexRevs` (`None` when the version is absent or has none): the knowledge
    /// fake reads facts at these, like the Postgres join on `repo_versions.revs`.
    pub fn index_revs(&self, org: OrgId, key: RepoVersionKey) -> Option<IndexRevs> {
        self.state.lock().versions.get(&(org, key)).and_then(|row| row.revs)
    }

    fn outbox(&self, jobs: &[NewJob]) {
        self.jobs.insert_validated(jobs);
    }
}

#[async_trait]
impl RepoStore for MemRepoStore {
    async fn upsert_repo(&self, repo: &Repo, jobs: &[NewJob]) -> Result<Repo, RepoStoreError> {
        MemJobQueue::validate_all(jobs)?;
        let org = repo.org_id;
        let mut state = self.state.lock();
        let existing = state.repos.values().find(|r| r.org_id == org && r.external_id == repo.external_id).map(|r| r.id);
        let stored = match existing {
            Some(id) => {
                let row = state.repos.get_mut(&(org, id)).ok_or(RepoStoreError::NotFound)?;
                row.owner.clone_from(&repo.owner);
                row.name.clone_from(&repo.name);
                row.default_branch.clone_from(&repo.default_branch);
                row.installation_id = repo.installation_id;
                row.access = repo.access;
                row.clone()
            }
            None => {
                if state.repos.contains_key(&(org, repo.id)) {
                    return Err(RepoStoreError::Conflict);
                }
                state.repos.insert((org, repo.id), repo.clone());
                repo.clone()
            }
        };
        self.outbox(jobs);
        Ok(stored)
    }

    async fn repo(&self, org: OrgId, id: RepoId) -> Result<Repo, RepoStoreError> {
        self.state.lock().repos.get(&(org, id)).cloned().ok_or(RepoStoreError::NotFound)
    }

    async fn get_by_external_id(&self, org: OrgId, external_id: u64) -> Result<Option<Repo>, RepoStoreError> {
        Ok(self.state.lock().repos.values().find(|r| r.org_id == org && r.external_id == external_id).cloned())
    }

    async fn repo_by_name(&self, org: OrgId, owner: &str, name: &str) -> Result<Option<Repo>, RepoStoreError> {
        let (owner, name) = (owner.to_lowercase(), name.to_lowercase());
        let state = self.state.lock();
        Ok(state.repos.values().find(|r| r.org_id == org && r.owner.to_lowercase() == owner && r.name.to_lowercase() == name).cloned())
    }

    async fn list_repos(&self, org: OrgId) -> Result<Vec<Repo>, RepoStoreError> {
        let state = self.state.lock();
        let mut repos: Vec<Repo> = state.repos.values().filter(|r| r.org_id == org).cloned().collect();
        repos.sort_by_cached_key(|r| (r.owner.to_lowercase(), r.name.to_lowercase(), r.id));
        Ok(repos)
    }

    async fn set_access(&self, org: OrgId, repo: RepoId, access: RepoAccess) -> Result<(), RepoStoreError> {
        let mut state = self.state.lock();
        state.repos.get_mut(&(org, repo)).ok_or(RepoStoreError::NotFound)?.access = access;
        Ok(())
    }

    async fn active_repos_system(&self) -> Result<Vec<(OrgId, RepoId)>, RepoStoreError> {
        let state = self.state.lock();
        Ok(state.repos.iter().filter(|(_, r)| r.access == RepoAccess::Granted).map(|(key, _)| *key).collect())
    }

    async fn add_version_reason(&self, org: OrgId, repo: RepoId, sha: CommitSha, reason: VersionReason) -> Result<bool, RepoStoreError> {
        let now = self.clock.now();
        self.state.lock().add_reason(org, RepoVersionKey::new(repo, sha), reason, now)
    }

    async fn default_head(&self, org: OrgId, repo: RepoId) -> Result<Option<CommitSha>, RepoStoreError> {
        let state = self.state.lock();
        Ok(state
            .reasons
            .iter()
            .find(|r| r.org == org && r.key.repo_id == repo && r.ended_at.is_none() && r.reason == VersionReason::DefaultHead)
            .map(|r| r.key.sha))
    }

    async fn set_default_head(&self, org: OrgId, repo: RepoId, sha: CommitSha) -> Result<bool, RepoStoreError> {
        self.add_version_reason(org, repo, sha, VersionReason::DefaultHead).await
    }

    async fn version(&self, org: OrgId, key: RepoVersionKey) -> Result<RepoVersion, RepoStoreError> {
        let state = self.state.lock();
        let row = state.versions.get(&(org, key)).ok_or(RepoStoreError::NotFound)?;
        Ok(state.view(org, key, row))
    }

    async fn versions(&self, org: OrgId, repo: RepoId) -> Result<Vec<RepoVersion>, RepoStoreError> {
        let state = self.state.lock();
        let mut out: Vec<RepoVersion> =
            state.versions.iter().filter(|((o, key), _)| *o == org && key.repo_id == repo).map(|((_, key), row)| state.view(org, *key, row)).collect();
        out.sort_by_key(|v| (v.created_at, v.sha));
        Ok(out)
    }

    async fn version_reasons(&self, org: OrgId, repo: RepoId) -> Result<Vec<VersionReasonRecord>, RepoStoreError> {
        let state = self.state.lock();
        let mut out: Vec<VersionReasonRecord> = state
            .reasons
            .iter()
            .filter(|r| r.org == org && r.key.repo_id == repo)
            .map(|r| VersionReasonRecord { sha: r.key.sha, reason: r.reason.clone(), added_at: r.added_at, ended_at: r.ended_at })
            .collect();
        out.sort_by(|a, b| (a.added_at, a.sha, &a.reason).cmp(&(b.added_at, b.sha, &b.reason)));
        Ok(out)
    }

    async fn delete_version(&self, org: OrgId, key: RepoVersionKey) -> Result<(), RepoStoreError> {
        let mut state = self.state.lock();
        state.versions.remove(&(org, key));
        state.reasons.retain(|r| !(r.org == org && r.key == key));
        state.batches.retain(|(o, k, _, _), _| !(*o == org && *k == key));
        Ok(())
    }

    async fn apply_layer_event(
        &self,
        org: OrgId,
        key: RepoVersionKey,
        layer: Layer,
        event: LayerEvent,
        jobs: &[NewJob],
    ) -> Result<LayerState, RepoStoreError> {
        MemJobQueue::validate_all(jobs)?;
        let mut state = self.state.lock();
        let row = state.version_row(org, key)?;
        let current = row.layers.get(&layer).cloned().unwrap_or(LayerState::Pending);
        let next = current.apply(event).map_err(|e| RepoStoreError::Invalid(e.to_string()))?;
        row.layers.insert(layer, next.clone());
        self.outbox(jobs);
        Ok(next)
    }

    async fn begin_layer_batches(&self, org: OrgId, key: RepoVersionKey, layer: Layer, n: u32, jobs: &[NewJob]) -> Result<(), RepoStoreError> {
        if n == 0 {
            return Err(RepoStoreError::Invalid("a batched layer needs at least one batch".into()));
        }
        MemJobQueue::validate_all(jobs)?;
        let mut state = self.state.lock();
        let row = state.version_row(org, key)?;
        let current = row.layers.get(&layer).cloned().unwrap_or(LayerState::Pending);
        if !current.is_ready() {
            let next = current.apply(LayerEvent::Start).map_err(|e| RepoStoreError::Invalid(e.to_string()))?;
            row.layers.insert(layer, next);
        }
        state.batches.retain(|(o, k, l, _), _| !(*o == org && *k == key && *l == layer));
        for batch in 1..=n {
            state.batches.insert((org, key, layer, batch), None);
        }
        self.outbox(jobs);
        Ok(())
    }

    async fn finish_layer_batch(&self, org: OrgId, key: RepoVersionKey, layer: Layer, batch: u32, on_last: &[NewJob]) -> Result<u32, RepoStoreError> {
        MemJobQueue::validate_all(on_last)?;
        let now = self.clock.now();
        let mut state = self.state.lock();
        state.version_row(org, key)?;
        let done = *state.batches.get(&(org, key, layer, batch)).ok_or(RepoStoreError::NotFound)?;
        let open = |state: &State| {
            let count = state.batches.iter().filter(|((o, k, l, _), done)| *o == org && *k == key && *l == layer && done.is_none()).count();
            u32::try_from(count).unwrap_or(u32::MAX)
        };
        if done.is_some() {
            return Ok(open(&state));
        }
        let remaining = open(&state) - 1;
        if remaining == 0 {
            let row = state.version_row(org, key)?;
            let current = row.layers.get(&layer).cloned().unwrap_or(LayerState::Pending);
            let next = current.apply(LayerEvent::Complete { at: now }).map_err(|e| RepoStoreError::Invalid(e.to_string()))?;
            row.layers.insert(layer, next);
            self.outbox(on_last);
        }
        state.batches.insert((org, key, layer, batch), Some(now));
        Ok(remaining)
    }

    async fn set_index_revs(&self, org: OrgId, key: RepoVersionKey, revs: &IndexRevs) -> Result<(), RepoStoreError> {
        self.state.lock().version_row(org, key)?.revs = Some(*revs);
        Ok(())
    }

    async fn set_source_bundle(&self, org: OrgId, key: RepoVersionKey, bundle: &SourceBundleRef, jobs: &[NewJob]) -> Result<(), RepoStoreError> {
        MemJobQueue::validate_all(jobs)?;
        let mut state = self.state.lock();
        state.version_row(org, key)?.bundle = Some(bundle.clone());
        self.outbox(jobs);
        Ok(())
    }

    async fn bundle_for_tree(&self, org: OrgId, repo: RepoId, tree: TreeOid) -> Result<Option<SourceBundleRef>, RepoStoreError> {
        let state = self.state.lock();
        Ok(state
            .versions
            .iter()
            .filter(|((o, key), _)| *o == org && key.repo_id == repo)
            .find_map(|(_, row)| row.bundle.as_ref().filter(|b| b.tree == tree).cloned()))
    }

    async fn put_recipe(&self, org: OrgId, recipe: &Recipe) -> Result<bool, RepoStoreError> {
        if recipe.org_id != org {
            return Err(cross_tenant());
        }
        let now = self.clock.now();
        let mut state = self.state.lock();
        state.require_repo(org, recipe.repo_id)?;
        let key = (org, recipe.repo_id, recipe.hash);
        if state.recipes.contains_key(&key) {
            return Ok(false);
        }
        state.recipes.insert(key, RecipeRow { recipe: recipe.clone(), created_at: now });
        Ok(true)
    }

    async fn get_recipe(&self, org: OrgId, repo: RepoId, hash: RecipeHash) -> Result<Recipe, RepoStoreError> {
        self.state.lock().recipes.get(&(org, repo, hash)).map(|row| row.recipe.clone()).ok_or(RepoStoreError::NotFound)
    }

    async fn latest_recipe(&self, org: OrgId, repo: RepoId) -> Result<Option<Recipe>, RepoStoreError> {
        let state = self.state.lock();
        let rows: Vec<&RecipeRow> = state.recipes.iter().filter(|((o, r, _), _)| *o == org && *r == repo).map(|(_, row)| row).collect();
        let validated = rows
            .iter()
            .filter_map(|row| validated_at(&row.recipe).map(|at| (Reverse(at), row.recipe.hash, *row)))
            .min_by_key(|(at, hash, _)| (*at, *hash))
            .map(|(_, _, row)| row.recipe.clone());
        let draft = || {
            rows.iter()
                .filter(|row| row.recipe.status == RecipeStatus::Draft)
                .min_by_key(|row| (Reverse(row.created_at), row.recipe.hash))
                .map(|row| row.recipe.clone())
        };
        Ok(validated.or_else(draft))
    }

    async fn list_recipes(&self, org: OrgId, repo: RepoId, page: PageRequest) -> Result<Page<Recipe>, RepoStoreError> {
        let after = page.after.as_ref().map(decode_recipe).transpose()?;
        let state = self.state.lock();
        let items: Vec<(RecipeKey, Recipe)> = state
            .recipes
            .iter()
            .filter(|((o, r, _), _)| *o == org && *r == repo)
            .map(|(_, row)| ((Reverse(row.created_at.unix_timestamp_nanos()), row.recipe.hash), row.recipe.clone()))
            .collect();
        let (items, next) = keyset_page(items, after.as_ref(), page.effective_limit(), encode_recipe);
        Ok(Page { items, next })
    }

    async fn update_recipe(&self, org: OrgId, recipe: &Recipe, expect: &RecipeStatus, jobs: &[NewJob]) -> Result<(), RepoStoreError> {
        if recipe.org_id != org {
            return Err(cross_tenant());
        }
        MemJobQueue::validate_all(jobs)?;
        let mut state = self.state.lock();
        let row = state.recipes.get_mut(&(org, recipe.repo_id, recipe.hash)).ok_or(RepoStoreError::NotFound)?;
        if row.recipe.status != *expect {
            return Err(RepoStoreError::Conflict);
        }
        let stored: RecipeData = row.recipe.clone().into_data();
        row.recipe = Recipe::rehydrate(RecipeData { status: recipe.status.clone(), validated_at_sha: recipe.validated_at_sha, ..stored });
        self.outbox(jobs);
        Ok(())
    }

    async fn record_deployment(&self, org: OrgId, deployment: &Deployment) -> Result<bool, RepoStoreError> {
        if deployment.org_id != org {
            return Err(cross_tenant());
        }
        let mut state = self.state.lock();
        state.require_repo(org, deployment.repo_id)?;
        let key = (org, deployment.repo_id, deployment.environment.clone(), deployment.observed_at);
        if state.deployments.contains_key(&key) {
            return Ok(false);
        }
        state.deployments.insert(key, deployment.clone());
        Ok(true)
    }

    async fn latest_deployment(&self, org: OrgId, repo: RepoId, environment: &str, as_of: OffsetDateTime) -> Result<Option<Deployment>, RepoStoreError> {
        let state = self.state.lock();
        Ok(state
            .deployments
            .iter()
            .filter(|((o, r, env, at), _)| *o == org && *r == repo && env == environment && *at <= as_of)
            .max_by_key(|((_, _, _, at), _)| *at)
            .map(|(_, d)| d.clone()))
    }

    async fn list_deployments(&self, org: OrgId, repo: RepoId, page: PageRequest) -> Result<Page<Deployment>, RepoStoreError> {
        let after = page.after.as_ref().map(decode_deployment).transpose()?;
        let state = self.state.lock();
        let items: Vec<(DeploymentKey, Deployment)> = state
            .deployments
            .iter()
            .filter(|((o, r, _, _), _)| *o == org && *r == repo)
            .map(|((_, _, env, at), d)| ((Reverse(at.unix_timestamp_nanos()), env.clone()), d.clone()))
            .collect();
        let (items, next) = keyset_page(items, after.as_ref(), page.effective_limit(), encode_deployment);
        Ok(Page { items, next })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::jobs::JobQueue;
    use crate::testing::{RepoStoreFixture, SeqIdGen, contract_repo_store};

    #[tokio::test]
    async fn mem_repo_store_passes_the_contract() {
        contract_repo_store(|clock| async move {
            let jobs = Arc::new(MemJobQueue::new(clock.clone(), Arc::new(SeqIdGen::new())));
            let repos = Arc::new(MemRepoStore::new(clock, jobs.clone()));
            RepoStoreFixture { repos: repos as Arc<dyn RepoStore>, jobs: jobs as Arc<dyn JobQueue> }
        })
        .await;
    }
}
