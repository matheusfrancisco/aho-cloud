//! Shared fixtures: a sample deployment and sandbox, and an in-process `Vmm` whose replicas are
//! local HTTP servers (HTTP health), in-memory guest control servers (command health) and real
//! exec servers running local processes.

#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::http::StatusCode;
use cloud_api::internal::{Deployment, SANDBOX_IMAGE_REPO, SandboxSpec};
use cloud_contract::ledger::Stage;
use cloud_runtime::guest::control::{self, Control, ProcessState};
use cloud_runtime::guest::exec::{self, ExecContext};
use cloud_runtime::guest::process::{ReapMode, Reaper};
use cloud_runtime::vmm::{DiskId, GuestConn, ReplicaSpec, Started, Vmm, VmmError, labels};
use futures::future::BoxFuture;
use parking_lot::Mutex;

pub const DIGEST_1: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";
pub const DIGEST_2: &str = "sha256:2222222222222222222222222222222222222222222222222222222222222222";

/// `web` (HTTP health on 8080) and `worker` (command health, with a volume).
pub fn manifest_json() -> serde_json::Value {
    serde_json::json!({
        "api_version": "deploy.aho.cloud/v1",
        "project": "acme-shop",
        "source": { "kind": "git", "repo": "https://github.com/acme/shop", "commit": "0123456789abcdef0123456789abcdef01234567" },
        "build": { "kind": "dockerfile" },
        "processes": [
            { "name": "web", "port": 8080, "expose": "public", "health": { "kind": "http", "path": "/healthz" } },
            { "name": "worker", "command": ["python", "-m", "worker"],
              "health": { "kind": "command", "argv": ["python", "-m", "worker", "--check"] },
              "volume": { "mount": "/data", "size_mb": 1024 } }
        ],
        "auth": { "mode": "none" },
        "secrets": [ { "name": "STRIPE_KEY", "reach": ["api.stripe.com"] } ],
        "egress": ["api.stripe.com", "*.sentry.io"],
        "data_class": "none",
        "owner": { "name": "Ana", "email": "ana@acme.com" },
        "budget": { "ceiling_usd_month": 50, "degrade": "sleep" }
    })
}

pub fn deployment(release: &str, digest: &str) -> Deployment {
    Deployment {
        project: "acme-shop".into(),
        stage: Stage::Live,
        release: release.into(),
        image_digest: digest.into(),
        manifest: serde_json::from_value(manifest_json()).unwrap(),
        secrets: BTreeMap::from([("STRIPE_KEY".into(), "sk_live_1".into())]),
    }
}

pub fn sandbox(slug: &str, digest: &str) -> SandboxSpec {
    SandboxSpec {
        slug: slug.into(),
        image_repo: SANDBOX_IMAGE_REPO.into(),
        image_digest: digest.into(),
        env: BTreeMap::from([("ANTHROPIC_API_KEY".into(), "sk-ant-1".into())]),
        egress: vec!["api.anthropic.com".into(), "*.github.com".into()],
        port: Some(3000),
        cpus: 2,
        mem_mib: 2048,
        disk_mib: 10240,
    }
}

#[derive(Default)]
pub struct Fake {
    /// `start <process> <index> <id>` / `start sandbox <slug> <id>` / `stop <id>` /
    /// `exec <id>` / `destroy <disk>`, in call order.
    pub log: Vec<String>,
    pub specs: HashMap<String, ReplicaSpec>,
    pub alive: HashSet<String>,
    servers: HashMap<String, (SocketAddr, tokio::task::JoinHandle<()>)>,
    /// Processes whose health checks fail.
    pub unhealthy: HashSet<String>,
    pub fail_start: bool,
}

/// The in-process backend. Exec sessions run on this machine, as this user.
#[derive(Clone, Default)]
pub struct TestVmm {
    pub fake: Arc<Mutex<Fake>>,
    /// What [`Vmm::guest_program`] answers.
    pub guest_program: String,
}

/// The disk as the fake logs it.
pub fn disk_name(disk: &DiskId) -> String {
    match disk {
        DiskId::Process { project, stage, process, index } => format!("{project}/{}/{process}-{index}", stage.as_str()),
        DiskId::Sandbox(slug) => format!("sandbox/{slug}"),
    }
}

impl TestVmm {
    pub fn alive_ids(&self, process: &str) -> Vec<String> {
        let f = self.fake.lock();
        let mut ids: Vec<(u16, String)> = f
            .alive
            .iter()
            .filter(|id| f.specs[*id].label(labels::PROCESS) == Some(process))
            .map(|id| (index(&f.specs[id]), id.clone()))
            .collect();
        ids.sort();
        ids.into_iter().map(|(_, id)| id).collect()
    }

    /// The running VMs of sandbox `slug`.
    pub fn sandbox_ids(&self, slug: &str) -> Vec<String> {
        let f = self.fake.lock();
        f.alive.iter().filter(|id| f.specs[*id].label(labels::SANDBOX) == Some(slug)).cloned().collect()
    }

    pub fn spec(&self, id: &str) -> ReplicaSpec {
        self.fake.lock().specs[id].clone()
    }

