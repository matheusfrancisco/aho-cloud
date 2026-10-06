//! Read-only checkouts for live text search (knowledge.md "Text search on Cloud Run" step 3):
//! `{data_dir}/checkouts/{org}/{repo}/{sha}/`, materialized once per sha (single flight) into
//! `{sha}.tmp` and renamed into place, recorded with a `{sha}.size` sidecar, evicted least
//! recently used above the byte cap. Leases are refcounted so an in-use checkout is never
//! evicted.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Instant;

use mage_core::domain::{CommitSha, OrgId, RepoId};

use crate::error::AppError;
use crate::ports::git::{GitRepoReader, MirrorRef};

type Key = (OrgId, RepoId, CommitSha);

struct Entry {
    size: u64,
    last_used: Instant,
    leases: u32,
}

#[derive(Default)]
struct State {
    entries: HashMap<Key, Entry>,
    /// Per-sha single-flight locks for materialization.
    building: HashMap<Key, Arc<tokio::sync::Mutex<()>>>,
}

struct Inner {
    root: PathBuf,
    cap_bytes: u64,
    state: Mutex<State>,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The checkout cache under `{data_dir}/checkouts`.
#[derive(Clone)]
pub struct CheckoutCache {
    inner: Arc<Inner>,
}

/// A checkout in use; the cache will not evict it while the lease lives.
pub struct CheckoutLease {
    inner: Arc<Inner>,
    key: Key,
    path: PathBuf,
}

impl CheckoutLease {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for CheckoutLease {
    fn drop(&mut self) {
        let mut state = self.inner.lock();
        if let Some(entry) = state.entries.get_mut(&self.key) {
            entry.leases = entry.leases.saturating_sub(1);
            entry.last_used = Instant::now();
        }
    }
}

fn dir_of(root: &Path, (org, repo, sha): &Key) -> PathBuf {
    root.join(org.to_string()).join(repo.to_string()).join(sha.to_string())
}

fn sidecar(dir: &Path) -> PathBuf {
    dir.with_extension("size")
}

fn tmp_of(dir: &Path) -> PathBuf {
    dir.with_extension("tmp")
}

/// Total size of regular files and symlinks under `dir` (symlinks not followed).
fn tree_size(dir: &Path) -> std::io::Result<u64> {
    let mut total = 0;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        for entry in std::fs::read_dir(&next)? {
            let entry = entry?;
            let meta = std::fs::symlink_metadata(entry.path())?;
            if meta.is_dir() {
                stack.push(entry.path());
            } else {
                total += meta.len();
            }
        }
    }
    Ok(total)
}

fn remove_tree(dir: &Path) {
    if let Err(err) = std::fs::remove_dir_all(dir) {
        if err.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(path = %dir.display(), error = %err, "could not remove checkout files");
        }
    }
}

/// Sidecar first: an interrupted removal leaves an incomplete checkout, deleted on next open.
fn remove_checkout(dir: &Path) {
    let _ = std::fs::remove_file(sidecar(dir));
    remove_tree(dir);
}

impl CheckoutCache {
    /// Opens the cache at `root`, registering complete checkouts left by an earlier process
    /// (directory plus size sidecar) and deleting incomplete ones.
    pub fn open(root: PathBuf, cap_bytes: u64) -> Self {
        let mut state = State::default();
        for (key, dir) in scan(&root) {
            match std::fs::read_to_string(sidecar(&dir)).ok().and_then(|s| s.trim().parse::<u64>().ok()) {
                Some(size) => {
                    state.entries.insert(key, Entry { size, last_used: Instant::now(), leases: 0 });
                }
                None => remove_checkout(&dir),
            }
        }
        CheckoutCache { inner: Arc::new(Inner { root, cap_bytes, state: Mutex::new(state) }) }
    }

    /// Bytes held by registered checkouts.
    pub fn bytes(&self) -> u64 {
        self.inner.lock().entries.values().map(|e| e.size).sum()
    }

