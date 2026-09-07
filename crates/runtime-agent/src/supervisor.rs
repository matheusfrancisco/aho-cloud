//! Desired state per (project, stage) and the loop that makes it true: boot missing replicas,
//! replace ones from an old release (or with rotated secrets), stop extras, restart crashed or
//! persistently unhealthy ones, and report [`DeploymentStatus`].
//!
//! API calls only record intent and return at once; [`Supervisor::reconcile`] (driven by
//! [`Supervisor::run`]) does the work, so a slow image pull never blocks the API.

use std::collections::{BTreeMap, BTreeSet};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cloud_api::internal::{Deployment, DeploymentStatus, ProcessStatus, RunState};
use cloud_contract::ledger::Stage;
use cloud_contract::manifest::{Health, Process};
use futures::future::join_all;
use parking_lot::Mutex;
use tokio::sync::Notify;

use crate::egress::{EgressPolicies, GuestPolicy};
use crate::guest::{GuestConfig, ImageConfig, VOLUME_DEVICE, VolumeMount};
use crate::health;
use crate::net::{Allocator, Lease};
use crate::vmm::{ReplicaSpec, Vmm, VolumeSpec};

#[derive(Clone, Debug)]
pub struct SupervisorConfig {
    /// Egress proxy port on the gateway, exported to guests as `HTTP(S)_PROXY`.
    pub proxy_port: u16,
    /// Upper bound for [`Supervisor::scale`].
    pub max_replicas: u16,
    pub health_timeout: Duration,
    /// Consecutive failed checks before a replica is replaced.
    pub unhealthy_threshold: u32,
    /// After boot, failed checks do not count until the replica was healthy once or this passed.
    pub startup_grace: Duration,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        SupervisorConfig {
            proxy_port: 3128,
            max_replicas: 16,
            health_timeout: Duration::from_secs(2),
            unhealthy_threshold: 3,
            startup_grace: Duration::from_secs(60),
        }
    }
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum SupervisorError {
    #[error("no deployment for {0}/{1}")]
    NotFound(String, &'static str),
    #[error("the deployment has no process `{0}`")]
    UnknownProcess(String),
    #[error("at most {max} replicas per process")]
    TooManyReplicas { max: u16 },
    #[error("{0}")]
    Invalid(String),
}

type Key = (String, Stage);

/// Why a replica is being replaced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Replace {
    /// `restart`: keep serving until the replacement is healthy.
    Requested,
    /// Failed its health checks past the threshold: stop at once.
    Unhealthy,
}

struct Replica {
    id: String,
    process: String,
    index: u16,
    /// Fingerprint of the deployment it was booted from.
    generation: String,
    /// Port and health check it was booted with (a newer release may change them).
    port: Option<u16>,
    health: Health,
    lease: Lease,
    started_at: Instant,
    /// Set once `Vmm::start` returned.
    booted: bool,
    healthy: bool,
    ever_healthy: bool,
    failures: u32,
    replace: Option<Replace>,
}

struct Entry {
    deployment: Deployment,
    generation: String,
    desired: BTreeMap<String, u16>,
    maintenance: bool,
    sleeping: bool,
    removed: bool,
    replicas: Vec<Replica>,
    /// Slots whose last attempt failed (start error, crash, failed health checks), cleared
    /// when the slot is healthy again.
    failing: BTreeMap<(String, u16), String>,
}

impl Entry {
    fn is_current(&self, r: &Replica) -> bool {
        r.generation == self.generation && r.replace.is_none() && self.desired.get(&r.process).is_some_and(|&n| r.index < n)
    }
}

struct State {
    entries: BTreeMap<Key, Entry>,
    allocator: Allocator,
}

struct Inner<V> {
    vmm: V,
    cfg: SupervisorConfig,
    egress: EgressPolicies,
    state: Mutex<State>,
    kick: Notify,
    pass: tokio::sync::Mutex<()>,
}

/// The per-host supervisor. Cheap to clone.
pub struct Supervisor<V> {
    inner: Arc<Inner<V>>,
}

impl<V> Clone for Supervisor<V> {
    fn clone(&self) -> Self {
        Supervisor { inner: self.inner.clone() }
    }
}

