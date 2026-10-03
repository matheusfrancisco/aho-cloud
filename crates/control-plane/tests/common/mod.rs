//! Test harness: the real router over MemStore, memory ledgers, a temp blob dir, fake ports and a
//! manual clock, driven with `tower::ServiceExt::oneshot`.
#![allow(dead_code)]

use std::sync::Arc;

use aho_log::{LogStore, MemoryLog};
use aho_types::Principal;
use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use cloud_api::routes::{self, fill};
use cloud_contract::ledger::{Stage, approval_message};
use cloud_control_plane::blobs::FsBlobStore;
use cloud_control_plane::clock::ManualClock;
use cloud_control_plane::crypto::{b64url, sha256};
use cloud_control_plane::fakes::{FakeBuilder, FakeResolver, FakeRuntime, FakeSessionHost, SoftAuthenticator};
use cloud_control_plane::ledgers::Ledgers;
use cloud_control_plane::store::MemStore;
use cloud_control_plane::{AppState, Config, MasterKey, Services, router};
use cloud_domains::EdgeTarget;
use ed25519_dalek::{Signer as _, SigningKey};
use http_body_util::BodyExt as _;
use serde_json::{Value, json};
use tower::ServiceExt as _;

pub const SERVICE: &str = "svc-token-0123456789abcdef0123456789abcdef";
pub const RP_ID: &str = "console.test";
pub const ORIGIN: &str = "https://console.test";
pub const START: u64 = 1_800_000_000;

pub struct H {
    pub app: Router,
    pub state: AppState,
    pub builder: Arc<FakeBuilder>,
    pub runtime: Arc<FakeRuntime>,
    pub host: Arc<FakeSessionHost>,
    pub dns: Arc<FakeResolver>,
    pub clock: Arc<ManualClock>,
    _dir: tempfile::TempDir,
}

pub struct Device {
    pub key_id: String,
    pub key: SigningKey,
    pub token: String,
}

pub struct Person {
    pub id: String,
    pub email: String,
    /// Console session (passkey).
    pub token: String,
    pub auth: SoftAuthenticator,
    pub device: Option<Device>,
}

impl Person {
    pub fn device(&self) -> &Device {
        self.device.as_ref().expect("device login first")
    }

    /// An Ed25519 proof over `message` with this person's device key.
    pub fn device_proof(&self, message: &[u8]) -> Value {
        let d = self.device();
        json!({"method": "device_key", "key_id": d.key_id, "signature": b64url(&d.key.sign(message).to_bytes())})
    }

    /// A passkey proof over `message` (challenge = SHA-256 of it), as the console makes.
    pub fn passkey_proof(&mut self, message: &[u8]) -> Value {
        json!({"method": "passkey", "credential": self.auth.assert_challenge(&sha256(message))})
    }
}

impl H {
    pub async fn new() -> H {
        H::with(|_| {}).await
    }

    /// Like [`H::new`], with `tweak` applied to the configuration first.
    pub async fn with(tweak: impl FnOnce(&mut Config)) -> H {
        let dir = tempfile::tempdir().unwrap();
        let mut cfg = Config::new(MasterKey::new([42; 32]), SERVICE, RP_ID, ORIGIN);
        cfg.public_url = "http://control.test".into();
        cfg.edge_target = EdgeTarget {
            cname: "edge.aho.test".into(),
            ipv4: vec!["203.0.113.10".parse().unwrap()],
            ipv6: vec![],
            nameservers: vec!["ns1.aho.test".into()],
        };
        cfg.preview_domain = "preview.aho.test".into();
        cfg.domain_grace_secs = 3600;
        tweak(&mut cfg);
        let builder = Arc::new(FakeBuilder::new());
        let runtime = Arc::new(FakeRuntime::new());
        let host = Arc::new(FakeSessionHost::new());
        let dns = Arc::new(FakeResolver::new());
        let clock = Arc::new(ManualClock::new(START));
        let services = Services { builder: builder.clone(), runtime: runtime.clone(), session_host: host.clone(), resolver: dns.clone() };
        let state = AppState::new(
            cfg,
            Arc::new(MemStore::new()),
            Arc::new(FsBlobStore::new(dir.path().join("blobs"))),
            Ledgers::memory(),
            services,
            clock.clone(),
        )
        .await
        .unwrap();
        H { app: router(state.clone()), state, builder, runtime, host, dns, clock, _dir: dir }
    }

