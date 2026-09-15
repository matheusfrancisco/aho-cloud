//! `cloud_propose`: local validation, session upload, release proposal. The control plane decides
//! everything after that; nothing here can approve or activate.

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

use aho_cap::{
    BoundWorld, BuildError, CancelToken, Capability, CapabilityId, Config, ConfigError, Deps, ExecOutcome, ExecRequest, Fix, HandleKind,
    Kind, Manifest, MissingDep, Present, Registrar, Requirement, Resolver, Route, ToolDecl, ToolExec, declare_capability,
};
use aho_config::{ConfigKey, Loadout, Ty};
use aho_egress::{EgressError, EgressHandle, HostList, HttpRequest, HttpResponse, Limits, Method};
use aho_log::{LogStore, OpKind};
use aho_types::{BoxFuture, Card, Consequence, ContentBlock, RootSchema, SessionId, Value};
use cloud_api::{ApiError, CheckResult, ProposeRelease, Release, SessionUploaded, routes};
use cloud_contract::ledger::Provenance;
use serde::de::DeserializeOwned;
use serde_json::json;

use crate::{NS, PROPOSED, Proposed, event, fold_deploy, session_hex};

/// Credential holding the aho cloud API token (`aho credential set ahocloud`).
pub const CREDENTIAL: &str = "ahocloud";
const CREDENTIAL_KEY: &str = "credentials.ahocloud";
/// Control-plane base URL unless `cloud.api_url` says otherwise.
pub const API_URL_DEFAULT: &str = "https://api.aho.cloud";

// ───────────────────────────── config and deps ─────────────────────────────

/// The API token; never printed.
#[derive(Clone)]
struct Token(String);

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(<secret>)")
    }
}

/// `cloud.*` settings plus the `ahocloud` credential and the operator's egress list.
#[derive(Clone, Debug)]
pub struct ProposeConfig {
    /// Base URL without a trailing `/`.
    pub api_url: String,
    /// When set, only manifests for this project are proposed.
    pub project: Option<String>,
    pub hosts: HostList,
    token: Token,
}

impl Config for ProposeConfig {
    const PREFIX: &'static str = "cloud";

    fn keys() -> Vec<ConfigKey> {
        let mut keys = aho_egress::config_keys();
        keys.extend([
            ConfigKey::new(
                "cloud.api_url",
                Ty::String,
                Some(json!(API_URL_DEFAULT)),
                "aho cloud control-plane base URL; https, or http to a loopback address only.",
            ),
            ConfigKey::new("cloud.project", Ty::String, None, "aho cloud project slug this session works on."),
            ConfigKey::new(CREDENTIAL_KEY, Ty::String, None, "aho cloud API token; `aho credential set ahocloud` stores it.").secret(),
        ]);
        keys
    }

    fn from_loadout(l: &Loadout) -> Result<Self, ConfigError> {
        let api_url = aho_cap::cfg_str(l, "cloud.api_url", API_URL_DEFAULT).trim().trim_end_matches('/').to_owned();
        let url =
            aho_egress::parse_url(&api_url).map_err(|_| ConfigError::Type { key: "cloud.api_url".into(), expected: "an http(s) URL" })?;
        let loopback = matches!(url.host.as_str(), "127.0.0.1" | "localhost" | "::1");
        if url.scheme != "https" && !loopback {
            return Err(ConfigError::Type {
                key: "cloud.api_url".into(),
                expected: "an https URL (plain http only to a loopback address)",
            });
        }
        let project = l.str("cloud.project").map(str::trim).filter(|p| !p.is_empty()).map(str::to_owned);
        let token = l
            .secrets
            .get(CREDENTIAL_KEY)
            .map(|s| s.expose().trim().to_owned())
            .filter(|t| !t.is_empty())
            .ok_or_else(|| ConfigError::MissingKey(CREDENTIAL_KEY.into()))?;
        Ok(ProposeConfig { api_url, project, hosts: HostList::from_loadout(l)?, token: Token(token) })
    }
}