/// What a deployment's replicas are started from; any change replaces them.
fn generation(d: &Deployment) -> String {
    let doc = serde_json::json!([d.release, d.image_digest, d.manifest, d.secrets]);
    blake3::hash(doc.to_string().as_bytes()).to_hex().to_string()
}

fn new_id() -> String {
    let id = uuid::Uuid::new_v4().simple().to_string();
    format!("vm-{}", &id[..12])
}

/// The config drive for one replica (the backend adds the image's own config).
pub fn guest_config(d: &Deployment, process: &Process, lease: &Lease, proxy_port: u16) -> GuestConfig {
    let mut env = BTreeMap::new();
    for secret in &d.manifest.secrets {
        if let Some(value) = d.secrets.get(&secret.name) {
            env.insert(secret.name.clone(), value.clone());
        }
    }
    if let Some(port) = process.port {
        env.insert("PORT".to_owned(), port.to_string());
    }
    let proxy = format!("http://{}:{proxy_port}", lease.gateway);
    for key in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
        env.insert(key.to_owned(), proxy.clone());
    }
    for key in ["NO_PROXY", "no_proxy"] {
        env.insert(key.to_owned(), "localhost,127.0.0.1,::1".to_owned());
    }
    GuestConfig {
        process: process.name.clone(),
        command: process.command.clone(),
        image: ImageConfig::default(),
        env,
        port: process.port,
        health: process.health.clone(),
        volume: process.volume.as_ref().map(|v| VolumeMount { device: VOLUME_DEVICE.to_owned(), mount: v.mount.clone() }),
        dns: vec![lease.gateway],
    }
}

/// One replica as seen at the start of a pass.
struct Seen {
    id: String,
    process: String,
    index: u16,
    ip: IpAddr,
    current: bool,
    booted: bool,
    healthy: bool,
    broken: bool,
}

/// Input of one reconcile pass for one deployment.
struct Plan {
    seen: Vec<Seen>,
    /// Sleeping or removed.
    stop_all: bool,
    desired: BTreeMap<String, u16>,
    /// Processes with a volume.
    volumes: BTreeSet<String>,
}

impl<V: Vmm> Supervisor<V> {
    pub fn new(vmm: V, allocator: Allocator, egress: EgressPolicies, cfg: SupervisorConfig) -> Self {
        Supervisor {
            inner: Arc::new(Inner {
                vmm,
                cfg,
                egress,
                state: Mutex::new(State { entries: BTreeMap::new(), allocator }),
                kick: Notify::new(),
                pass: tokio::sync::Mutex::new(()),
            }),
        }
    }

    fn status_of(&self, entry: &Entry) -> DeploymentStatus {
        let d = &entry.deployment;
        let mut processes = Vec::new();
        let mut running = 0usize;
        let mut healthy_total = 0usize;
        let mut unproven = false;
        for p in &d.manifest.processes {
            let current: Vec<&Replica> = entry.replicas.iter().filter(|r| r.process == p.name && entry.is_current(r)).collect();
            let healthy: Vec<&Replica> = current.iter().copied().filter(|r| r.healthy).collect();
            running += current.len();
            healthy_total += healthy.len();
            unproven |= current.iter().any(|r| !r.ever_healthy);
            // Where to send traffic now: healthy replicas, including predecessors still
            // serving while their successors come up.
            let addresses = entry
                .replicas
                .iter()
                .filter(|r| r.process == p.name && r.healthy)
                .filter_map(|r| r.port.map(|port| self.inner.vmm.endpoint(&r.id, r.lease.ip, port).to_string()))
                .collect();
            processes.push(ProcessStatus {
                name: p.name.clone(),
                replicas: u16::try_from(current.len()).unwrap_or(u16::MAX),
                healthy: u16::try_from(healthy.len()).unwrap_or(u16::MAX),
                addresses,
            });
        }
        let desired: usize = entry.desired.values().map(|&n| usize::from(n)).sum();
        let failing = entry.failing.keys().any(|(p, i)| entry.desired.get(p).is_some_and(|&n| *i < n));
        let state = if entry.sleeping {
            RunState::Sleeping
        } else if entry.maintenance {
            RunState::Maintenance
        } else if running == desired && healthy_total == desired {
            RunState::Running
        } else if failing {
            if healthy_total == 0 { RunState::Failed } else { RunState::Degraded }
        } else if running < desired || unproven {
            RunState::Starting
        } else {
            RunState::Degraded
        };
        DeploymentStatus { project: d.project.clone(), stage: d.stage, release: d.release.clone(), state, processes }
    }

