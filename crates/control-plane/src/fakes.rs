//! In-process implementations of every port, and a software WebAuthn authenticator. Used by the
//! tests and for running the control plane without its sibling services.

use parking_lot::Mutex;
use std::collections::{BTreeMap, VecDeque};
use std::net::IpAddr;

use async_trait::async_trait;
use cloud_api::internal::{
    BuildJob, BuildResult, Deployment, DeploymentStatus, Maintenance, ProcessStatus, RunState, Scale, StartWorkspace, WorkspaceStarted,
};
use cloud_contract::ledger::Stage;
use cloud_domains::{DnsError, Resolver, normalize_name};
use futures::future::BoxFuture;
use p256::ecdsa::signature::Signer as _;
use p256::ecdsa::{Signature, SigningKey};
use serde_json::{Value, json};

use crate::crypto::{b64url, b64url_decode, random_bytes, sha256};
use crate::ports::{BuildService, PortError, RuntimeService, SessionHostService};

/// A builder that answers from a queue of scripted results, defaulting to a successful build
/// whose digest is derived from the release id.
#[derive(Default)]
pub struct FakeBuilder {
    scripted: Mutex<VecDeque<BuildResult>>,
    jobs: Mutex<Vec<BuildJob>>,
}

impl FakeBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    /// The next build answers `result`.
    pub fn push(&self, result: BuildResult) {
        self.scripted.lock().push_back(result);
    }

    /// Every job received, in order.
    pub fn jobs(&self) -> Vec<BuildJob> {
        self.jobs.lock().clone()
    }

    /// The digest the default answer uses for `release`.
    pub fn digest_for(release: &str) -> String {
        format!("sha256:{}", crate::crypto::hex(&sha256(release.as_bytes())))
    }
}

#[async_trait]
impl BuildService for FakeBuilder {
    async fn build(&self, job: BuildJob) -> Result<BuildResult, PortError> {
        let release = job.release.clone();
        self.jobs.lock().push(job);
        Ok(self
            .scripted
            .lock()
            .pop_front()
            .unwrap_or_else(|| BuildResult::Built { image_digest: Self::digest_for(&release), log: "built by FakeBuilder".into() }))
    }
}

/// What a [`FakeRuntime`] was asked to do, besides deployments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuntimeCall {
    Restart { project: String, stage: Stage },
    Scale { project: String, stage: Stage, scale: Scale },
    Maintenance { project: String, stage: Stage, on: bool },
    Sleep { project: String, stage: Stage },
}

/// A runtime that records deployments and reports every process healthy at `10.0.0.1:<port>`.
#[derive(Default)]
pub struct FakeRuntime {
    deployments: Mutex<Vec<Deployment>>,
    calls: Mutex<Vec<RuntimeCall>>,
    status: Mutex<BTreeMap<(String, Stage), DeploymentStatus>>,
}

impl FakeRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every deployment received, in order (including the secrets it carried).
    pub fn deployments(&self) -> Vec<Deployment> {
        self.deployments.lock().clone()
    }

    pub fn calls(&self) -> Vec<RuntimeCall> {
        self.calls.lock().clone()
    }

    /// The release running at `(project, stage)`.
    pub fn running(&self, project: &str, stage: Stage) -> Option<String> {
        self.status.lock().get(&(project.to_owned(), stage)).map(|s| s.release.clone())
    }

    fn set_state(&self, project: &str, stage: Stage, state: RunState) -> Result<(), PortError> {
        let mut g = self.status.lock();
        let s = g.get_mut(&(project.to_owned(), stage)).ok_or_else(|| PortError::Status {
            service: "runtime",
            status: 404,
            message: format!("no deployment {project}/{}", stage.as_str()),
        })?;
        s.state = state;
        Ok(())
    }
}

#[async_trait]
impl RuntimeService for FakeRuntime {
    async fn deploy(&self, d: Deployment) -> Result<DeploymentStatus, PortError> {
        let processes = d
            .manifest
            .processes
            .iter()
            .map(|p| ProcessStatus {
                name: p.name.clone(),
                replicas: 1,
                healthy: 1,
                addresses: p.port.map(|port| vec![format!("10.0.0.1:{port}")]).unwrap_or_default(),
            })
            .collect();
        let status = DeploymentStatus {
            project: d.project.clone(),
            stage: d.stage,
            release: d.release.clone(),
            state: RunState::Running,
            processes,
        };
        self.status.lock().insert((d.project.clone(), d.stage), status.clone());
        self.deployments.lock().push(d);
        Ok(status)
    }

    async fn status(&self, project: &str, stage: Stage) -> Result<Option<DeploymentStatus>, PortError> {
        Ok(self.status.lock().get(&(project.to_owned(), stage)).cloned())
    }

    async fn restart(&self, project: &str, stage: Stage) -> Result<(), PortError> {
        self.set_state(project, stage, RunState::Running)?;
        self.calls.lock().push(RuntimeCall::Restart { project: project.into(), stage });
        Ok(())
    }