/// The bound world's network, and the session log uploaded as provenance.
pub struct ProposeDeps {
    pub egress: EgressHandle,
    pub log: Option<Arc<dyn LogStore>>,
    pub session: Option<SessionId>,
}

impl Deps for ProposeDeps {
    const REQUIRES: &'static [HandleKind] = &[HandleKind::Egress(Route::Sandbox)];
    fn resolve(r: &Resolver) -> Result<Self, MissingDep> {
        Ok(ProposeDeps { egress: r.handle()?, log: r.service("aho.log"), session: r.service("aho.session_id") })
    }
}

// ───────────────────────────── the tool ─────────────────────────────

fn schema(_: &BoundWorld) -> RootSchema {
    RootSchema(json!({
        "type": "object",
        "properties": {
            "manifest": {
                "type": "object",
                "description": "The complete deploy manifest (api_version `deploy.aho.cloud/v1`), built from cloud_draft and the human's answers."
            },
            "edge_toml": {"type": "string", "description": "Text of the project's edge.toml, if it ships one."}
        },
        "required": ["manifest"],
        "additionalProperties": false
    }))
}

fn call_card(args: &Value) -> Card {
    let project = args.pointer("/manifest/project").and_then(Value::as_str).unwrap_or("?");
    Card::Generic { title: "Propose release".into(), body: format!("proposing a release of `{project}` to aho cloud") }
}

fn result_card(_: &Value, v: &Value) -> Card {
    let release = v.get("release").and_then(Value::as_str).unwrap_or("?");
    Card::Generic { title: "Release proposed".into(), body: format!("release {release} awaits human approval") }
}

pub static PROPOSE_TOOLS: &[ToolDecl] = &[ToolDecl::new(
    "cloud_propose",
    "Propose a deploy manifest to aho cloud as a new release. The manifest is validated locally first: refusals come back with \
     the field and the fix, and nothing is sent. Otherwise this session's log is uploaded as provenance and the proposal is filed. \
     This tool can only propose: the cloud builds the release, and nothing goes live until a human approves it with a signed \
     approval in the console or with `ahocloud`. No tool can approve.",
    Consequence::NetworkEgress,
)
.schema(schema)
.op(OpKind::Remote, false, true)
.present(Present { call: call_card, result: result_card })];

/// Limits for control-plane calls: small answers, no redirects (the bearer never follows one).
const LIMITS: Limits = Limits { max_body_bytes: 1024 * 1024, timeout_ms: 60_000, max_redirects: 0 };

/// `cloud_propose`.
#[derive(Clone)]
pub struct DeployPropose {
    egress: EgressHandle,
    log: Option<Arc<dyn LogStore>>,
    session: Option<SessionId>,
    cfg: Arc<ProposeConfig>,
}