    fn with_entry<T>(
        &self,
        project: &str,
        stage: Stage,
        f: impl FnOnce(&mut Entry) -> Result<T, SupervisorError>,
    ) -> Result<(T, DeploymentStatus), SupervisorError> {
        let mut state = self.inner.state.lock();
        let entry = state
            .entries
            .get_mut(&(project.to_owned(), stage))
            .filter(|e| !e.removed)
            .ok_or_else(|| SupervisorError::NotFound(project.to_owned(), stage.as_str()))?;
        let out = f(entry)?;
        let status = self.status_of(entry);
        drop(state);
        self.inner.kick.notify_one();
        Ok((out, status))
    }

    /// Records `d` as the desired state for its (project, stage). Replica counts carry over
    /// for processes that still exist (new ones get 1); maintenance is kept; sleep is lifted.
    pub fn apply(&self, d: Deployment) -> Result<DeploymentStatus, SupervisorError> {
        if d.project != d.manifest.project {
            return Err(SupervisorError::Invalid(format!(
                "deployment project `{}` differs from manifest project `{}`",
                d.project, d.manifest.project
            )));
        }
        let key = (d.project.clone(), d.stage);
        let generation = generation(&d);
        let mut state = self.inner.state.lock();
        let entry = state.entries.entry(key).or_insert_with(|| Entry {
            deployment: d.clone(),
            generation: String::new(),
            desired: BTreeMap::new(),
            maintenance: false,
            sleeping: false,
            removed: false,
            replicas: Vec::new(),
            failing: BTreeMap::new(),
        });
        let desired = d
            .manifest
            .processes
            .iter()
            .map(|p| (p.name.clone(), if entry.removed { 1 } else { entry.desired.get(&p.name).copied().unwrap_or(1) }))
            .collect();
        entry.desired = desired;
        entry.deployment = d;
        entry.generation = generation;
        entry.sleeping = false;
        entry.removed = false;
        entry.failing.clear();
        let status = self.status_of(entry);
        drop(state);
        self.inner.kick.notify_one();
        Ok(status)
    }

    pub fn status(&self, project: &str, stage: Stage) -> Result<DeploymentStatus, SupervisorError> {
        let state = self.inner.state.lock();
        state
            .entries
            .get(&(project.to_owned(), stage))
            .filter(|e| !e.removed)
            .map(|e| self.status_of(e))
            .ok_or_else(|| SupervisorError::NotFound(project.to_owned(), stage.as_str()))
    }

    /// Sets the replica count of one process.
    pub fn scale(&self, project: &str, stage: Stage, process: &str, replicas: u16) -> Result<DeploymentStatus, SupervisorError> {
        let max = self.inner.cfg.max_replicas;
        self.with_entry(project, stage, |e| {
            if e.deployment.manifest.process(process).is_none() {
                return Err(SupervisorError::UnknownProcess(process.to_owned()));
            }
            if replicas > max {
                return Err(SupervisorError::TooManyReplicas { max });
            }
            e.desired.insert(process.to_owned(), replicas);
            Ok(())
        })
        .map(|((), s)| s)
    }

    /// Replaces every replica; each keeps serving until its replacement is healthy.
    pub fn restart(&self, project: &str, stage: Stage) -> Result<DeploymentStatus, SupervisorError> {
        self.with_entry(project, stage, |e| {
            for r in &mut e.replicas {
                r.replace.get_or_insert(Replace::Requested);
            }
            Ok(())
        })
        .map(|((), s)| s)
    }

    /// Maintenance keeps every replica running; only the reported state changes (the edge
    /// serves its maintenance page).
    pub fn maintenance(&self, project: &str, stage: Stage, on: bool) -> Result<DeploymentStatus, SupervisorError> {
        self.with_entry(project, stage, |e| {
            e.maintenance = on;
            Ok(())
        })
        .map(|((), s)| s)
    }

    /// Stops every replica until the next [`Supervisor::apply`].
    pub fn sleep(&self, project: &str, stage: Stage) -> Result<DeploymentStatus, SupervisorError> {
        self.with_entry(project, stage, |e| {
            e.sleeping = true;
            Ok(())
        })
        .map(|((), s)| s)
    }

