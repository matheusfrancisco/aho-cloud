//! `GET /internal/v1/sites`: everything the edge serves, derived from verified domain bindings,
//! active releases, runtime status, each release's `edge.toml`, and auth secrets. Versioned: a
//! persisted index remembers each hostname's content hash, so `?since=v` returns only what
//! changed after `v`, and a hostname that stops being served comes back once as `tombstoned`.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet};

use cloud_api::internal::{RunState, Site, SiteState, Sites};
use cloud_contract::edge::EdgeConfig;
use cloud_contract::ledger::Stage;
use cloud_contract::manifest::{Auth, Expose};
use cloud_domains::{Registry, Serving};
use serde::{Deserialize, Serialize};

use crate::error::AppError;
use crate::ops::ledger_state;
use crate::state::AppState;
use crate::store::ProjectRec;

/// Secret whose per-stage value is the shared edge password (`auth.mode = password`).
pub const EDGE_PASSWORD: &str = "EDGE_PASSWORD";
/// Secret whose per-stage value is the OIDC client secret (`auth.mode = oidc`).
pub const OIDC_CLIENT_SECRET: &str = "OIDC_CLIENT_SECRET";
/// Secrets named with this prefix are project service tokens for `bearer` routes.
pub const EDGE_BEARER_TOKEN_PREFIX: &str = "EDGE_BEARER_TOKEN";

const INDEX_KEY: &str = "sites.index";

#[derive(Default, Serialize, Deserialize)]
struct Index {
    version: u64,
    hosts: BTreeMap<String, IndexEntry>,
}

#[derive(Serialize, Deserialize)]
struct IndexEntry {
    /// Keyed blake3 of the site JSON (the site carries secrets; the index must not).
    hash: String,
    project: String,
    /// Version at which this hostname last changed.
    changed: u64,
    present: bool,
}

fn tombstone(hostname: &str, project: &str) -> Site {
    Site {
        hostname: hostname.to_owned(),
        project: project.to_owned(),
        stage: Stage::Live,
        state: SiteState::Tombstoned,
        upstreams: Vec::new(),
        edge: EdgeConfig::default(),
        auth: Auth::None,
        password_hash: None,
        oidc_client_secret: None,
        verifier_secrets: BTreeMap::new(),
        service_token_hashes: Vec::new(),
        allowed_emails: Vec::new(),
    }
}

/// The site for `release` active at `stage` of `project`, served at `hostname`.
async fn site(
    state: &AppState,
    project: &ProjectRec,
    stage: Stage,
    release: &str,
    hostname: &str,
    ledger: &cloud_ledger::LedgerState,
) -> Result<Option<Site>, AppError> {
    let Some(rs) = ledger.releases.get(release) else {
        return Ok(None);
    };
    let manifest = &rs.manifest;
    let meta = state.store.release_meta(&project.slug, release).await?;
    let edge = match meta.as_ref().and_then(|m| m.edge_toml.as_deref()) {
        Some(text) => EdgeConfig::parse(text).map_err(|e| AppError::internal(format!("stored edge.toml of {release}: {e}")))?,
        None => EdgeConfig::implied_by(manifest),
    };
    let status = state.services.runtime.status(&project.slug, stage).await?;
    let public: BTreeSet<&str> = manifest.processes.iter().filter(|p| p.expose == Expose::Public).map(|p| p.name.as_str()).collect();
    let (run, upstreams) = match status.filter(|s| s.release == release) {
        Some(s) => {
            (s.state, s.processes.iter().filter(|p| public.contains(p.name.as_str())).flat_map(|p| p.addresses.iter().cloned()).collect())
        }
        None => (RunState::Starting, Vec::new()),
    };
    let site_state = match run {
        RunState::Maintenance => SiteState::Maintenance,
        RunState::Sleeping => SiteState::Sleeping,
        RunState::Starting | RunState::Running | RunState::Degraded | RunState::Failed => SiteState::Active,
    };

    let mut values: BTreeMap<String, String> = BTreeMap::new();
    let mut password_hash = None;
    for s in state.store.secrets(&project.slug).await?.into_iter().filter(|s| s.stage == stage) {
        if s.name == EDGE_PASSWORD {
            password_hash = s.edge_phc.clone();
        }
        values.insert(s.name.clone(), state.secrets.open(&project.slug, &s.name, stage, &s.ciphertext)?);
    }
    let verifier_secrets = edge
        .routes
        .iter()
        .filter_map(|r| r.verify.as_ref())
        .filter_map(|v| values.get(v.secret()).map(|val| (v.secret().to_owned(), val.clone())))
        .collect();
    let oidc_client_secret = if matches!(manifest.auth, Auth::Oidc { .. }) { values.get(OIDC_CLIENT_SECRET).cloned() } else { None };
    let service_token_hashes = values
        .iter()
        .filter(|(k, _)| k.starts_with(EDGE_BEARER_TOKEN_PREFIX))
        .map(|(_, v)| blake3::hash(v.as_bytes()).to_hex().to_string())
        .collect();
    let allowed_emails = if stage == Stage::Live {
        Vec::new()
    } else {
        let mut emails = Vec::new();
        for m in state.store.members(&project.org).await? {
            if let Some(u) = state.store.user(&m.user_id).await? {
                emails.push(u.email);
            }
        }
        emails.sort();
        emails
    };
    Ok(Some(Site {
        hostname: hostname.to_owned(),
        project: project.slug.clone(),
        stage,
        state: site_state,
        upstreams,
        edge,
        auth: manifest.auth.clone(),
        password_hash: if matches!(manifest.auth, Auth::Password) { password_hash } else { None },
        oidc_client_secret,
        verifier_secrets,
        service_token_hashes,
        allowed_emails,
    }))
}