    pub async fn send(
        &self,
        method: Method,
        path: &str,
        token: Option<&str>,
        content_type: Option<&str>,
        body: Vec<u8>,
    ) -> (StatusCode, Value) {
        let mut b = Request::builder().method(method).uri(path);
        if let Some(t) = token {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        if let Some(ct) = content_type {
            b = b.header("content-type", ct);
        }
        let resp = self.app.clone().oneshot(b.body(Body::from(body)).unwrap()).await.unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()))
        };
        (status, value)
    }

    pub async fn req(&self, method: Method, path: &str, token: Option<&str>, body: Option<Value>) -> (StatusCode, Value) {
        match body {
            Some(v) => self.send(method, path, token, Some("application/json"), serde_json::to_vec(&v).unwrap()).await,
            None => self.send(method, path, token, None, Vec::new()).await,
        }
    }

    pub async fn get(&self, path: &str, token: &str) -> (StatusCode, Value) {
        self.req(Method::GET, path, Some(token), None).await
    }

    pub async fn post(&self, path: &str, token: Option<&str>, body: Value) -> (StatusCode, Value) {
        self.req(Method::POST, path, token, Some(body)).await
    }

    /// POST that must succeed; returns the body.
    pub async fn ok(&self, path: &str, token: &str, body: Value) -> Value {
        let (s, v) = self.post(path, Some(token), body).await;
        assert!(s.is_success(), "POST {path} → {s}: {v}");
        v
    }

    pub async fn get_ok(&self, path: &str, token: &str) -> Value {
        let (s, v) = self.get(path, token).await;
        assert!(s.is_success(), "GET {path} → {s}: {v}");
        v
    }

    /// Passkey sign-up.
    pub async fn signup(&self, email: &str, name: &str) -> Person {
        let mut auth = SoftAuthenticator::new(RP_ID, ORIGIN);
        let (s, ch) = self.post(routes::PASSKEY_REGISTER_START, None, json!({"email": email, "name": name})).await;
        assert_eq!(s, StatusCode::OK, "{ch}");
        let credential = auth.register(&ch["options"]);
        let (s, session) =
            self.post(routes::PASSKEY_REGISTER_FINISH, None, json!({"ceremony_id": ch["ceremony_id"], "credential": credential})).await;
        assert_eq!(s, StatusCode::OK, "{session}");
        Person {
            id: session["user"]["id"].as_str().unwrap().to_owned(),
            email: email.to_owned(),
            token: session["token"].as_str().unwrap().to_owned(),
            auth,
            device: None,
        }
    }

    /// `ahocloud login`: device start, console approval, poll.
    pub async fn device_login(&self, person: &mut Person, seed: u8) {
        let key = SigningKey::from_bytes(&[seed; 32]);
        let started = self
            .ok(
                routes::DEVICE_START,
                &person.token,
                json!({"device_name": format!("{}@laptop", person.email), "public_key": b64url(key.verifying_key().as_bytes())}),
            )
            .await;
        let (s, _) = self.post(routes::DEVICE_APPROVE, Some(&person.token), json!({"user_code": started["user_code"]})).await;
        assert_eq!(s, StatusCode::NO_CONTENT);
        let (s, polled) = self.post(routes::DEVICE_POLL, None, json!({"device_code": started["device_code"]})).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(polled["status"], "approved", "{polled}");
        person.device =
            Some(Device { key_id: polled["key_id"].as_str().unwrap().into(), key, token: polled["token"].as_str().unwrap().into() });
    }

    /// Ana (owner) and Bob (member) in org `acme` with project `project`, both device-logged-in.
    pub async fn team(&self, project: &str) -> (Person, Person) {
        let mut ana = self.signup("ana@acme.test", "Ana").await;
        let mut bob = self.signup("bob@acme.test", "Bob").await;
        self.ok(routes::ORGS, &ana.token, json!({"slug": "acme", "name": "Acme"})).await;
        self.ok(&fill(routes::ORG_MEMBERS, &[("org", "acme")]), &ana.token, json!({"email": bob.email, "owner": false})).await;
        self.ok(&fill(routes::ORG_PROJECTS, &[("org", "acme")]), &ana.token, json!({"slug": project})).await;
        self.device_login(&mut ana, 1).await;
        self.device_login(&mut bob, 2).await;
        (ana, bob)
    }

    pub async fn set_secret(&self, project: &str, token: &str, name: &str, stage: Stage, value: &str) -> Value {
        let (s, v) = self
            .req(
                Method::PUT,
                &fill(routes::SECRETS, &[("p", project)]),
                Some(token),
                Some(json!({"name": name, "stage": stage, "value": value})),
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        v
    }

    /// Proposes `manifest` and waits for the build pipeline.
    pub async fn propose(&self, project: &str, token: &str, manifest: Value, provenance: Value) -> Value {
        let (s, r) = self
            .post(&fill(routes::RELEASES, &[("p", project)]), Some(token), json!({"manifest": manifest, "provenance": provenance}))
            .await;
        assert_eq!(s, StatusCode::CREATED, "{r}");
        self.state.idle().await;
        self.release(project, token, r["id"].as_str().unwrap()).await
    }

    pub async fn release(&self, project: &str, token: &str, id: &str) -> Value {
        self.get_ok(&fill(routes::RELEASE, &[("p", project), ("r", id)]), token).await
    }

    /// Approves `release` at `stage` with `person`'s device key.
    pub async fn approve(&self, project: &str, person: &Person, release: &Value, stage: Stage) -> (StatusCode, Value) {
        let id = release["id"].as_str().unwrap();
        let message = approval_message(project, id, release["manifest_hash"].as_str().unwrap(), stage);
        let body = json!({"stage": stage, "proof": person.device_proof(&message)});
        self.post(&fill(routes::APPROVE, &[("p", project), ("r", id)]), Some(&person.device().token), body).await
    }

    pub async fn promote(&self, project: &str, token: &str, id: &str, stage: Stage) -> (StatusCode, Value) {
        self.post(&fill(routes::PROMOTE, &[("p", project), ("r", id)]), Some(token), json!({"stage": stage})).await
    }

    /// Approves (by everyone in `approvers`) and promotes through staging and live.
    pub async fn take_live(&self, project: &str, approvers: &[&Person], release: &Value) {
        let id = release["id"].as_str().unwrap();
        for stage in [Stage::Staging, Stage::Live] {
            for p in approvers {
                let (s, v) = self.approve(project, p, release, stage).await;
                assert_eq!(s, StatusCode::OK, "approve {stage:?}: {v}");
            }
            let (s, v) = self.promote(project, &approvers[0].token, id, stage).await;
            assert_eq!(s, StatusCode::OK, "promote {stage:?}: {v}");
        }
    }
}