    /// Stops every replica and forgets the deployment (volumes stay on disk).
    pub fn remove(&self, project: &str, stage: Stage) -> Result<(), SupervisorError> {
        self.with_entry(project, stage, |e| {
            e.removed = true;
            Ok(())
        })
        .map(drop)
    }

    /// Reconciles until the task is dropped: after every change, and every `interval`.
    pub async fn run(&self, interval: Duration) {
        loop {
            self.reconcile().await;
            tokio::select! {
                () = self.inner.kick.notified() => {}
                () = tokio::time::sleep(interval) => {}
            }
        }
    }

    /// One pass over every deployment (concurrently); passes never overlap.
    pub async fn reconcile(&self) {
        let _pass = self.inner.pass.lock().await;
        let keys: Vec<Key> = self.inner.state.lock().entries.keys().cloned().collect();
        join_all(keys.iter().map(|k| self.reconcile_one(k))).await;
    }

    async fn reconcile_one(&self, key: &Key) {
        let Some(plan) = self.snapshot(key) else {
            return;
        };

        // Liveness: a VM that died is replaced like an outdated one. Passes never overlap, so
        // every seen replica has finished booting.
        let alive: Vec<bool> = join_all(plan.seen.iter().map(|r| async { r.booted && self.inner.vmm.running(&r.id).await })).await;

        let mut keep: BTreeSet<(String, u16)> = BTreeSet::new();
        let mut handed_over: BTreeSet<(String, u16)> = BTreeSet::new();
        let mut stale: Vec<(&Seen, bool)> = Vec::new();
        let mut crashed: Vec<(String, u16)> = Vec::new();
        for (r, &up) in plan.seen.iter().zip(&alive) {
            let slot = (r.process.clone(), r.index);
            if !plan.stop_all && up && r.current && !keep.contains(&slot) {
                if r.healthy {
                    handed_over.insert(slot.clone());
                }
                keep.insert(slot);
                continue;
            }
            if !up {
                tracing::warn!(id = %r.id, process = %r.process, "replica is not running");
                if r.current {
                    crashed.push(slot.clone());
                }
            }
            stale.push((r, up));
        }
        let missing: Vec<(String, u16)> = if plan.stop_all {
            Vec::new()
        } else {
            plan.desired.iter().flat_map(|(p, &n)| (0..n).map(move |i| (p.clone(), i))).filter(|slot| !keep.contains(slot)).collect()
        };
        let desired_slot = |slot: &(String, u16)| plan.desired.get(&slot.0).is_some_and(|&n| slot.1 < n);

        // A volume is attached to one VM at a time, so its holder stops before the successor
        // boots. Otherwise the successor boots first and the predecessor keeps serving until
        // the successor is healthy: a release that never gets healthy never takes the old
        // one down.
        let mut first = Vec::new();
        let mut later = Vec::new();
        for (r, up) in stale {
            let slot = (r.process.clone(), r.index);
            if plan.stop_all || !up || (plan.volumes.contains(&r.process) && missing.contains(&slot)) {
                first.push(r);
            } else if !desired_slot(&slot) || r.broken || handed_over.contains(&slot) {
                later.push(r);
            }
        }
        if !crashed.is_empty()
            && let Some(entry) = self.inner.state.lock().entries.get_mut(key)
        {
            for slot in crashed {
                entry.failing.insert(slot, "exited".to_owned());
            }
        }
        join_all(first.iter().map(|r| self.stop_replica(key, &r.id, r.ip))).await;
        join_all(missing.iter().map(|(p, i)| self.start_replica(key, p, *i))).await;
        join_all(later.iter().map(|r| self.stop_replica(key, &r.id, r.ip))).await;

        self.check_health(key).await;

        let mut state = self.inner.state.lock();
        if state.entries.get(key).is_some_and(|e| e.removed && e.replicas.is_empty()) {
            state.entries.remove(key);
        }
    }

