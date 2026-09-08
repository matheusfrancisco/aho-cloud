//! Shared fixtures: a sample deployment and an in-process `Vmm` whose replicas are local HTTP
//! servers (HTTP health) and in-memory guest control servers (command health).

#![allow(dead_code)]

use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::http::StatusCode;
use cloud_api::internal::Deployment;
use cloud_contract::ledger::Stage;
use cloud_runtime::guest::control::{self, Control, ProcessState};
use cloud_runtime::vmm::{GuestConn, ReplicaSpec, Started, Vmm, VmmError};
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

#[derive(Default)]
pub struct Fake {
    /// `start <process> <index> <id>` / `stop <id>`, in call order.
    pub log: Vec<String>,
    pub specs: HashMap<String, ReplicaSpec>,
    pub alive: HashSet<String>,
    servers: HashMap<String, (SocketAddr, tokio::task::JoinHandle<()>)>,
    /// Processes whose health checks fail.
    pub unhealthy: HashSet<String>,
    pub fail_start: bool,
}

/// The in-process backend.
#[derive(Clone, Default)]
pub struct TestVmm {
    pub fake: Arc<Mutex<Fake>>,
}

impl TestVmm {
    pub fn alive_ids(&self, process: &str) -> Vec<String> {
        let f = self.fake.lock();
        let mut ids: Vec<(u16, String)> =
            f.alive.iter().filter(|id| f.specs[*id].process == process).map(|id| (f.specs[id].index, id.clone())).collect();
        ids.sort();
        ids.into_iter().map(|(_, id)| id).collect()
    }

    pub fn spec(&self, id: &str) -> ReplicaSpec {
        self.fake.lock().specs[id].clone()
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

impl Vmm for TestVmm {
    fn start(&self, spec: ReplicaSpec) -> BoxFuture<'_, Result<Started, VmmError>> {
        Box::pin(async move {
            if self.fake.lock().fail_start {
                return Err(VmmError::Control("image pull failed".into()));
            }
            let mut server = None;
            if spec.guest.port.is_some() {
                let fake = self.fake.clone();
                let process = spec.process.clone();
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
            f.log.push(format!("start {} {} {}", spec.process, spec.index, spec.id));
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
                !f.unhealthy.contains(&f.specs[id].process)
            };
            let (host, guest) = tokio::io::duplex(4096);
            tokio::spawn(async move {
                let g = FakeGuest { healthy };
                let _ = control::serve(guest, &g).await;
            });
            Ok(Box::new(host) as GuestConn)
        })
    }

    fn endpoint(&self, id: &str, _ip: Ipv4Addr, _port: u16) -> SocketAddr {
        self.fake.lock().servers.get(id).map(|(addr, _)| *addr).unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], 1)))
    }
}