fn declares(ledger: &cloud_ledger::LedgerState, stage: Stage, hostname: &str) -> Option<String> {
    let release = ledger.active.get(&stage)?;
    let rs = ledger.releases.get(release)?;
    rs.manifest.domains.iter().any(|d| cloud_domains::normalize_name(&d.hostname) == hostname).then(|| release.clone())
}

/// Every site served right now, by hostname.
pub async fn current(state: &AppState) -> Result<BTreeMap<String, Site>, AppError> {
    let projects = state.store.projects().await?;
    let registry: Registry = state.registry.lock().await.clone();
    let mut out = BTreeMap::new();
    let mut hostnames = BTreeSet::new();
    for p in &projects {
        hostnames.extend(registry.for_project(&p.slug).into_iter().map(|b| b.hostname.clone()));
    }
    let by_slug: BTreeMap<&str, &ProjectRec> = projects.iter().map(|p| (p.slug.as_str(), p)).collect();
    let mut ledgers: BTreeMap<String, cloud_ledger::LedgerState> = BTreeMap::new();
    for host in hostnames {
        match registry.serving(&host) {
            Serving::Project(b) => {
                let Some(project) = by_slug.get(b.project.as_str()).filter(|p| p.deleted_at.is_none()) else {
                    out.insert(host.clone(), tombstone(&host, &b.project));
                    continue;
                };
                let ledger = match ledgers.entry(project.slug.clone()) {
                    Entry::Occupied(e) => e.into_mut(),
                    Entry::Vacant(e) => e.insert(ledger_state(state, &project.slug)?),
                };
                // A verified domain serves only a release whose manifest declares it: adding a
                // domain is a signed widening.
                let chosen = declares(ledger, Stage::Live, &host)
                    .map(|r| (Stage::Live, r))
                    .or_else(|| declares(ledger, Stage::Staging, &host).map(|r| (Stage::Staging, r)));
                if let Some((stage, release)) = chosen
                    && let Some(site) = site(state, project, stage, &release, &host, ledger).await?
                {
                    out.insert(host.clone(), site);
                }
            }
            Serving::Tombstone(b) => {
                out.insert(host.clone(), tombstone(&host, &b.project));
            }
            Serving::Unbound => {}
        }
    }
    for p in &projects {
        let host = state.cfg.preview_host(&p.slug);
        if p.deleted_at.is_some() {
            out.insert(host.clone(), tombstone(&host, &p.slug));
            continue;
        }
        let ledger = match ledgers.remove(&p.slug) {
            Some(l) => l,
            None => ledger_state(state, &p.slug)?,
        };
        if let Some(release) = ledger.active.get(&Stage::Preview)
            && let Some(site) = site(state, p, Stage::Preview, release, &host, &ledger).await?
        {
            out.insert(host, site);
        }
    }
    Ok(out)
}

fn site_hash(key: &[u8; 32], site: &Site) -> String {
    blake3::keyed_hash(key, &cloud_contract::canonical::to_bytes(site)).to_hex().to_string()
}

/// The versioned snapshot (`since = None`) or delta (`since = Some(v)`).
pub async fn sites(state: &AppState, since: Option<u64>) -> Result<Sites, AppError> {
    let _serialized = state.sites_lock.lock().await;
    let current = current(state).await?;
    let mut index: Index = match state.store.kv_get(INDEX_KEY).await? {
        Some(text) => serde_json::from_str(&text).map_err(|e| AppError::internal(format!("sites index: {e}")))?,
        None => Index::default(),
    };
    let key = state.cfg.master_key.derive("aho-cloud sites index v1");
    let next = index.version + 1;
    let mut changed = false;
    for (host, site) in &current {
        let hash = site_hash(&key, site);
        let same = index.hosts.get(host).is_some_and(|e| e.present && e.hash == hash);
        if !same {
            index.hosts.insert(host.clone(), IndexEntry { hash, project: site.project.clone(), changed: next, present: true });
            changed = true;
        }
    }
    for (host, e) in &mut index.hosts {
        if e.present && !current.contains_key(host) {
            e.present = false;
            e.changed = next;
            changed = true;
        }
    }
    if changed {
        index.version = next;
        let text = serde_json::to_string(&index).map_err(|e| AppError::internal(e.to_string()))?;
        state.store.kv_put(INDEX_KEY, &text).await?;
    }
    let sites = match since {
        None => current.into_values().collect(),
        Some(since) => index
            .hosts
            .iter()
            .filter(|(_, e)| e.changed > since)
            .map(|(host, e)| match current.get(host) {
                Some(site) if e.present => site.clone(),
                _ => tombstone(host, &e.project),
            })
            .collect(),
    };
    Ok(Sites { version: index.version, sites })
}