    /// Replicas as of now, and the targets.
    fn snapshot(&self, key: &Key) -> Option<Plan> {
        let state = self.inner.state.lock();
        let entry = state.entries.get(key)?;
        let seen = entry
            .replicas
            .iter()
            .map(|r| Seen {
                id: r.id.clone(),
                process: r.process.clone(),
                index: r.index,
                ip: IpAddr::V4(r.lease.ip),
                current: entry.is_current(r),
                booted: r.booted,
                healthy: r.healthy,
                broken: r.replace == Some(Replace::Unhealthy),
            })
            .collect();
        let volumes = entry.deployment.manifest.processes.iter().filter(|p| p.volume.is_some()).map(|p| p.name.clone()).collect();
        Some(Plan { seen, stop_all: entry.sleeping || entry.removed, desired: entry.desired.clone(), volumes })
    }

    async fn stop_replica(&self, key: &Key, id: &str, ip: IpAddr) {
        match self.inner.vmm.stop(id).await {
            Ok(()) => {
                let mut state = self.inner.state.lock();
                let State { entries, allocator } = &mut *state;
                if let Some(entry) = entries.get_mut(key)
                    && let Some(pos) = entry.replicas.iter().position(|r| r.id == id)
                {
                    let r = entry.replicas.remove(pos);
                    allocator.release(r.lease.ip);
                }
                self.inner.egress.remove(ip);
                tracing::info!(id, "replica stopped");
            }
            Err(e) => tracing::warn!(id, error = %e, "stopping replica failed; will retry"),
        }
    }

    async fn start_replica(&self, key: &Key, process: &str, index: u16) {
        let spec = {
            let mut state = self.inner.state.lock();
            let State { entries, allocator } = &mut *state;
            let Some(entry) = entries.get_mut(key) else {
                return;
            };
            let d = &entry.deployment;
            let Some(p) = d.manifest.process(process) else {
                return;
            };
            let lease = match allocator.allocate() {
                Ok(lease) => lease,
                Err(e) => {
                    tracing::warn!(process, error = %e, "no address for replica");
                    entry.failing.insert((process.to_owned(), index), e.to_string());
                    return;
                }
            };
            let spec = ReplicaSpec {
                id: new_id(),
                project: d.project.clone(),
                stage: d.stage,
                process: p.name.clone(),
                index,
                release: d.release.clone(),
                image_digest: d.image_digest.clone(),
                guest: guest_config(d, p, &lease, self.inner.cfg.proxy_port),
                volume: p.volume.as_ref().map(|v| VolumeSpec { size_mb: v.size_mb }),
                lease: lease.clone(),
            };
            self.inner
                .egress
                .set(IpAddr::V4(lease.ip), GuestPolicy { project: d.project.clone(), stage: d.stage, allow: d.manifest.egress.clone() });
            entry.replicas.push(Replica {
                id: spec.id.clone(),
                process: p.name.clone(),
                index,
                generation: entry.generation.clone(),
                port: p.port,
                health: p.health.clone(),
                lease,
                started_at: Instant::now(),
                booted: false,
                healthy: false,
                ever_healthy: false,
                failures: 0,
                replace: None,
            });
            spec
        };
        let id = spec.id.clone();
        let ip = spec.lease.ip;
        let result = self.inner.vmm.start(spec).await;
        let mut state = self.inner.state.lock();
        let State { entries, allocator } = &mut *state;
        let entry = entries.get_mut(key);
        match result {
            Ok(_) => {
                if let Some(r) = entry.and_then(|e| e.replicas.iter_mut().find(|r| r.id == id)) {
                    r.booted = true;
                    r.started_at = Instant::now();
                }
            }
            Err(e) => {
                tracing::warn!(%id, process, error = %e, "replica failed to start");
                if let Some(entry) = entry {
                    entry.replicas.retain(|r| r.id != id);
                    entry.failing.insert((process.to_owned(), index), e.to_string());
                }
                allocator.release(ip);
                self.inner.egress.remove(IpAddr::V4(ip));
            }
        }
    }

