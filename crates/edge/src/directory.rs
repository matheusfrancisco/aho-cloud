//! Where the edge learns which hostnames exist and how to serve them.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use cloud_api::internal::{Site, Sites, routes};
use parking_lot::{Mutex, RwLock};

/// Hostname → [`Site`]. `host` is lowercase, without port or trailing dot.
pub trait Directory: Send + Sync {
    fn site(&self, host: &str) -> Option<Arc<Site>>;
}

fn key(hostname: &str) -> String {
    hostname.trim_end_matches('.').to_ascii_lowercase()
}

type Map = HashMap<String, Arc<Site>>;

/// A fixed set of sites that callers may update (tests, single-tenant setups).
#[derive(Default)]
pub struct StaticDirectory {
    sites: RwLock<Map>,
}

impl StaticDirectory {
    pub fn new(sites: impl IntoIterator<Item = Site>) -> Self {
        let dir = Self::default();
        for s in sites {
            dir.upsert(s);
        }
        dir
    }

    /// Adds or replaces the site for `site.hostname`.
    pub fn upsert(&self, site: Site) {
        self.sites.write().insert(key(&site.hostname), Arc::new(site));
    }
}

impl Directory for StaticDirectory {
    fn site(&self, host: &str) -> Option<Arc<Site>> {
        self.sites.read().get(host).cloned()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum DirectoryError {
    #[error("control plane request: {0}")]
    Http(#[from] reqwest::Error),
    #[error("control plane answered {0}")]
    Status(u16),
}

/// Sites polled from the control plane ([`routes::SITES`], `?since=<version>`).
///
/// The first poll (and any poll after the control plane's version went backwards) fetches the
/// full snapshot and replaces the map; later polls fetch only changed hostnames and upsert
/// them. Hostnames the control plane stops serving arrive as `tombstoned`, so a stale entry
/// never keeps routing to a former tenant. On errors the last known state keeps serving.
pub struct PollingDirectory {
    http: reqwest::Client,
    url: String,
    token: String,
    sites: RwLock<Map>,
    version: Mutex<Option<u64>>,
}

impl PollingDirectory {
    /// `control_plane` is the base URL (`https://cp.internal:8443`); `token` the service token.
    pub fn new(control_plane: &str, token: impl Into<String>) -> Result<Self, DirectoryError> {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(15)).build()?;
        Ok(Self {
            http,
            url: format!("{}{}", control_plane.trim_end_matches('/'), routes::SITES),
            token: token.into(),
            sites: RwLock::new(HashMap::new()),
            version: Mutex::new(None),
        })
    }

    /// Last version applied; `None` before the first successful poll.
    pub fn version(&self) -> Option<u64> {
        *self.version.lock()
    }

    /// One poll; returns the version now applied.
    pub async fn poll(&self) -> Result<u64, DirectoryError> {
        let since = self.version();
        let got = self.fetch(since).await?;
        match since {
            Some(v) if got.version < v => {
                let full = self.fetch(None).await?;
                Ok(self.apply(full, true))
            }
            Some(_) => Ok(self.apply(got, false)),
            None => Ok(self.apply(got, true)),
        }
    }

    /// Polls every `every` forever, logging failures.
    pub fn spawn(self: Arc<Self>, every: Duration) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(every);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                if let Err(e) = self.poll().await {
                    tracing::warn!(error = %e, "site poll failed; serving last known sites");
                }
            }
        })
    }

    async fn fetch(&self, since: Option<u64>) -> Result<Sites, DirectoryError> {
        let url = match since {
            Some(v) => format!("{}?since={v}", self.url),
            None => self.url.clone(),
        };
        let resp = self.http.get(url).bearer_auth(&self.token).send().await?;
        if !resp.status().is_success() {
            return Err(DirectoryError::Status(resp.status().as_u16()));
        }
        Ok(resp.json().await?)
    }

    fn apply(&self, got: Sites, replace: bool) -> u64 {
        let mut map = self.sites.write();
        let mut next: Map = if replace { HashMap::with_capacity(got.sites.len()) } else { std::mem::take(&mut *map) };
        for site in got.sites {
            let k = key(&site.hostname);
            // Keep the existing Arc when nothing changed so per-site caches stay warm.
            let entry = match map.get(&k).or_else(|| next.get(&k)) {
                Some(old) if **old == site => old.clone(),
                _ => Arc::new(site),
            };
            next.insert(k, entry);
        }
        *map = next;
        drop(map);
        *self.version.lock() = Some(got.version);
        got.version
    }
}

impl Directory for PollingDirectory {
    fn site(&self, host: &str) -> Option<Arc<Site>> {
        self.sites.read().get(host).cloned()
    }
}
