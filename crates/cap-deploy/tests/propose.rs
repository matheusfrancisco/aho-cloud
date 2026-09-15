//! cloud_propose end to end: a composition with a test world whose egress is real HTTP, started
//! with a session log, talking to an in-process fake control plane on 127.0.0.1.
#![allow(clippy::unwrap_used, clippy::expect_used)] // integration-test helpers may panic on setup failure

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use aho_cap::*;
use aho_config::{Loadout, ResolveInputs, Schema, resolve};
use aho_egress::{Egress, EgressError, EgressHandle, HttpRequest, HttpResponse};
use aho_log::{CapState, CustomFold, CustomFolds, EntryKind, LogStore, MemoryLog, fold};
use aho_types::*;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use cap_deploy::{DeployDraft, DeployPropose, DeployState, NS, ProposeConfig, fold_deploy};
use cloud_api::{ApiError, ProposeRelease, Release, ReleaseStatus, SessionUploaded};
use cloud_contract::ledger::Provenance;
use cloud_contract::validate::{Refusal, RefusalCode};
use parking_lot::Mutex;
use serde_json::json;

const TOKEN: &str = "ahc_test_token_7Qx";

// ───────────────────────────── the test world's egress ─────────────────────────────

/// Real HTTP through reqwest, honouring the request's host list, limits and redirect cap.
struct HttpEgress(reqwest::Client);

impl Egress for HttpEgress {
    fn fetch(&self, req: HttpRequest) -> BoxFuture<'_, Result<HttpResponse, EgressError>> {
        Box::pin(async move {
            req.hosts.check_url(&req.url).map_err(EgressError::Denied)?;
            let method = match req.method {
                aho_egress::Method::Get => reqwest::Method::GET,
                aho_egress::Method::Post => reqwest::Method::POST,
                aho_egress::Method::Head => reqwest::Method::HEAD,
            };
            let mut b = self.0.request(method, &req.url).timeout(Duration::from_millis(req.limits.timeout_ms));
            for h in &req.headers {
                b = b.header(&h.name, &h.value);
            }
            if let Some(body) = req.body {
                b = b.body(body);
            }
            let resp = b.send().await.map_err(|e| EgressError::Connect { url: req.url.clone(), why: e.to_string() })?;
            let status = resp.status().as_u16();
            if aho_egress::is_redirect(status) && req.limits.max_redirects == 0 {
                return Err(EgressError::TooManyRedirects { limit: 0, url: req.url.clone() });
            }
            let headers = resp.headers().iter().map(|(k, v)| (k.as_str().to_owned(), v.to_str().unwrap_or("").to_owned())).collect();
            let final_url = resp.url().to_string();
            let bytes = resp.bytes().await.map_err(|e| EgressError::Connect { url: req.url.clone(), why: e.to_string() })?;
            let cap = req.limits.max_body_bytes.min(bytes.len());
            Ok(HttpResponse { status, final_url, headers, body: bytes[..cap].to_vec(), truncated: cap < bytes.len(), redirects: 0 })
        })
    }

    fn enforced_by(&self) -> &str {
        "policy only"
    }
}

/// Provides the egress seam the way a world does.
struct TestWorld;

impl Capability for TestWorld {
    const ID: CapabilityId = Self::ID_;
    const MANIFEST: &'static Manifest = &Self::MANIFEST_;
    type Deps = ();
    type Config = ();

    fn build(_: (), _: ()) -> Result<Self, BuildError> {
        Ok(TestWorld)
    }

    fn register(&self, r: &mut Registrar<'_>) {
        let client = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
        r.provide(EgressHandle(Arc::new(HttpEgress(client))));
    }
}

declare_capability!(
    TestWorld,
    "test-world",
    kind = Kind::Provider,
    provides = [aho_egress::SEAM],
    requires = [],
    tools = &[],
    provides_handles = [HandleKind::Egress(Route::Sandbox)]
);

// ───────────────────────────── the fake control plane ─────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Accept,
    Refuse,
    WrongHash,
}

#[derive(Clone, Debug)]
struct Hit {
    path: String,
    query: HashMap<String, String>,
    auth: String,
    content_type: String,
    body: Vec<u8>,
}

#[derive(Clone)]
struct Api {
    mode: Mode,
    hits: Arc<Mutex<Vec<Hit>>>,
}

fn record(api: &Api, path: String, query: HashMap<String, String>, headers: &HeaderMap, body: &[u8]) -> bool {
    let h = |n: &str| headers.get(n).and_then(|v| v.to_str().ok()).unwrap_or("").to_owned();
    let auth = h("authorization");
    api.hits.lock().push(Hit { path, query, auth: auth.clone(), content_type: h("content-type"), body: body.to_vec() });
    auth == format!("Bearer {TOKEN}")
}