    /// Probes every booted replica (predecessors too, so their traffic share stays accurate);
    /// only current ones are replaced for failing.
    async fn check_health(&self, key: &Key) {
        let probes: Vec<(String, Lease, Health, Option<u16>)> = {
            let state = self.inner.state.lock();
            let Some(entry) = state.entries.get(key) else {
                return;
            };
            entry.replicas.iter().filter(|r| r.booted).map(|r| (r.id.clone(), r.lease.clone(), r.health.clone(), r.port)).collect()
        };
        let timeout = self.inner.cfg.health_timeout;
        let results = join_all(probes.iter().map(|(id, lease, check, port)| async move {
            match check {
                Health::Http { path, port: health_port } => match health_port.or(*port) {
                    Some(port) => health::http_ok(self.inner.vmm.endpoint(id, lease.ip, port), path, timeout).await,
                    None => false,
                },
                Health::Command { argv } => match self.inner.vmm.connect(id).await {
                    Ok(conn) => health::command_ok(conn, argv, timeout).await,
                    Err(_) => false,
                },
            }
        }))
        .await;

        let grace = self.inner.cfg.startup_grace;
        let threshold = self.inner.cfg.unhealthy_threshold.max(1);
        let mut state = self.inner.state.lock();
        let Some(entry) = state.entries.get_mut(key) else {
            return;
        };
        for ((id, ..), ok) in probes.iter().zip(results) {
            let Some(pos) = entry.replicas.iter().position(|r| &r.id == id) else {
                continue;
            };
            let current = entry.is_current(&entry.replicas[pos]);
            let r = &mut entry.replicas[pos];
            let slot = (r.process.clone(), r.index);
            if ok {
                r.healthy = true;
                r.ever_healthy = true;
                r.failures = 0;
                if current {
                    entry.failing.remove(&slot);
                }
                continue;
            }
            r.healthy = false;
            if current && (r.ever_healthy || r.started_at.elapsed() >= grace) {
                r.failures += 1;
                if r.failures >= threshold {
                    tracing::warn!(id = %r.id, failures = r.failures, "replica unhealthy; replacing");
                    r.replace = Some(Replace::Unhealthy);
                    entry.failing.insert(slot, "failed its health checks".to_owned());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use cloud_contract::manifest::{Expose, Secret, Volume};

    use super::*;

    fn process(name: &str) -> Process {
        Process {
            name: name.into(),
            command: None,
            port: Some(8080),
            expose: Expose::Public,
            health: Health::Http { path: "/".into(), port: None },
            volume: Some(Volume { mount: "/data".into(), size_mb: 64 }),
        }
    }

    #[test]
    fn guest_env_carries_declared_secrets_port_and_proxy_only() {
        let manifest_json = serde_json::json!({
            "api_version": "deploy.aho.cloud/v1", "project": "shop",
            "source": {"kind": "oci", "digest": format!("sha256:{}", "a".repeat(64))},
            "build": {"kind": "prebuilt"}, "processes": [], "auth": {"mode": "none"},
            "data_class": "none", "owner": {"name": "A", "email": "a@x.com"},
            "budget": {"ceiling_usd_month": 5, "degrade": "sleep"}
        });
        let mut manifest: cloud_contract::Manifest = serde_json::from_value(manifest_json).unwrap();
        manifest.secrets = vec![Secret { name: "STRIPE_KEY".into(), reach: vec!["api.stripe.com".into()], expires: None }];
        let d = Deployment {
            project: "shop".into(),
            stage: Stage::Staging,
            release: "r1".into(),
            image_digest: "sha256:x".into(),
            manifest,
            secrets: BTreeMap::from([
                ("STRIPE_KEY".into(), "aho-cloud-placeholder:STRIPE_KEY".into()),
                ("UNDECLARED".into(), "leak".into()),
            ]),
        };
        let mut alloc = Allocator::new("10.9.9.0/24".parse().unwrap()).unwrap();
        let lease = alloc.allocate().unwrap();
        let g = guest_config(&d, &process("web"), &lease, 3128);
        assert_eq!(g.env["STRIPE_KEY"], "aho-cloud-placeholder:STRIPE_KEY");
        assert!(!g.env.contains_key("UNDECLARED"));
        assert_eq!(g.env["PORT"], "8080");
        assert_eq!(g.env["HTTPS_PROXY"], "http://10.9.9.1:3128");
        assert_eq!(g.dns, [lease.gateway]);
        assert_eq!(g.volume, Some(VolumeMount { device: "/dev/vdc".into(), mount: "/data".into() }));

        let mut rotated = d.clone();
        rotated.secrets.insert("STRIPE_KEY".into(), "sk_live_new".into());
        assert_ne!(generation(&d), generation(&rotated));
        assert_eq!(generation(&d), generation(&d.clone()));
    }
}