impl DeployPropose {
    async fn propose(&self, args: &Value, cancel: &CancelToken) -> ExecOutcome {
        let Some(raw) = args.get("manifest") else {
            return ExecOutcome::err("BAD_ARGS", "cloud_propose needs `manifest`, the whole deploy manifest; nothing was sent.");
        };
        let manifest: cloud_contract::Manifest = match serde_json::from_value(raw.clone()) {
            Ok(m) => m,
            Err(e) => {
                return ExecOutcome::err(
                    "BAD_MANIFEST",
                    format!("the manifest does not have the deploy.aho.cloud/v1 shape ({e}); nothing was sent."),
                );
            }
        };
        let edge_toml = args.get("edge_toml").and_then(Value::as_str).map(str::to_owned);
        if let Some(p) = &self.cfg.project
            && *p != manifest.project
        {
            return ExecOutcome::err(
                "WRONG_PROJECT",
                format!("this session deploys `{p}` (cloud.project) but the manifest names `{}`; nothing was sent.", manifest.project),
            );
        }
        let check = CheckResult::of(&manifest, edge_toml.as_deref());
        if !check.accepted() {
            return ExecOutcome::err("REFUSED", render_refusals(&check));
        }

        let project = manifest.project.clone();
        let provenance = match self.upload_session(&project, cancel).await {
            Ok(p) => p,
            Err(out) => return out,
        };
        let manifest_hash = manifest.hash();
        let body = ProposeRelease { manifest, edge_toml, provenance };
        let url = format!("{}{}", self.cfg.api_url, routes::fill(routes::RELEASES, &[("p", project.as_str())]));
        let payload = match serde_json::to_vec(&body) {
            Ok(b) => b,
            Err(e) => return ExecOutcome::err("BAD_MANIFEST", format!("the proposal does not serialize ({e}); nothing was sent.")),
        };
        let release: Release = match self.call(&url, "application/json", payload, cancel).await {
            Ok(r) => r,
            Err(out) => return out,
        };
        if release.manifest_hash != manifest_hash {
            return ExecOutcome::err(
                "HASH_MISMATCH",
                format!(
                    "the cloud recorded release {} with manifest hash {}, but the manifest sent hashes to {manifest_hash}. Do not approve it; \
                     tell the human.",
                    release.id, release.manifest_hash
                ),
            );
        }
        let proposed = Proposed { project: project.clone(), release: release.id.clone(), manifest_hash: manifest_hash.clone() };
        let text = format!(
            "Proposed release {} of `{project}` ({manifest_hash}); status {:?}. Nothing is live: the cloud builds it, and a human must \
             approve it in the aho cloud console or with `ahocloud`.",
            release.id, release.status
        );
        let value = json!({
            "release": release.id,
            "project": project,
            "manifest_hash": manifest_hash,
            "status": release.status,
            "provenance": release.provenance,
            "events": [event(PROPOSED, &proposed)],
        });
        ExecOutcome::Done { value, content: vec![ContentBlock::text(text)], meta: None }
    }

    /// Uploads the session log and returns the provenance the server verified; `Unattested`
    /// when this host gave the tool no session log.
    async fn upload_session(&self, project: &str, cancel: &CancelToken) -> Result<Provenance, ExecOutcome> {
        let (Some(log), Some(id)) = (&self.log, self.session) else {
            return Ok(Provenance::Unattested { commit: None, ci_run: None });
        };
        let mut body = Vec::new();
        for e in log.read_from(None) {
            let line =
                serde_json::to_vec(&e).map_err(|e| ExecOutcome::err("LOG_BROKEN", format!("a log entry does not serialize ({e})")))?;
            body.extend_from_slice(&line);
            body.push(b'\n');
        }
        let url = format!("{}{}?session_id={}", self.cfg.api_url, routes::fill(routes::SESSIONS, &[("p", project)]), session_hex(id));
        let up: SessionUploaded = self.call(&url, "application/x-ndjson", body, cancel).await?;
        Ok(Provenance::Attested { session_id: up.session_id, head_hash: up.head_hash })
    }

    /// One authenticated POST; a 2xx body decodes as `T`, anything else as [`ApiError`].
    async fn call<T: DeserializeOwned>(
        &self,
        url: &str,
        content_type: &str,
        body: Vec<u8>,
        cancel: &CancelToken,
    ) -> Result<T, ExecOutcome> {
        if let Err(why) = self.cfg.hosts.check_url(url) {
            return Err(ExecOutcome::err("EGRESS_DENIED", why));
        }
        let req = HttpRequest {
            method: Method::Post,
            url: url.to_owned(),
            headers: Vec::new(),
            body: Some(body),
            limits: LIMITS,
            hosts: self.cfg.hosts.clone(),
        }
        .header("content-type", content_type)
        .header("accept", "application/json")
        .secret_header("authorization", format!("Bearer {}", self.cfg.token.0));
        let resp = match cancellable(cancel, self.egress.0.fetch(req)).await {
            None => return Err(ExecOutcome::Cancelled),
            Some(Err(e)) => return Err(egress_outcome(&e)),
            Some(Ok(r)) => r,
        };
        if resp.is_success() {
            return serde_json::from_slice(&resp.body).map_err(|e| {
                ExecOutcome::err("BAD_RESPONSE", format!("{url} answered {} with a body this tool does not understand ({e})", resp.status))
            });
        }
        Err(api_outcome(&resp))
    }
}