fn api_error(status: StatusCode, code: &str, refusals: Vec<Refusal>) -> Response {
    (status, axum::Json(ApiError { code: code.into(), message: format!("{code} by the fake"), refusals })).into_response()
}

async fn sessions(
    State(api): State<Api>,
    Path(p): Path<String>,
    Query(q): Query<HashMap<String, String>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !record(&api, format!("/v1/projects/{p}/sessions"), q.clone(), &headers, &body) {
        return api_error(StatusCode::UNAUTHORIZED, "unauthorized", vec![]);
    }
    let Ok(entries) = cloud_draft::verify_jsonl(std::str::from_utf8(&body).unwrap_or("")) else {
        return api_error(StatusCode::BAD_REQUEST, "invalid", vec![]);
    };
    let s = cloud_draft::SessionSummary::of(&q["session_id"], &entries).unwrap();
    axum::Json(SessionUploaded { session_id: s.session_id, head_hash: s.head_hash, entries: s.entries }).into_response()
}

async fn releases(State(api): State<Api>, Path(p): Path<String>, headers: HeaderMap, body: Bytes) -> Response {
    if !record(&api, format!("/v1/projects/{p}/releases"), HashMap::new(), &headers, &body) {
        return api_error(StatusCode::UNAUTHORIZED, "unauthorized", vec![]);
    }
    let req: ProposeRelease = serde_json::from_slice(&body).unwrap();
    if api.mode == Mode::Refuse {
        let r = Refusal {
            code: RefusalCode::DataClassUnsupported,
            field: "/data_class".into(),
            message: "not here".into(),
            fix: "call us".into(),
        };
        return api_error(StatusCode::UNPROCESSABLE_ENTITY, "refused", vec![r]);
    }
    let manifest_hash = if api.mode == Mode::WrongHash { format!("blake3:{}", "0".repeat(64)) } else { req.manifest.hash() };
    let release = Release {
        id: "r-0001".into(),
        project: p,
        manifest_hash,
        manifest: req.manifest,
        provenance: req.provenance,
        image_digest: None,
        stage: None,
        status: ReleaseStatus::Building,
        diff: Default::default(),
        requirement: cloud_contract::policy::Requirement { approvals: 1, owner_required: true },
        approvals: vec![],
        taint: vec![],
        undeclared_egress: vec![],
    };
    (StatusCode::CREATED, axum::Json(release)).into_response()
}

async fn fake_api(mode: Mode) -> (String, Arc<Mutex<Vec<Hit>>>) {
    let hits = Arc::new(Mutex::new(Vec::new()));
    let app = Router::new()
        .route("/v1/projects/{p}/sessions", post(sessions))
        .route("/v1/projects/{p}/releases", post(releases))
        .with_state(Api { mode, hits: hits.clone() });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (url, hits)
}

// ───────────────────────────── the session ─────────────────────────────

fn comp() -> Composition {
    Composition::new("t").provide::<TestWorld>().consume::<DeployDraft>().consume::<DeployPropose>()
}

fn loadout(sets: &[(&str, &str)]) -> Loadout {
    let mut schema = Schema::default();
    for k in comp().config_keys() {
        schema.add(k);
    }
    let mut i = ResolveInputs::default();
    i.sets = sets.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
    resolve(&i, &schema, false).unwrap()
}

struct Session {
    log: MemoryLog,
    reg: Registry,
}

const SESSION: SessionId = SessionId(0x5e55);

fn session(api_url: &str, token: &str, extra: &[(&str, &str)]) -> Session {
    let mut sets = vec![("cloud.api_url", api_url), ("credentials.ahocloud", token)];
    sets.extend_from_slice(extra);
    let l = loadout(&sets);
    let policy = PolicyInput { preset: "dev".into(), allowed: ConsequenceSet::of(&[Consequence::ReadOnly, Consequence::NetworkEgress]) };
    let p = plan(&comp().manifests(), &l, &Env::empty(Os::current()), &policy, vec![]).unwrap();
    for id in ["test-world", "cloud-deploy-draft", "cloud-deploy-propose"] {
        assert!(matches!(p.row(id).unwrap().state, CapState::Active), "{id}: {:?}", p.row(id).unwrap().state);
    }
    let log = MemoryLog::new();
    log.append(None, Principal::Operator, EntryKind::UserMessage { form: Form::Prompt, text: "ship it".into(), client_key: None }).unwrap();
    let mut seed = Registry::default();
    let shared: Arc<dyn LogStore> = Arc::new(log.clone());
    seed.services.insert("aho.log".into(), Arc::new(shared));
    seed.services.insert("aho.session_id".into(), Arc::new(SESSION));
    let (reg, broken) = comp().start_seeded(&p, &l, seed);
    assert!(broken.is_empty(), "{broken:?}");
    Session { log, reg }
}