    async fn scale(&self, project: &str, stage: Stage, scale: Scale) -> Result<(), PortError> {
        {
            let mut g = self.status.lock();
            if let Some(p) = g.get_mut(&(project.to_owned(), stage)).and_then(|s| s.processes.iter_mut().find(|p| p.name == scale.process))
            {
                p.replicas = scale.replicas;
                p.healthy = scale.replicas;
            }
        }
        self.calls.lock().push(RuntimeCall::Scale { project: project.into(), stage, scale });
        Ok(())
    }

    async fn maintenance(&self, project: &str, stage: Stage, m: Maintenance) -> Result<(), PortError> {
        self.set_state(project, stage, if m.on { RunState::Maintenance } else { RunState::Running })?;
        self.calls.lock().push(RuntimeCall::Maintenance { project: project.into(), stage, on: m.on });
        Ok(())
    }

    async fn sleep(&self, project: &str, stage: Stage) -> Result<(), PortError> {
        self.set_state(project, stage, RunState::Sleeping)?;
        self.calls.lock().push(RuntimeCall::Sleep { project: project.into(), stage });
        Ok(())
    }
}

/// A session host that hands out `wss://sessions.test/rpc/<project>` workspaces.
#[derive(Default)]
pub struct FakeSessionHost {
    requests: Mutex<Vec<StartWorkspace>>,
}

impl FakeSessionHost {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn requests(&self) -> Vec<StartWorkspace> {
        self.requests.lock().clone()
    }
}

#[async_trait]
impl SessionHostService for FakeSessionHost {
    async fn start(&self, request: StartWorkspace) -> Result<WorkspaceStarted, PortError> {
        // Like the real host: the session id is known only if the caller resumes one.
        let started = WorkspaceStarted {
            session_id: request.session_id.clone(),
            rpc_url: format!("wss://sessions.test/rpc/{}", request.project),
            token: b64url(&random_bytes(16)),
            preview_url: format!("https://{}.preview.test", request.project),
        };
        self.requests.lock().push(request);
        Ok(started)
    }
}

/// DNS answers from a table.
#[derive(Default)]
pub struct FakeResolver {
    txt: Mutex<BTreeMap<String, Vec<String>>>,
    cname: Mutex<BTreeMap<String, String>>,
    addrs: Mutex<BTreeMap<String, Vec<IpAddr>>>,
    ns: Mutex<BTreeMap<String, Vec<String>>>,
}

impl FakeResolver {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_txt(&self, name: &str, values: &[&str]) {
        self.txt.lock().insert(normalize_name(name), values.iter().map(|s| (*s).to_owned()).collect());
    }

    pub fn clear_txt(&self, name: &str) {
        self.txt.lock().remove(&normalize_name(name));
    }

    pub fn set_cname(&self, name: &str, target: &str) {
        self.cname.lock().insert(normalize_name(name), normalize_name(target));
    }

    pub fn set_addrs(&self, name: &str, addrs: &[IpAddr]) {
        self.addrs.lock().insert(normalize_name(name), addrs.to_vec());
    }

    pub fn set_ns(&self, name: &str, servers: &[&str]) {
        self.ns.lock().insert(normalize_name(name), servers.iter().map(|s| normalize_name(s)).collect());
    }
}

impl Resolver for FakeResolver {
    fn txt(&self, name: &str) -> BoxFuture<'_, Result<Vec<String>, DnsError>> {
        let v = self.txt.lock().get(&normalize_name(name)).cloned().unwrap_or_default();
        Box::pin(async move { Ok(v) })
    }

    fn cname(&self, name: &str) -> BoxFuture<'_, Result<Option<String>, DnsError>> {
        let v = self.cname.lock().get(&normalize_name(name)).cloned();
        Box::pin(async move { Ok(v) })
    }

    fn addrs(&self, name: &str) -> BoxFuture<'_, Result<Vec<IpAddr>, DnsError>> {
        let v = self.addrs.lock().get(&normalize_name(name)).cloned().unwrap_or_default();
        Box::pin(async move { Ok(v) })
    }

    fn ns(&self, name: &str) -> BoxFuture<'_, Result<Vec<String>, DnsError>> {
        let v = self.ns.lock().get(&normalize_name(name)).cloned().unwrap_or_default();
        Box::pin(async move { Ok(v) })
    }
}

/// A software WebAuthn authenticator (ES256, `none` attestation) for driving the passkey flows
/// without a browser.
pub struct SoftAuthenticator {
    pub rp_id: String,
    /// The origin written into clientDataJSON.
    pub origin: String,
    /// Whether to set the UV flag.
    pub user_verified: bool,
    credentials: Vec<SoftCredential>,
}

struct SoftCredential {
    id: Vec<u8>,
    key: SigningKey,
    user_handle: Vec<u8>,
    counter: u32,
}

impl SoftAuthenticator {
    pub fn new(rp_id: &str, origin: &str) -> Self {
        Self { rp_id: rp_id.into(), origin: origin.into(), user_verified: true, credentials: Vec::new() }
    }

