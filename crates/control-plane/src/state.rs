//! Shared server state: config, store, blobs, ledgers, the domain registry, ports, clock, and the
//! per-sandbox locks.

use std::ops::Deref;
use std::sync::Arc;

use cloud_domains::{Registry, Resolver};
use tokio::sync::Mutex;
use tokio_util::task::TaskTracker;

use crate::blobs::BlobStore;
use crate::clock::Clock;
use crate::config::Config;
use crate::crypto::{SecretBox, b64url, b64url_decode};
use crate::error::AppError;
use crate::ledgers::Ledgers;
use crate::ports::{BuildService, RuntimeService, SessionHostService};
use crate::sandboxes::SlugLocks;
use crate::store::Store;
use crate::webauthn::RelyingParty;

/// The ports to sibling services.
#[derive(Clone)]
pub struct Services {
    pub builder: Arc<dyn BuildService>,
    pub runtime: Arc<dyn RuntimeService>,
    pub session_host: Arc<dyn SessionHostService>,
    pub resolver: Arc<dyn Resolver>,
}

pub struct Inner {
    pub cfg: Config,
    pub store: Arc<dyn Store>,
    pub blobs: Arc<dyn BlobStore>,
    pub ledgers: Ledgers,
    pub services: Services,
    pub clock: Arc<dyn Clock>,
    pub secrets: SecretBox,
    pub rp: RelyingParty,
    /// Loaded from the store at start; saved (sealed) after every change.
    pub registry: Mutex<Registry>,
    /// Background work (builds); `AppState::idle` waits for it.
    pub jobs: TaskTracker,
    /// Serializes SITES version bumps.
    pub sites_lock: Mutex<()>,
    /// One change at a time per sandbox.
    pub sandbox_locks: SlugLocks,
}

/// Cheap to clone; one per server.
#[derive(Clone)]
pub struct AppState(Arc<Inner>);

impl Deref for AppState {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.0
    }
}

const REGISTRY_KEY: &str = "domains.registry";

impl AppState {
    /// Builds the state, restoring the domain registry snapshot from the store.
    pub async fn new(
        cfg: Config,
        store: Arc<dyn Store>,
        blobs: Arc<dyn BlobStore>,
        ledgers: Ledgers,
        services: Services,
        clock: Arc<dyn Clock>,
    ) -> Result<Self, AppError> {
        let secrets = SecretBox::new(&cfg.master_key);
        let mut registry = match store.kv_get(REGISTRY_KEY).await? {
            Some(sealed) => {
                let bytes = b64url_decode(&sealed).ok_or_else(|| AppError::internal("domain registry snapshot is not base64url"))?;
                let json = secrets.open_bytes(REGISTRY_KEY, &bytes)?;
                serde_json::from_slice::<Registry>(&json).map_err(|e| AppError::internal(format!("domain registry snapshot: {e}")))?
            }
            None => Registry::new(cfg.master_key.derive("aho-cloud domain tokens v1"), cfg.domain_grace_secs),
        };
        registry.set_edge(cfg.edge_target.clone());
        let rp = RelyingParty { id: cfg.rp_id.clone(), origin: cfg.rp_origin.clone(), name: cfg.rp_name.clone() };
        Ok(Self(Arc::new(Inner {
            cfg,
            store,
            blobs,
            ledgers,
            services,
            clock,
            secrets,
            rp,
            registry: Mutex::new(registry),
            jobs: TaskTracker::new(),
            sites_lock: Mutex::new(()),
            sandbox_locks: SlugLocks::default(),
        })))
    }

    /// Unix seconds.
    pub fn now(&self) -> u64 {
        self.clock.now()
    }

    /// Persists the registry (sealed: the snapshot carries the token secret).
    pub async fn save_registry(&self, registry: &Registry) -> Result<(), AppError> {
        let json = serde_json::to_vec(registry).map_err(|e| AppError::internal(e.to_string()))?;
        let sealed = self.secrets.seal_bytes(REGISTRY_KEY, &json)?;
        self.store.kv_put(REGISTRY_KEY, &b64url(&sealed)).await?;
        Ok(())
    }

    /// Waits until every background job spawned so far has finished.
    pub async fn idle(&self) {
        self.jobs.close();
        self.jobs.wait().await;
        self.jobs.reopen();
    }

    /// Re-checks every live domain binding (the bin runs this periodically).
    pub async fn recheck_domains(&self) -> Result<usize, AppError> {
        let mut registry = self.registry.lock().await;
        let transitions = registry.recheck_all(self.services.resolver.as_ref(), self.now()).await;
        if !transitions.is_empty() {
            self.save_registry(&registry).await?;
        }
        Ok(transitions.len())
    }
}
