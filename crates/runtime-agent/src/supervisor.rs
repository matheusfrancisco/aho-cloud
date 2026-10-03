//! Desired state per (project, stage) and per sandbox, and the loop that makes it true.
//!
//! Deployments: boot missing replicas, replace ones from an old release (or with rotated
//! secrets), stop extras, restart crashed or persistently unhealthy ones, and report
//! [`DeploymentStatus`]. Sandboxes: one VM each, wanted running or stopped, replaced when its
//! spec changes (its workspace disk is kept), rebooted when it dies, and its disk destroyed
//! when the sandbox is removed; reported as [`SandboxStatus`]. Both share the address
//! allocator and the egress policies.
//!
//! API calls only record intent and return at once; [`Supervisor::reconcile`] (driven by
//! [`Supervisor::run`]) does the work, so a slow image pull never blocks the API.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use cloud_api::SandboxState;
use cloud_api::internal::{Deployment, DeploymentStatus, ProcessStatus, RunState, SANDBOX_WORKSPACE, SandboxSpec, SandboxStatus};
use cloud_contract::ledger::Stage;
use cloud_contract::manifest::{Health, Process};
use cloud_contract::validate::is_project_slug;
use futures::future::join_all;
use parking_lot::Mutex;
use tokio::sync::Notify;

use crate::egress::{EgressPolicies, GuestPolicy};
use crate::guest::{GuestConfig, ImageConfig, VOLUME_DEVICE, VolumeMount};
use crate::health;
use crate::net::{Allocator, Lease};
use crate::vmm::{DiskId, GuestConn, ReplicaSpec, Vmm, VolumeSpec, labels};

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
    /// Consecutive boot failures (or deaths) after which a sandbox reports `failed`; it is
    /// still retried.
    pub sandbox_failures: u32,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        SupervisorConfig {
            proxy_port: 3128,
            max_replicas: 16,
            health_timeout: Duration::from_secs(2),
            unhealthy_threshold: 3,
            startup_grace: Duration::from_secs(60),
            sandbox_failures: 3,
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
    #[error("no sandbox `{0}`")]
    NoSandbox(String),
    #[error("sandbox `{0}` is not running")]
    NotRunning(String),
    /// The VM backend failed (e.g. opening an exec stream).
    #[error("{0}")]
    Backend(String),
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
    sandboxes: BTreeMap<String, SandboxEntry>,
    /// Slugs whose workspace disk must be destroyed before anything boots from it again.
    wipes: BTreeSet<String>,
    allocator: Allocator,
}

/// One sandbox: what is wanted, and its VM.
struct SandboxEntry {
    spec: SandboxSpec,
    generation: String,
    /// Wanted running (`put_sandbox`) rather than stopped (`stop_sandbox`).
    running: bool,
    /// Deleted: stop, destroy the disk, forget.
    removed: bool,
    vm: Option<SandboxVm>,
    /// Consecutive boot failures and deaths; cleared once a VM is seen running after its boot.
    failures: u32,
    error: Option<String>,
}

struct SandboxVm {
    id: String,
    lease: Lease,
    /// Fingerprint of the spec it was booted from.
    generation: String,
    port: Option<u16>,
    /// Set once `Vmm::start` returned.
    booted: bool,
    /// Seen running by a pass after the one that booted it.
    proven: bool,
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

/// What a sandbox VM is started from; any change replaces it.
fn sandbox_generation(s: &SandboxSpec) -> String {
    blake3::hash(serde_json::json!(s).to_string().as_bytes()).to_hex().to_string()
}

/// The sandbox's VM while it counts as running: wanted, booted from the current spec, and its
/// disk not waiting to be destroyed.
fn running_vm(e: &SandboxEntry, wiping: bool) -> Option<&SandboxVm> {
    e.vm.as_ref().filter(|vm| e.running && !wiping && vm.booted && vm.generation == e.generation)
}

fn new_id() -> String {
    let id = uuid::Uuid::new_v4().simple().to_string();
    format!("vm-{}", &id[..12])
}

/// `PORT` and the egress proxy, over whatever `env` holds.
fn platform_env(env: &mut BTreeMap<String, String>, port: Option<u16>, lease: &Lease, proxy_port: u16) {
    if let Some(port) = port {
        env.insert("PORT".to_owned(), port.to_string());
    }
    let proxy = format!("http://{}:{proxy_port}", lease.gateway);
    for key in ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"] {
        env.insert(key.to_owned(), proxy.clone());
    }
    for key in ["NO_PROXY", "no_proxy"] {
        env.insert(key.to_owned(), "localhost,127.0.0.1,::1".to_owned());
    }
}