fn agent() -> Principal {
    Principal::Agent { id: AgentId(1), turn: TurnId(1) }
}

impl Session {
    /// The session's side of a call: the `ToolCall` entry, then the tool body.
    async fn propose(&self, args: Value) -> ExecOutcome {
        self.log
            .append(
                None,
                agent(),
                EntryKind::ToolCall {
                    call: CallId("p1".into()),
                    tool: "cloud_propose".into(),
                    args: args.clone(),
                    class: Consequence::NetworkEgress,
                },
            )
            .unwrap();
        let (_, exec) = self.reg.tool("cloud_propose").unwrap();
        let view = ExecView {
            call: CallId("p1".into()),
            tool: "cloud_propose".into(),
            class: Consequence::NetworkEgress,
            args,
            policy: PolicyView {
                epoch: Epoch(1),
                allowed: ConsequenceSet::ALL,
                world: BoundWorld::host(),
                workspace: String::new(),
                home: String::new(),
            },
            by: agent(),
        };
        exec(ExecRequest { view, cancel: CancelToken::new(), progress: None }).await
    }
}

fn manifest() -> Value {
    json!({
        "api_version": "deploy.aho.cloud/v1",
        "project": "shop",
        "source": {"kind": "archive", "hash": format!("blake3:{}", "a".repeat(64))},
        "build": {"kind": "dockerfile"},
        "processes": [{"name": "web", "port": 8080, "expose": "public", "health": {"kind": "http", "path": "/healthz"}}],
        "auth": {"mode": "none"},
        "data_class": "none",
        "owner": {"name": "Ana", "email": "ana@shop.io"},
        "budget": {"ceiling_usd_month": 20, "degrade": "sleep"}
    })
}

fn err_of(o: &ExecOutcome) -> (String, String) {
    match o {
        ExecOutcome::Err { code, reason } => (code.clone(), reason.clone()),
        other => panic!("expected Err, got {other:?}"),
    }
}

// ───────────────────────────── tests ─────────────────────────────

#[tokio::test]
async fn a_valid_manifest_uploads_the_session_then_proposes_with_the_bearer_and_records_the_event() {
    let (url, hits) = fake_api(Mode::Accept).await;
    let s = session(&url, TOKEN, &[]);
    let (value, content) = match s.propose(json!({"manifest": manifest()})).await {
        ExecOutcome::Done { value, content, .. } => (value, content),
        other => panic!("{other:?}"),
    };

    let hits = hits.lock().clone();
    assert_eq!(hits.iter().map(|h| h.path.as_str()).collect::<Vec<_>>(), ["/v1/projects/shop/sessions", "/v1/projects/shop/releases"]);
    assert!(hits.iter().all(|h| h.auth == format!("Bearer {TOKEN}")), "every call carries the credential as a bearer");
    assert_eq!(hits[0].query.get("session_id").map(String::as_str), Some("5e55"));
    assert_eq!(hits[0].content_type, "application/x-ndjson");
    let uploaded = cloud_draft::verify_jsonl(std::str::from_utf8(&hits[0].body).unwrap()).unwrap();
    assert!(
        matches!(&uploaded.last().unwrap().kind, EntryKind::ToolCall { tool, .. } if tool == "cloud_propose"),
        "the log up to this call is uploaded"
    );

    let sent: ProposeRelease = serde_json::from_slice(&hits[1].body).unwrap();
    assert_eq!(sent.provenance, Provenance::Attested { session_id: "5e55".into(), head_hash: uploaded.last().unwrap().hash.to_hex() });
    let hash = sent.manifest.hash();
    assert_eq!(value["release"], "r-0001");
    assert_eq!(value["manifest_hash"], hash.as_str());
    let text = content[0].as_text().unwrap();
    assert!(text.contains("a human must approve"), "{text}");
    assert!(!text.contains(TOKEN) && !value.to_string().contains(TOKEN), "the token never reaches the model or the log");

    // the session appends `value.events`; the fold accepts it once, and never the same release twice
    let e = &value["events"][0];
    assert_eq!((e["ns"].as_str(), e["name"].as_str()), (Some(NS), Some("Proposed")));
    let append = || {
        s.log
            .append(
                None,
                Principal::system("cloud_propose"),
                EntryKind::Custom { ns: NS.into(), name: "Proposed".into(), body: e["body"].clone() },
            )
            .unwrap()
    };
    append();
    let mut folds = CustomFolds::new();
    folds.insert(NS.into(), fold_deploy as CustomFold);
    let st = fold(&s.log.read_branch(s.log.head().unwrap()).unwrap(), &folds).unwrap();
    let deploy: DeployState = serde_json::from_value(st.custom[NS].clone()).unwrap();
    assert_eq!(
        deploy.proposed.iter().map(|p| (p.release.as_str(), p.manifest_hash.as_str())).collect::<Vec<_>>(),
        [("r-0001", hash.as_str())]
    );
    append();
    assert!(fold(&s.log.read_branch(s.log.head().unwrap()).unwrap(), &folds).is_err());
}

