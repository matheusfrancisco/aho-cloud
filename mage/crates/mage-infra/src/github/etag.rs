//! Conditional GETs: the last `ETag` and body per (URL, installation, token scope). A 304 does not
//! count against the primary rate limit [unverified], so repeated metadata reads stay cheap.

use std::collections::{HashMap, VecDeque};

use bytes::Bytes;
use mage_core::domain::ids::InstallationId;
use parking_lot::Mutex;

/// Entries kept; the oldest insert is dropped first.
pub(crate) const MAX_ENTRIES: usize = 512;
/// Larger bodies are not cached.
pub(crate) const MAX_BODY: usize = 256 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct EtagKey {
    pub url: String,
    pub inst: InstallationId,
    pub scope: String,
}

#[derive(Debug, Clone)]
pub(crate) struct Cached {
    pub etag: String,
    pub status: u16,
    pub body: Bytes,
    pub next: Option<String>,
}

#[derive(Debug, Default)]
pub(crate) struct EtagCache {
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    entries: HashMap<EtagKey, Cached>,
    /// Insertion order of `entries`' keys.
    order: VecDeque<EtagKey>,
}

impl EtagCache {
    pub fn get(&self, key: &EtagKey) -> Option<Cached> {
        self.state.lock().entries.get(key).cloned()
    }

    /// Stores a response; bodies over [`MAX_BODY`] drop any older entry instead.
    pub fn put(&self, key: EtagKey, cached: Cached) {
        let mut state = self.state.lock();
        if cached.body.len() > MAX_BODY {
            if state.entries.remove(&key).is_some() {
                state.order.retain(|k| k != &key);
            }
            return;
        }
        if state.entries.insert(key.clone(), cached).is_none() {
            state.order.push_back(key);
        }
        while state.entries.len() > MAX_ENTRIES {
            let Some(oldest) = state.order.pop_front() else { break };
            state.entries.remove(&oldest);
        }
    }

    pub fn evict_installation(&self, inst: InstallationId) {
        let mut state = self.state.lock();
        state.entries.retain(|k, _| k.inst != inst);
        state.order.retain(|k| k.inst != inst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: usize, inst: u64) -> EtagKey {
        EtagKey { url: format!("https://api.github.com/x/{n}"), inst: InstallationId(inst), scope: String::new() }
    }

    fn cached(len: usize) -> Cached {
        Cached { etag: "\"e\"".into(), status: 200, body: Bytes::from(vec![b'x'; len]), next: None }
    }

    #[test]
    fn bounded_oldest_first() {
        let cache = EtagCache::default();
        for n in 0..=MAX_ENTRIES {
            cache.put(key(n, 1), cached(1));
        }
        assert!(cache.get(&key(0, 1)).is_none(), "the oldest entry is dropped");
        assert!(cache.get(&key(1, 1)).is_some());
        assert!(cache.get(&key(MAX_ENTRIES, 1)).is_some());
    }

    #[test]
    fn large_bodies_are_not_kept() {
        let cache = EtagCache::default();
        cache.put(key(0, 1), cached(1));
        cache.put(key(0, 1), cached(MAX_BODY + 1));
        assert!(cache.get(&key(0, 1)).is_none());
    }

    #[test]
    fn eviction_is_per_installation() {
        let cache = EtagCache::default();
        cache.put(key(0, 1), cached(1));
        cache.put(key(0, 2), cached(1));
        cache.evict_installation(InstallationId(1));
        assert!(cache.get(&key(0, 1)).is_none());
        assert!(cache.get(&key(0, 2)).is_some());
    }
}