    /// Where the fake serves replica `id`'s port.
    pub fn endpoint_of(&self, id: &str) -> SocketAddr {
        self.fake.lock().servers[id].0
    }

    /// Simulates the VM dying.
    pub fn crash(&self, id: &str) {
        let mut f = self.fake.lock();
        f.alive.remove(id);
        if let Some((_, task)) = f.servers.remove(id) {
            task.abort();
        }
    }

    pub fn set_healthy(&self, process: &str, healthy: bool) {
        let mut f = self.fake.lock();
        if healthy {
            f.unhealthy.remove(process);
        } else {
            f.unhealthy.insert(process.to_owned());
        }
    }

    pub fn log(&self) -> Vec<String> {
        self.fake.lock().log.clone()
    }
}

struct FakeGuest {
    healthy: bool,
}

impl Control for FakeGuest {
    fn health(&self, _argv: Vec<String>, _timeout: Duration) -> BoxFuture<'_, Result<i32, String>> {
        let code = if self.healthy { 0 } else { 1 };
        Box::pin(async move { Ok(code) })
    }
    fn restart(&self) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(async { Ok(()) })
    }
    fn status(&self) -> ProcessState {
        ProcessState::default()
    }
}

/// The process slot index of a deployment replica.
pub fn index(spec: &ReplicaSpec) -> u16 {
    spec.label(labels::INDEX).unwrap().parse().unwrap()
}

impl Vmm for TestVmm {
    fn start(&self, spec: ReplicaSpec) -> BoxFuture<'_, Result<Started, VmmError>> {
        Box::pin(async move {
            if self.fake.lock().fail_start {
                return Err(VmmError::Control("image pull failed".into()));
            }
            let mut server = None;
            if spec.guest.port.is_some() {
                let fake = self.fake.clone();
                let process = spec.label(labels::PROCESS).unwrap_or_default().to_owned();
                let app = Router::new().fallback(move || {
                    let sick = fake.lock().unhealthy.contains(&process);
                    async move { if sick { StatusCode::SERVICE_UNAVAILABLE } else { StatusCode::OK } }
                });
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let task = tokio::spawn(async move {
                    let _ = axum::serve(listener, app).await;
                });
                server = Some((addr, task));
            }
            let mut f = self.fake.lock();
            match spec.label(labels::SANDBOX) {
                Some(slug) => f.log.push(format!("start sandbox {slug} {}", spec.id)),
                None => f.log.push(format!("start {} {} {}", spec.label(labels::PROCESS).unwrap(), index(&spec), spec.id)),
            }
            f.alive.insert(spec.id.clone());
            if let Some(server) = server {
                f.servers.insert(spec.id.clone(), server);
            }
            f.specs.insert(spec.id.clone(), spec);
            Ok(Started { pid: None })
        })
    }

    fn stop<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<(), VmmError>> {
        Box::pin(async move {
            let mut f = self.fake.lock();
            f.log.push(format!("stop {id}"));
            f.alive.remove(id);
            if let Some((_, task)) = f.servers.remove(id) {
                task.abort();
            }
            Ok(())
        })
    }

    fn running<'a>(&'a self, id: &'a str) -> BoxFuture<'a, bool> {
        Box::pin(async move { self.fake.lock().alive.contains(id) })
    }

    fn connect<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<GuestConn, VmmError>> {
        Box::pin(async move {
            let healthy = {
                let f = self.fake.lock();
                if !f.alive.contains(id) {
                    return Err(VmmError::NotFound(id.into()));
                }
                !f.unhealthy.contains(f.specs[id].label(labels::PROCESS).unwrap_or_default())
            };
            let (host, guest) = tokio::io::duplex(4096);
            tokio::spawn(async move {
                let g = FakeGuest { healthy };
                let _ = control::serve(guest, &g).await;
            });
            Ok(Box::new(host) as GuestConn)
        })
    }

    fn exec<'a>(&'a self, id: &'a str) -> BoxFuture<'a, Result<GuestConn, VmmError>> {
        Box::pin(async move {
            let mut f = self.fake.lock();
            if !f.alive.contains(id) {
                return Err(VmmError::NotFound(id.into()));
            }
            f.log.push(format!("exec {id}"));
            drop(f);
            let (host, guest) = tokio::io::duplex(64 * 1024);
            let reaper = Reaper::start(ReapMode::Own).unwrap();
            tokio::spawn(async move {
                let _ = exec::serve(guest, &reaper, &ExecContext::inherit(Duration::from_millis(300))).await;
            });
            Ok(Box::new(host) as GuestConn)
        })
    }

    fn destroy_volume<'a>(&'a self, disk: &'a DiskId) -> BoxFuture<'a, Result<(), VmmError>> {
        Box::pin(async move {
            self.fake.lock().log.push(format!("destroy {}", disk_name(disk)));
            Ok(())
        })
    }

    fn endpoint(&self, id: &str, _ip: Ipv4Addr, _port: u16) -> SocketAddr {
        self.fake.lock().servers.get(id).map(|(addr, _)| *addr).unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], 1)))
    }

    fn guest_program(&self) -> &str {
        &self.guest_program
    }
}