    pub fn credential_ids(&self) -> Vec<Vec<u8>> {
        self.credentials.iter().map(|c| c.id.clone()).collect()
    }

    fn client_data(&self, ty: &str, challenge: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({"type": ty, "challenge": challenge, "origin": self.origin, "crossOrigin": false})).unwrap_or_default()
    }

    fn flags(&self, attested: bool) -> u8 {
        0x01 | if self.user_verified { 0x04 } else { 0 } | if attested { 0x40 } else { 0 }
    }

    /// Answers `navigator.credentials.create(options)`.
    pub fn register(&mut self, options: &Value) -> Value {
        let pk = &options["publicKey"];
        let challenge = pk["challenge"].as_str().unwrap_or_default().to_owned();
        let user_handle = pk["user"]["id"].as_str().and_then(b64url_decode).unwrap_or_default();
        let key = SigningKey::random(&mut rand::rngs::OsRng);
        let id = random_bytes(16);
        let point = key.verifying_key().to_encoded_point(false);
        let (x, y) = (point.x().map(|x| x.to_vec()).unwrap_or_default(), point.y().map(|y| y.to_vec()).unwrap_or_default());
        use ciborium::value::Value as C;
        let cose = C::Map(vec![
            (C::Integer(1.into()), C::Integer(2.into())),
            (C::Integer(3.into()), C::Integer((-7).into())),
            (C::Integer((-1).into()), C::Integer(1.into())),
            (C::Integer((-2).into()), C::Bytes(x)),
            (C::Integer((-3).into()), C::Bytes(y)),
        ]);
        let mut cose_bytes = Vec::new();
        let _ = ciborium::into_writer(&cose, &mut cose_bytes);
        let mut auth_data = sha256(self.rp_id.as_bytes()).to_vec();
        auth_data.push(self.flags(true));
        auth_data.extend_from_slice(&0u32.to_be_bytes());
        auth_data.extend_from_slice(&[0u8; 16]);
        auth_data.extend_from_slice(&u16::try_from(id.len()).unwrap_or(16).to_be_bytes());
        auth_data.extend_from_slice(&id);
        auth_data.extend_from_slice(&cose_bytes);
        let att = C::Map(vec![
            (C::Text("fmt".into()), C::Text("none".into())),
            (C::Text("attStmt".into()), C::Map(Vec::new())),
            (C::Text("authData".into()), C::Bytes(auth_data)),
        ]);
        let mut att_bytes = Vec::new();
        let _ = ciborium::into_writer(&att, &mut att_bytes);
        let cdj = self.client_data("webauthn.create", &challenge);
        self.credentials.push(SoftCredential { id: id.clone(), key, user_handle, counter: 0 });
        json!({
            "id": b64url(&id),
            "rawId": b64url(&id),
            "type": "public-key",
            "response": {"clientDataJSON": b64url(&cdj), "attestationObject": b64url(&att_bytes)},
        })
    }

    /// Answers `navigator.credentials.get(options)` with the first allowed credential (or the
    /// first one registered).
    pub fn assert(&mut self, options: &Value) -> Value {
        let pk = &options["publicKey"];
        let challenge = pk["challenge"].as_str().unwrap_or_default().to_owned();
        let allowed: Vec<Vec<u8>> = pk["allowCredentials"]
            .as_array()
            .map(|a| a.iter().filter_map(|c| c["id"].as_str().and_then(b64url_decode)).collect())
            .unwrap_or_default();
        let idx = self.credentials.iter().position(|c| allowed.is_empty() || allowed.contains(&c.id)).unwrap_or(0);
        self.assert_with(idx, &challenge)
    }

    /// An assertion over `challenge` bytes, as the console produces for approvals
    /// (`challenge = SHA-256(approval_message)`).
    pub fn assert_challenge(&mut self, challenge: &[u8]) -> Value {
        self.assert_with(0, &b64url(challenge))
    }

    fn assert_with(&mut self, idx: usize, challenge: &str) -> Value {
        let cdj = self.client_data("webauthn.get", challenge);
        let flags = self.flags(false);
        let rp_hash = sha256(self.rp_id.as_bytes());
        let Some(cred) = self.credentials.get_mut(idx) else {
            return json!({});
        };
        cred.counter += 1;
        let mut auth_data = rp_hash.to_vec();
        auth_data.push(flags);
        auth_data.extend_from_slice(&cred.counter.to_be_bytes());
        let mut signed = auth_data.clone();
        signed.extend_from_slice(&sha256(&cdj));
        let sig: Signature = cred.key.sign(&signed);
        json!({
            "id": b64url(&cred.id),
            "rawId": b64url(&cred.id),
            "type": "public-key",
            "response": {
                "clientDataJSON": b64url(&cdj),
                "authenticatorData": b64url(&auth_data),
                "signature": b64url(sig.to_der().as_bytes()),
                "userHandle": b64url(&cred.user_handle),
            },
        })
    }
}