/// A valid manifest for `project`: one public web process, one secret reaching Stripe.
pub fn manifest(project: &str) -> Value {
    json!({
        "api_version": "deploy.aho.cloud/v1",
        "project": project,
        "source": {"kind": "oci", "digest": format!("sha256:{}", "a".repeat(64))},
        "build": {"kind": "prebuilt"},
        "processes": [{"name": "web", "port": 8080, "expose": "public", "health": {"kind": "http", "path": "/healthz"}}],
        "auth": {"mode": "none"},
        "secrets": [{"name": "STRIPE_KEY", "reach": ["api.stripe.com"]}],
        "egress": ["api.stripe.com"],
        "data_class": "none",
        "owner": {"name": "Ana", "email": "ana@acme.test"},
        "budget": {"ceiling_usd_month": 20, "degrade": "sleep"}
    })
}

pub fn unattested() -> Value {
    json!({"kind": "unattested"})
}

/// A genuine aho session log (JSONL) whose entries carry `texts`.
pub fn session_log(texts: &[&str]) -> String {
    let log = MemoryLog::new();
    for t in texts {
        log.append(
            None,
            Principal::system("test"),
            aho_log::EntryKind::Custom { ns: "Note".into(), name: "said".into(), body: json!({"text": t}) },
        )
        .unwrap();
    }
    log.read_from(None).iter().map(|e| serde_json::to_string(e).unwrap()).collect::<Vec<_>>().join("\n") + "\n"
}

pub async fn upload_session(h: &H, project: &str, token: &str, session_id: &str, log: &str) -> (StatusCode, Value) {
    let path = format!("{}?session_id={session_id}", fill(routes::SESSIONS, &[("p", project)]));
    h.send(Method::POST, &path, Some(token), Some("application/x-ndjson"), log.as_bytes().to_vec()).await
}