    /// The checkout of `sha`, materialized from the mirror on first use.
    pub async fn lease(&self, git: &dyn GitRepoReader, org: OrgId, repo: RepoId, sha: CommitSha) -> Result<CheckoutLease, AppError> {
        let key = (org, repo, sha);
        let dir = dir_of(&self.inner.root, &key);
        if let Some(lease) = self.try_lease(key, &dir) {
            return Ok(lease);
        }
        let flight = self.inner.lock().building.entry(key).or_default().clone();
        let _guard = flight.lock().await;
        if let Some(lease) = self.try_lease(key, &dir) {
            return Ok(lease);
        }
        // On failure the flight lock stays registered, so a retry never races a waiter.
        let size = self.materialize(git, key, &dir).await?;
        let lease = {
            let mut state = self.inner.lock();
            state.entries.insert(key, Entry { size, last_used: Instant::now(), leases: 1 });
            state.building.remove(&key);
            CheckoutLease { inner: self.inner.clone(), key, path: dir }
        };
        self.evict().await;
        Ok(lease)
    }

    fn try_lease(&self, key: Key, dir: &Path) -> Option<CheckoutLease> {
        let mut state = self.inner.lock();
        let entry = state.entries.get_mut(&key)?;
        entry.leases += 1;
        entry.last_used = Instant::now();
        Some(CheckoutLease { inner: self.inner.clone(), key, path: dir.to_path_buf() })
    }

    async fn materialize(&self, git: &dyn GitRepoReader, key: Key, dir: &Path) -> Result<u64, AppError> {
        let tmp = tmp_of(dir);
        let parent = dir.parent().map(Path::to_path_buf).unwrap_or_else(|| self.inner.root.clone());
        let (tmp_clean, parent_clean) = (tmp.clone(), parent.clone());
        tokio::task::spawn_blocking(move || {
            remove_tree(&tmp_clean);
            std::fs::create_dir_all(&parent_clean)
        })
        .await
        .map_err(|e| AppError::Internal(format!("checkout task: {e}")))?
        .map_err(|e| AppError::Unavailable(format!("checkout dir: {e}")))?;
        git.materialize(&MirrorRef::new(key.0, key.1), key.2, &tmp).await?;
        let (tmp_done, dir_done) = (tmp.clone(), dir.to_path_buf());
        tokio::task::spawn_blocking(move || -> std::io::Result<u64> {
            let size = tree_size(&tmp_done)?;
            remove_checkout(&dir_done);
            std::fs::rename(&tmp_done, &dir_done)?;
            std::fs::write(sidecar(&dir_done), size.to_string())?;
            Ok(size)
        })
        .await
        .map_err(|e| AppError::Internal(format!("checkout task: {e}")))?
        .map_err(|e| AppError::Unavailable(format!("checkout {}: {e}", dir.display())))
    }

    /// Drops least recently used, unleased checkouts until the cache fits its cap.
    async fn evict(&self) {
        let victims: Vec<PathBuf> = {
            let mut state = self.inner.lock();
            let mut total: u64 = state.entries.values().map(|e| e.size).sum();
            let mut idle: Vec<(Instant, Key)> = state.entries.iter().filter(|(_, e)| e.leases == 0).map(|(k, e)| (e.last_used, *k)).collect();
            idle.sort();
            let mut victims = Vec::new();
            for (_, key) in idle {
                if total <= self.inner.cap_bytes {
                    break;
                }
                if let Some(entry) = state.entries.remove(&key) {
                    total -= entry.size;
                    victims.push(dir_of(&self.inner.root, &key));
                }
            }
            victims
        };
        if victims.is_empty() {
            return;
        }
        let removal = tokio::task::spawn_blocking(move || victims.iter().for_each(|dir| remove_checkout(dir)));
        if let Err(err) = removal.await {
            tracing::warn!(error = %err, "checkout eviction task failed");
        }
    }
}

/// `{root}/{org}/{repo}/{sha}` directories with parseable ids.
fn scan(root: &Path) -> Vec<(Key, PathBuf)> {
    let mut found = Vec::new();
    let dirs = |p: &Path| -> Vec<(String, PathBuf)> {
        std::fs::read_dir(p)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .filter_map(|e| Some((e.file_name().into_string().ok()?, e.path())))
            .collect()
    };
    for (org, org_dir) in dirs(root) {
        let Ok(org) = org.parse::<OrgId>() else { continue };
        for (repo, repo_dir) in dirs(&org_dir) {
            let Ok(repo) = repo.parse::<RepoId>() else { continue };
            for (sha, sha_dir) in dirs(&repo_dir) {
                match sha.parse::<CommitSha>() {
                    Ok(sha) => found.push(((org, repo, sha), sha_dir)),
                    // Leftover `{sha}.tmp` of an interrupted materialization.
                    Err(_) if sha.ends_with(".tmp") => remove_tree(&sha_dir),
                    Err(_) => {}
                }
            }
        }
    }
    found
}