#[tokio::test]
async fn local_refusals_short_circuit_before_any_request() {
    let (url, hits) = fake_api(Mode::Accept).await;
    let s = session(&url, TOKEN, &[]);

    let mut bad = manifest();
    bad["budget"]["ceiling_usd_month"] = json!(0);
    bad["processes"][0]["health"]["path"] = json!("healthz");
    let (code, reason) = err_of(&s.propose(json!({"manifest": bad})).await);
    assert_eq!(code, "REFUSED");
    assert!(
        reason.contains("/budget/ceiling_usd_month") && reason.contains("/processes/0/health/path") && reason.contains("nothing was sent"),
        "{reason}"
    );

    let edge = "default = \"edge\"\n";
    let (code, reason) = err_of(&s.propose(json!({"manifest": manifest(), "edge_toml": edge})).await);
    assert_eq!(code, "REFUSED", "an edge.toml that needs a login the manifest does not configure is refused too");
    assert!(reason.contains("edge.toml"), "{reason}");

    let (code, _) = err_of(&s.propose(json!({"manifest": {"api_version": "deploy.aho.cloud/v1"}})).await);
    assert_eq!(code, "BAD_MANIFEST");

    let (code, _) = err_of(&session(&url, TOKEN, &[("cloud.project", "other")]).propose(json!({"manifest": manifest()})).await);
    assert_eq!(code, "WRONG_PROJECT");

    assert!(hits.lock().is_empty(), "no refusal contacted the cloud");
}

#[tokio::test]
async fn server_answers_come_back_as_typed_errors() {
    let (url, _) = fake_api(Mode::Refuse).await;
    let (code, reason) = err_of(&session(&url, TOKEN, &[]).propose(json!({"manifest": manifest()})).await);
    assert_eq!(code, "refused");
    assert!(reason.contains("/data_class") && reason.contains("call us"), "{reason}");

    let (url, hits) = fake_api(Mode::Accept).await;
    let (code, reason) = err_of(&session(&url, "wrong-token", &[]).propose(json!({"manifest": manifest()})).await);
    assert_eq!(code, "unauthorized");
    assert!(reason.contains("aho credential set ahocloud"), "{reason}");
    assert_eq!(hits.lock().len(), 1, "a refused upload stops before the proposal");

    let (url, _) = fake_api(Mode::WrongHash).await;
    let (code, reason) = err_of(&session(&url, TOKEN, &[]).propose(json!({"manifest": manifest()})).await);
    assert_eq!(code, "HASH_MISMATCH");
    assert!(reason.contains("Do not approve"), "{reason}");
}

#[test]
fn the_api_url_must_be_https_unless_loopback_and_the_credential_must_be_set() {
    for (url, ok) in [
        ("https://api.aho.cloud", true),
        ("http://127.0.0.1:9", true),
        ("http://localhost:9/", true),
        ("http://api.aho.cloud", false),
        ("ftp://x", false),
    ] {
        let got = ProposeConfig::from_loadout(&loadout(&[("cloud.api_url", url), ("credentials.ahocloud", TOKEN)]));
        assert_eq!(got.is_ok(), ok, "{url}: {got:?}");
    }
    let cfg = ProposeConfig::from_loadout(&loadout(&[("credentials.ahocloud", TOKEN)])).unwrap();
    assert_eq!(cfg.api_url, cap_deploy::API_URL_DEFAULT);
    assert!(!format!("{cfg:?}").contains(TOKEN), "Debug never prints the token");
    assert!(matches!(ProposeConfig::from_loadout(&loadout(&[])), Err(ConfigError::MissingKey(k)) if k == "credentials.ahocloud"));
}