/// Races `f` against the call's cancellation token.
async fn cancellable<T>(cancel: &CancelToken, mut f: BoxFuture<'_, T>) -> Option<T> {
    loop {
        tokio::select! {
            out = &mut f => return Some(out),
            _ = tokio::time::sleep(Duration::from_millis(50)) => {
                if cancel.is_cancelled() {
                    return None;
                }
            }
        }
    }
}

fn egress_outcome(e: &EgressError) -> ExecOutcome {
    let code = match e {
        EgressError::Denied(_) => "EGRESS_DENIED",
        EgressError::InvalidUrl { .. } => "BAD_URL",
        EgressError::TooManyRedirects { .. } => "REDIRECTED",
        EgressError::Timeout { .. } => "TIMEOUT",
        EgressError::Connect { .. } => "UNREACHABLE",
        EgressError::Unsupported(_) => "UNSUPPORTED",
    };
    ExecOutcome::err(code, e.to_string())
}

fn api_outcome(resp: &HttpResponse) -> ExecOutcome {
    let Ok(api) = serde_json::from_slice::<ApiError>(&resp.body) else {
        let excerpt: String = String::from_utf8_lossy(&resp.body).chars().take(300).collect();
        return ExecOutcome::err(&format!("HTTP_{}", resp.status), format!("aho cloud answered HTTP {}: {}", resp.status, excerpt.trim()));
    };
    let mut reason = api.message.clone();
    if resp.status == 401 {
        reason.push_str(&format!(" The `{CREDENTIAL}` credential was refused; the human runs `aho credential set {CREDENTIAL}`."));
    }
    for r in &api.refusals {
        reason.push_str(&format!("\n- {}: {} (fix: {})", r.field, r.message, r.fix));
    }
    ExecOutcome::err(&api.code, reason)
}

fn render_refusals(check: &CheckResult) -> String {
    let mut out = format!(
        "the manifest was refused locally ({} refusals, {} edge.toml errors); nothing was sent to aho cloud.",
        check.refusals.len(),
        check.edge_errors.len()
    );
    for r in &check.refusals {
        out.push_str(&format!("\n- {}: {} (fix: {})", r.field, r.message, r.fix));
    }
    for e in &check.edge_errors {
        out.push_str(&format!("\n- edge.toml: {e}"));
    }
    out
}

impl Capability for DeployPropose {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = ProposeDeps;
    type Config = ProposeConfig;

    fn build(d: ProposeDeps, cfg: ProposeConfig) -> Result<Self, BuildError> {
        Ok(DeployPropose { egress: d.egress, log: d.log, session: d.session, cfg: Arc::new(cfg) })
    }

    fn register(&self, r: &mut Registrar<'_>) {
        r.entry_kind(NS, fold_deploy);
        let me = self.clone();
        let exec: ToolExec = Arc::new(move |req: ExecRequest| {
            let me = me.clone();
            Box::pin(async move { me.propose(&req.view.args, &req.cancel).await })
        });
        r.tool(&PROPOSE_TOOLS[0], exec);
    }
}

declare_capability!(
    DeployPropose,
    "cloud-deploy-propose",
    kind = Kind::Tool,
    provides = [],
    requires = [Requirement::Seam(aho_egress::SEAM), Requirement::Credential(CREDENTIAL)],
    tools = PROPOSE_TOOLS,
    unlock = Fix::cli("aho credential set ahocloud"),
    entry_kinds = ["Deploy/Proposed"],
    doc = "cloud_propose: files a deploy manifest with aho cloud as a release proposal after local validation; approvals stay human-only."
);