/// The config drive for one replica (the backend adds the image's own config).
pub fn guest_config(d: &Deployment, process: &Process, lease: &Lease, proxy_port: u16) -> GuestConfig {
    let mut env = BTreeMap::new();
    for secret in &d.manifest.secrets {
        if let Some(value) = d.secrets.get(&secret.name) {
            env.insert(secret.name.clone(), value.clone());
        }
    }
    platform_env(&mut env, process.port, lease, proxy_port);
    GuestConfig {
        process: process.name.clone(),
        command: process.command.clone(),
        image: ImageConfig::default(),
        env,
        port: process.port,
        volume: process.volume.as_ref().map(|v| VolumeMount { device: VOLUME_DEVICE.to_owned(), mount: v.mount.clone() }),
        dns: vec![lease.gateway],
    }
}

/// The config drive of a sandbox VM: the sandbox's environment under the platform's, its
/// workspace disk at [`SANDBOX_WORKSPACE`], and the image's own (idle) command.
pub fn sandbox_guest_config(s: &SandboxSpec, lease: &Lease, proxy_port: u16) -> GuestConfig {
    let mut env = s.env.clone();
    platform_env(&mut env, s.port, lease, proxy_port);
    GuestConfig {
        process: "sandbox".to_owned(),
        command: None,
        image: ImageConfig::default(),
        env,
        port: s.port,
        volume: Some(VolumeMount { device: VOLUME_DEVICE.to_owned(), mount: SANDBOX_WORKSPACE.to_owned() }),
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
                state: Mutex::new(State { entries: BTreeMap::new(), sandboxes: BTreeMap::new(), wipes: BTreeSet::new(), allocator }),
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

    fn sandbox_status_of(&self, slug: &str, e: &SandboxEntry, wiping: bool) -> SandboxStatus {
        let running = running_vm(e, wiping);
        let failed = e.running && running.is_none() && e.failures >= self.inner.cfg.sandbox_failures.max(1);
        let state = match (e.running, running, failed) {
            (false, ..) => SandboxState::Stopped,
            (true, Some(_), _) => SandboxState::Running,
            (true, None, true) => SandboxState::Failed,
            (true, None, false) => SandboxState::Starting,
        };
        SandboxStatus {
            slug: slug.to_owned(),
            state,
            address: running.and_then(|vm| vm.port.map(|port| self.inner.vmm.endpoint(&vm.id, vm.lease.ip, port).to_string())),
            error: if failed { e.error.clone() } else { None },
        }
    }

    fn with_sandbox<T>(&self, slug: &str, f: impl FnOnce(&mut SandboxEntry) -> T) -> Result<(T, SandboxStatus), SupervisorError> {
        let mut state = self.inner.state.lock();
        let State { sandboxes, wipes, .. } = &mut *state;
        let entry = sandboxes.get_mut(slug).filter(|e| !e.removed).ok_or_else(|| SupervisorError::NoSandbox(slug.to_owned()))?;
        let out = f(entry);
        let status = self.sandbox_status_of(slug, entry, wipes.contains(slug));
        drop(state);
        self.inner.kick.notify_one();
        Ok((out, status))
    }

    /// Wants sandbox `spec.slug` running with `spec`: boots it, or replaces a VM booted from a
    /// different spec (the workspace disk is kept). Lifts a stop and clears earlier failures.
    pub fn put_sandbox(&self, spec: SandboxSpec) -> Result<SandboxStatus, SupervisorError> {
        if !is_project_slug(&spec.slug) {
            return Err(SupervisorError::Invalid(format!("`{}` is not a sandbox slug", spec.slug)));
        }
        let slug = spec.slug.clone();
        let generation = sandbox_generation(&spec);
        let mut state = self.inner.state.lock();
        let State { sandboxes, wipes, .. } = &mut *state;
        let entry = sandboxes.entry(slug.clone()).or_insert_with(|| SandboxEntry {
            spec: spec.clone(),
            generation: String::new(),
            running: true,
            removed: false,
            vm: None,
            failures: 0,
            error: None,
        });
        entry.spec = spec;
        entry.generation = generation;
        entry.running = true;
        entry.removed = false;
        entry.failures = 0;
        entry.error = None;
        let status = self.sandbox_status_of(&slug, entry, wipes.contains(&slug));
        drop(state);
        self.inner.kick.notify_one();
        Ok(status)
    }

    /// Stops the sandbox's VM until the next [`Supervisor::put_sandbox`]; the disk is kept.
    pub fn stop_sandbox(&self, slug: &str) -> Result<SandboxStatus, SupervisorError> {
        self.with_sandbox(slug, |e| {
            e.running = false;
            e.failures = 0;
            e.error = None;
        })
        .map(|((), s)| s)
    }

    /// Stops the sandbox's VM, destroys its disk and forgets it. A slug this agent does not
    /// know still has its disk destroyed (it may predate a restart); either way nothing boots
    /// from that slug's disk before it is gone.
    pub fn remove_sandbox(&self, slug: &str) -> Result<(), SupervisorError> {
        if !is_project_slug(slug) {
            return Err(SupervisorError::Invalid(format!("`{slug}` is not a sandbox slug")));
        }
        let mut state = self.inner.state.lock();
        if let Some(e) = state.sandboxes.get_mut(slug) {
            e.removed = true;
            e.running = false;
        }
        state.wipes.insert(slug.to_owned());
        drop(state);
        self.inner.kick.notify_one();
        Ok(())
    }

    pub fn sandbox_status(&self, slug: &str) -> Result<SandboxStatus, SupervisorError> {
        let state = self.inner.state.lock();
        state
            .sandboxes
            .get(slug)
            .filter(|e| !e.removed)
            .map(|e| self.sandbox_status_of(slug, e, state.wipes.contains(slug)))
            .ok_or_else(|| SupervisorError::NoSandbox(slug.to_owned()))
    }

    /// Where the host reaches the running sandbox's `port`, when it has one.
    pub fn sandbox_address(&self, slug: &str) -> Result<Option<SocketAddr>, SupervisorError> {
        let state = self.inner.state.lock();
        let e = state.sandboxes.get(slug).filter(|e| !e.removed).ok_or_else(|| SupervisorError::NoSandbox(slug.to_owned()))?;
        Ok(running_vm(e, state.wipes.contains(slug)).and_then(|vm| vm.port.map(|port| self.inner.vmm.endpoint(&vm.id, vm.lease.ip, port))))
    }

    /// A fresh exec stream into the running sandbox (see [`crate::guest::exec`]).
    pub async fn sandbox_exec(&self, slug: &str) -> Result<GuestConn, SupervisorError> {
        let id = {
            let state = self.inner.state.lock();
            let e = state.sandboxes.get(slug).filter(|e| !e.removed).ok_or_else(|| SupervisorError::NoSandbox(slug.to_owned()))?;
            running_vm(e, state.wipes.contains(slug)).ok_or_else(|| SupervisorError::NotRunning(slug.to_owned()))?.id.clone()
        };
        self.inner.vmm.exec(&id).await.map_err(|e| SupervisorError::Backend(e.to_string()))
    }

    /// Path of `cloud-guest` inside sandboxes, for exec sessions that run it.
    pub fn guest_program(&self) -> &str {
        self.inner.vmm.guest_program()
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

    /// One pass over every deployment and sandbox (concurrently); passes never overlap.
    pub async fn reconcile(&self) {
        let _pass = self.inner.pass.lock().await;
        let (keys, slugs) = {
            let state = self.inner.state.lock();
            let keys: Vec<Key> = state.entries.keys().cloned().collect();
            let slugs: BTreeSet<String> = state.sandboxes.keys().chain(&state.wipes).cloned().collect();
            (keys, slugs)
        };
        futures::join!(join_all(keys.iter().map(|k| self.reconcile_one(k))), join_all(slugs.iter().map(|s| self.reconcile_sandbox(s))));
    }

    async fn reconcile_sandbox(&self, slug: &str) {
        let (vm, wanted, wiping) = {
            let state = self.inner.state.lock();
            let entry = state.sandboxes.get(slug);
            let vm = entry.and_then(|e| e.vm.as_ref().map(|vm| (vm.id.clone(), vm.lease.ip, vm.generation == e.generation, vm.proven)));
            (vm, entry.is_some_and(|e| e.running && !e.removed), state.wipes.contains(slug))
        };
        if let Some((id, ip, current, proven)) = vm {
            // Passes never overlap, so a VM seen here has finished booting.
            let alive = self.inner.vmm.running(&id).await;
            if alive && current && wanted && !wiping {
                if !proven {
                    self.update_sandbox(slug, |e| {
                        if let Some(vm) = &mut e.vm {
                            vm.proven = true;
                        }
                        e.failures = 0;
                        e.error = None;
                    });
                }
                return;
            }
            if !alive && current && wanted {
                tracing::warn!(slug, %id, "sandbox VM is not running; rebooting it");
                self.update_sandbox(slug, |e| {
                    e.failures += 1;
                    e.error = Some("the VM stopped unexpectedly".to_owned());
                });
            }
            if !self.stop_sandbox_vm(slug, &id, ip).await {
                return;
            }
        }
        if wiping {
            match self.inner.vmm.destroy_volume(&DiskId::Sandbox(slug.to_owned())).await {
                Ok(()) => {
                    self.inner.state.lock().wipes.remove(slug);
                    tracing::info!(slug, "sandbox disk destroyed");
                }
                Err(e) => {
                    tracing::warn!(slug, error = %e, "destroying the sandbox disk failed; will retry");
                    return;
                }
            }
        }
        {
            let mut state = self.inner.state.lock();
            let State { sandboxes, wipes, .. } = &mut *state;
            if sandboxes.get(slug).is_some_and(|e| e.removed && e.vm.is_none()) && !wipes.contains(slug) {
                sandboxes.remove(slug);
                return;
            }
        }
        self.start_sandbox_vm(slug).await;
    }

    fn update_sandbox(&self, slug: &str, f: impl FnOnce(&mut SandboxEntry)) {
        if let Some(e) = self.inner.state.lock().sandboxes.get_mut(slug) {
            f(e);
        }
    }

    /// Whether the VM is gone (its address and egress policy released).
    async fn stop_sandbox_vm(&self, slug: &str, id: &str, ip: Ipv4Addr) -> bool {
        if let Err(e) = self.inner.vmm.stop(id).await {
            tracing::warn!(slug, id, error = %e, "stopping sandbox VM failed; will retry");
            return false;
        }
        let mut state = self.inner.state.lock();
        let State { sandboxes, allocator, .. } = &mut *state;
        if let Some(e) = sandboxes.get_mut(slug)
            && e.vm.as_ref().is_some_and(|vm| vm.id == id)
        {
            e.vm = None;
            allocator.release(ip);
        }
        self.inner.egress.remove(IpAddr::V4(ip));
        tracing::info!(slug, id, "sandbox VM stopped");
        true
    }

    /// Boots the sandbox's VM when it is wanted, has none and its disk is not being destroyed.
    async fn start_sandbox_vm(&self, slug: &str) {
        let spec = {
            let mut state = self.inner.state.lock();
            let State { sandboxes, wipes, allocator, .. } = &mut *state;
            let Some(e) = sandboxes.get_mut(slug) else {
                return;
            };
            if !e.running || e.removed || e.vm.is_some() || wipes.contains(slug) {
                return;
            }
            let lease = match allocator.allocate() {
                Ok(lease) => lease,
                Err(err) => {
                    tracing::warn!(slug, error = %err, "no address for sandbox");
                    e.failures += 1;
                    e.error = Some(err.to_string());
                    return;
                }
            };
            let s = &e.spec;
            let spec = ReplicaSpec {
                id: new_id(),
                repo: s.image_repo.clone(),
                image_digest: s.image_digest.clone(),
                hostname: s.slug.clone(),
                labels: vec![(labels::SANDBOX.to_owned(), s.slug.clone())],
                guest: sandbox_guest_config(s, &lease, self.inner.cfg.proxy_port),
                volume: Some(VolumeSpec { disk: DiskId::Sandbox(s.slug.clone()), size_mb: s.disk_mib }),
                cpus: Some(s.cpus),
                mem_mib: Some(s.mem_mib),
                lease: lease.clone(),
            };
            self.inner.egress.set(IpAddr::V4(lease.ip), GuestPolicy { owner: format!("sandbox {slug}"), allow: s.egress.clone() });
            e.vm = Some(SandboxVm {
                id: spec.id.clone(),
                lease,
                generation: e.generation.clone(),
                port: s.port,
                booted: false,
                proven: false,
            });
            spec
        };
        let (id, ip) = (spec.id.clone(), spec.lease.ip);
        let result = self.inner.vmm.start(spec).await;
        let mut state = self.inner.state.lock();
        let State { sandboxes, allocator, .. } = &mut *state;
        let entry = sandboxes.get_mut(slug);
        match result {
            Ok(_) => {
                if let Some(vm) = entry.and_then(|e| e.vm.as_mut()).filter(|vm| vm.id == id) {
                    vm.booted = true;
                }
                tracing::info!(slug, %id, "sandbox VM booted");
            }
            Err(err) => {
                tracing::warn!(slug, %id, error = %err, "sandbox VM failed to start");
                if let Some(e) = entry {
                    if e.vm.as_ref().is_some_and(|vm| vm.id == id) {
                        e.vm = None;
                    }
                    e.failures += 1;
                    e.error = Some(err.to_string());
                }
                allocator.release(ip);
                self.inner.egress.remove(IpAddr::V4(ip));
            }
        }
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
                let State { entries, allocator, .. } = &mut *state;
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
            let State { entries, allocator, .. } = &mut *state;
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
                repo: d.project.clone(),
                image_digest: d.image_digest.clone(),
                hostname: format!("{}-{index}", p.name),
                labels: [
                    (labels::PROJECT, d.project.clone()),
                    (labels::STAGE, d.stage.as_str().to_owned()),
                    (labels::PROCESS, p.name.clone()),
                    (labels::INDEX, index.to_string()),
                    (labels::RELEASE, d.release.clone()),
                ]
                .into_iter()
                .map(|(k, v)| (k.to_owned(), v))
                .collect(),
                guest: guest_config(d, p, &lease, self.inner.cfg.proxy_port),
                volume: p.volume.as_ref().map(|v| VolumeSpec {
                    disk: DiskId::Process { project: d.project.clone(), stage: d.stage, process: p.name.clone(), index },
                    size_mb: v.size_mb,
                }),
                cpus: None,
                mem_mib: None,
                lease: lease.clone(),
            };
            let owner = format!("{}/{}", d.project, d.stage.as_str());
            self.inner.egress.set(IpAddr::V4(lease.ip), GuestPolicy { owner, allow: d.manifest.egress.clone() });
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
        let State { entries, allocator, .. } = &mut *state;
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
