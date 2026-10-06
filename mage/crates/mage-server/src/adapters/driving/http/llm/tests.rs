//! Gateway router with in-memory stores and the real upstream adapters against local stubs
//! (server.md "Testing" → HTTP integration, Gateway row).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures::StreamExt;
use mage_core::domain::{
    ArtifactId, Budget, OrgId, Run, RunError, RunErrorCode, RunId, RunKind, RunStatus, RunSubject, TicketId, Usage, UserId,
};
use mage_core::ports::Clock;
use mage_core::testing::{FakeClock, MemJobQueue, SeqIdGen};
use mage_core::wire::api::ModelRouteKind;
use parking_lot::Mutex;
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;
use url::Url;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::{GatewayState, router};
use crate::adapters::driven::llm_upstream::{AdcTokenSource, AnthropicApi, RoutedUpstream, UpstreamTimeouts, VertexUpstream};
use crate::app::auth::tokens::TokenService;
use crate::app::gateway::{BYOK_KEY_SECRET, Forward, GatewayConfig, GatewayDeps, ModelPrice, RunMeter, VertexRouteConfig};
use crate::app::runs::UsageVerdict;
use crate::error::AppError;
use crate::ports::audit::{Actor, AuditAction};
use crate::ports::auth::TokenScope;
use crate::ports::feed::FeedKind;
use crate::ports::llm::ModelSettings;
use crate::ports::org::{OrgStore, RetentionPolicy};
use crate::ports::runs::{NewRun, RunStore, RunUpdate, RunWrite};
use crate::ports::tokens::{Jti, Revocation, TokenStore};
use crate::ports::vault::{Activation, SecretBytes, SecretName, SecretScope, SecretVault};
use crate::testing::llm::{MESSAGE_JSON as MESSAGE, SSE_TEXT_TURN as SSE};
use crate::testing::runs::{sample_run, sample_spec};
use crate::testing::{FakeTokenSigner, MemAuditLog, MemChangeFeed, MemOrgStore, MemRunStore, MemSecretVault, MemTokenStore};

const SONNET: &str = "claude-sonnet-4-5";
const HAIKU: &str = "claude-haiku-4-5";
const VERTEX_SONNET: &str = "claude-sonnet-4-5@20250929";
const TENANT_KEY: &str = "sk-ant-api03-tenant-key";

/// Usage of [`SSE`] at the sonnet price: 120×3 + 30×3.75 + 2000×0.30 + 57×15 = 1927.5 → 1928 µ$.
fn sse_usage() -> Usage {
    Usage { input_tokens: 2150, output_tokens: 57, cost_micros: 1928, turns: 1, wall: Duration::ZERO }
}

fn sonnet_price() -> ModelPrice {
    ModelPrice { input: 3_000_000, output: 15_000_000, cache_write: 3_750_000, cache_read: 300_000 }
}

/// `RunMeter` with `RecordUsage`'s store effect: adds the delta to `usage_metered` and answers
/// `Deny` once the budget is exceeded.
struct StoreMeter {
    runs: Arc<MemRunStore>,
    recorded: Mutex<Vec<(OrgId, RunId, Usage)>>,
    failed: Mutex<Vec<(OrgId, RunId, RunError)>>,
}

#[async_trait]
impl RunMeter for StoreMeter {
    async fn record_usage(&self, org: OrgId, run: RunId, delta: Usage) -> Result<UsageVerdict, AppError> {
        let mut record = self.runs.get(org, run).await?;
        record.usage_metered = record.usage_metered.add(&delta);
        let over = record.usage_metered.exceeds(&record.run.budget);
        self.runs.commit(org, RunWrite { updates: vec![RunUpdate::of(record)], ..RunWrite::default() }).await?;
        self.recorded.lock().push((org, run, delta));
        Ok(match over {
            Some(limit) => UsageVerdict::Deny(format!("budget {limit}")),
            None => UsageVerdict::Allow,
        })
    }

    async fn fail_run(&self, org: OrgId, run: RunId, error: RunError) -> Result<(), AppError> {
        self.failed.lock().push((org, run, error));
        Ok(())
    }
}

impl StoreMeter {
    /// Waits for the detached settle task to report `n` deltas in total.
    async fn wait_recorded(&self, n: usize) -> Vec<(OrgId, RunId, Usage)> {
        for _ in 0..500 {
            if self.recorded.lock().len() >= n {
                return self.recorded.lock().clone();
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("expected {n} metered calls, got {:?}", self.recorded.lock());
    }
}

struct Harness {
    app: Router,
    shutdown: CancellationToken,
    tokens: Arc<TokenService>,
    token_store: Arc<MemTokenStore>,
    runs: Arc<MemRunStore>,
    orgs: Arc<MemOrgStore>,
    meter: Arc<StoreMeter>,
    audit: Arc<MemAuditLog>,
    feed: Arc<MemChangeFeed>,
    anthropic: MockServer,
    vertex: MockServer,
    _metadata: MockServer,
    org: OrgId,
    run: RunId,
}

struct Options {
    budget_micros: u64,
    per_run_in_flight: u32,
    with_key: bool,
    timeouts: UpstreamTimeouts,
    anthropic_url: Option<String>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            budget_micros: 1_000_000,
            per_run_in_flight: 4,
            with_key: true,
            timeouts: UpstreamTimeouts::default(),
            anthropic_url: None,
        }
    }
}

impl Harness {
    async fn new() -> Harness {
        Harness::with(Options::default()).await
    }

    async fn with(options: Options) -> Harness {
        let clock = Arc::new(FakeClock::new());
        let clock_dyn: Arc<dyn Clock> = clock.clone();
        let ids = Arc::new(SeqIdGen::new());
        let jobs = Arc::new(MemJobQueue::new(clock_dyn.clone(), ids.clone()));
        let feed = Arc::new(MemChangeFeed::new(clock_dyn.clone()));
        let orgs = Arc::new(MemOrgStore::new(clock_dyn.clone(), jobs.clone(), feed.clone()));
        let vault = Arc::new(MemSecretVault::new(clock_dyn.clone()));
        let token_store = Arc::new(MemTokenStore::new(clock_dyn.clone()));
        let audit = Arc::new(MemAuditLog::new(clock_dyn.clone()));
        let runs = Arc::new(MemRunStore::new(clock_dyn.clone(), jobs));
        let signer = Arc::new(FakeTokenSigner::new("https://mage.test"));
        let tokens = Arc::new(TokenService::new(signer, token_store.clone(), clock_dyn.clone(), ids));
        let meter = Arc::new(StoreMeter { runs: runs.clone(), recorded: Mutex::new(Vec::new()), failed: Mutex::new(Vec::new()) });

        let org = OrgId::from_uuid(uuid::Uuid::from_u128(0x0a));
        let now = clock.now();
        let owner = UserId::from_uuid(uuid::Uuid::from_u128(0x0b));
        orgs.create_org(
            &mage_core::domain::Org { id: org, slug: "acme".into(), name: "Acme".into(), created_at: now },
            owner,
            &RetentionPolicy::default(),
        )
        .await
        .unwrap();
        orgs.set_model_settings(
            org,
            &ModelSettings {
                route: ModelRouteKind::Anthropic,
                allowed_models: vec![SONNET.into(), HAIKU.into()],
                model_map: BTreeMap::from([(SONNET.to_owned(), VERTEX_SONNET.to_owned())]),
                max_tokens_cap: Some(8192),
            },
        )
        .await
        .unwrap();
        if options.with_key {
            vault
                .put(
                    &SecretScope::Org(org),
                    &SecretName::new(BYOK_KEY_SECRET).unwrap(),
                    SecretBytes::new(TENANT_KEY.as_bytes().to_vec()),
                    Activation::Now,
                    &Actor::System,
                )
                .await
                .unwrap();
        }

        let run = RunId::from_uuid(uuid::Uuid::from_u128(0x0c));
        let budget =
            Budget { max_wall: Duration::from_secs(600), max_turns: 40, max_cost_micros: options.budget_micros, max_gate_attempts: 2 };
        let ticket = TicketId::from_uuid(uuid::Uuid::from_u128(0x0d));
        let mut data = sample_run(org, run, RunKind::Triage, RunSubject::Ticket(ticket), budget, now).into_data();
        data.status = RunStatus::Running;
        let new =
            NewRun { run: Run::rehydrate(data), spec: sample_spec(RunKind::Triage), create_key: None, source_ready: true, affinity: None };
        runs.commit(org, RunWrite { inserts: vec![new], ..RunWrite::default() }).await.unwrap();

        let anthropic = MockServer::start().await;
        let vertex = MockServer::start().await;
        let metadata = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/computeMetadata/v1/instance/service-accounts/default/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"access_token": "ya29.adc", "expires_in": 3600})))
            .mount(&metadata)
            .await;
        let anthropic_url = options.anthropic_url.clone().unwrap_or_else(|| anthropic.uri());
        let adc = AdcTokenSource::metadata_server(Url::parse(&format!("{}/", metadata.uri())).unwrap()).unwrap();
        let upstream = RoutedUpstream::new(
            AnthropicApi::with_timeouts(&Url::parse(&anthropic_url).unwrap(), options.timeouts).unwrap(),
            Some(VertexUpstream::with_endpoint(adc, Url::parse(&vertex.uri()).unwrap(), options.timeouts).unwrap()),
        );
        let config = GatewayConfig {
            vertex: Some(VertexRouteConfig { project: "mage-prod".into(), region: "us-east5".into(), enabled_orgs: vec!["acme".into()] }),
            per_run_in_flight: options.per_run_in_flight,
            beta_allowlist_vertex: vec!["context-1m-2025-08-07".into()],
            pricing: BTreeMap::from([(SONNET.to_owned(), sonnet_price()), (HAIKU.to_owned(), sonnet_price())]),
            ..GatewayConfig::default()
        };
        let deps = GatewayDeps {
            tokens: tokens.clone(),
            runs: runs.clone(),
            orgs: orgs.clone(),
            vault: vault.clone(),
            upstream: Arc::new(upstream),
            meter: meter.clone(),
            audit: audit.clone(),
            feed: feed.clone(),
            clock: clock_dyn,
        };
        let forward = Arc::new(Forward::new(deps, config).unwrap());
        let shutdown = CancellationToken::new();
        let app = Router::new().nest("/llm/anthropic", router(GatewayState::new(forward, shutdown.clone())));
        Harness { app, shutdown, tokens, token_store, runs, orgs, meter, audit, feed, anthropic, vertex, _metadata: metadata, org, run }
    }

    fn token_for(&self, scope: TokenScope) -> String {
        self.tokens.mint(scope, time::Duration::hours(1)).unwrap().0.as_str().to_owned()
    }

    fn token(&self) -> String {
        self.token_for(TokenScope::Gateway { org: self.org, run: self.run, attempt: 1, models: vec![] })
    }

    async fn update_run(&self, f: impl FnOnce(&mut crate::ports::runs::RunRecord)) {
        let mut record = self.runs.get(self.org, self.run).await.unwrap();
        f(&mut record);
        self.runs.commit(self.org, RunWrite { updates: vec![RunUpdate::of(record)], ..RunWrite::default() }).await.unwrap();
    }

    async fn set_status(&self, status: RunStatus) {
        self.update_run(|r| {
            let mut data = r.run.clone().into_data();
            data.status = status;
            r.run = Run::rehydrate(data);
        })
        .await;
    }

    async fn mock_anthropic(&self, response: ResponseTemplate) {
        Mock::given(method("POST")).and(path("/v1/messages")).respond_with(response).mount(&self.anthropic).await;
    }

    async fn call(&self, request: Request<Body>) -> (StatusCode, axum::http::HeaderMap, Vec<u8>) {
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let headers = response.headers().clone();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap().to_vec();
        (status, headers, body)
    }

    async fn upstream_calls(&self) -> usize {
        self.anthropic.received_requests().await.unwrap().len() + self.vertex.received_requests().await.unwrap().len()
    }
}

fn post(route: &str) -> axum::http::request::Builder {
    Request::builder().method("POST").uri(format!("/llm/anthropic{route}")).header("content-type", "application/json")
}

fn messages(token: &str, body: &str) -> Request<Body> {
    post("/v1/messages").header("x-api-key", token).body(Body::from(body.to_owned())).unwrap()
}

fn sonnet_body(stream: bool) -> String {
    format!(r#"{{"model":"{SONNET}","max_tokens":1024,"stream":{stream},"messages":[{{"role":"user","content":"hi"}}]}}"#)
}

/// Asserts the Anthropic error shape and returns `(type, message)`.
fn anthropic_error(body: &[u8]) -> (String, String) {
    let v: Value = serde_json::from_slice(body).unwrap_or_else(|_| panic!("not JSON: {}", String::from_utf8_lossy(body)));
    assert_eq!(v["type"], "error");
    let message = v["error"]["message"].as_str().unwrap().to_owned();
    assert!(message.starts_with("mage: "), "{message}");
    (v["error"]["type"].as_str().unwrap().to_owned(), message)
}

#[tokio::test]
async fn credential_header_variants() {
    let h = Harness::new().await;
    h.mock_anthropic(ResponseTemplate::new(200).insert_header("content-type", "application/json").set_body_string(MESSAGE)).await;
    let token = h.token();
    let body = sonnet_body(false);
    let with = |headers: &[(&str, &str)]| {
        let mut b = post("/v1/messages");
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        b.body(Body::from(body.clone())).unwrap()
    };
    let bearer = format!("Bearer {token}");

    assert_eq!(h.call(with(&[("x-api-key", token.as_str())])).await.0, StatusCode::OK);
    assert_eq!(h.call(with(&[("authorization", bearer.as_str())])).await.0, StatusCode::OK);
    assert_eq!(
        h.call(with(&[("x-api-key", token.as_str()), ("authorization", bearer.as_str())])).await.0,
        StatusCode::OK,
        "both, same value"
    );
    let calls_ok = h.upstream_calls().await;

    let other = h.token();
    let other_bearer = format!("Bearer {other}");
    let (status, _, err) = h.call(with(&[("x-api-key", token.as_str()), ("authorization", other_bearer.as_str())])).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED, "both present and different");
    assert_eq!(anthropic_error(&err).0, "authentication_error");
    for headers in [
        &[][..],
        &[("authorization", "Basic dXNlcjpwYXNz")][..],
        &[("x-api-key", "mgg_not.a.token")][..],
        &[("cookie", "__Host-mage_sid=abc")][..],
    ] {
        let (status, _, err) = h.call(with(headers)).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{headers:?}");
        assert_eq!(anthropic_error(&err).0, "authentication_error");
    }
    // A valid token of another kind fails on the route's expected kind.
    let artifact = h.token_for(TokenScope::Artifact { org: h.org, artifact: ArtifactId::from_uuid(uuid::Uuid::from_u128(9)) });
    assert_eq!(h.call(with(&[("x-api-key", artifact.as_str())])).await.0, StatusCode::UNAUTHORIZED);
    // Revoked jti.
    let (revoked, claims) =
        h.tokens.mint(TokenScope::Gateway { org: h.org, run: h.run, attempt: 1, models: vec![] }, time::Duration::hours(1)).unwrap();
    h.token_store
        .revoke(&Revocation { jti: Jti(*claims.jti.as_bytes()), org_id: Some(h.org), reason: "test".into(), expires_at: claims.exp })
        .await
        .unwrap();
    assert_eq!(h.call(with(&[("x-api-key", revoked.as_str())])).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(h.upstream_calls().await, calls_ok, "rejected calls never reach the upstream");

    let sent = h.anthropic.received_requests().await.unwrap();
    assert!(
        sent.iter().all(|r| r.headers.get("x-api-key").unwrap() == TENANT_KEY),
        "the tenant key, never the gateway token, goes upstream"
    );
    assert!(sent.iter().all(|r| r.headers.get("authorization").is_none() && r.headers.get("cookie").is_none()));
}

#[tokio::test]
async fn run_state_and_attempt_gate_every_call() {
    let h = Harness::new().await;
    h.mock_anthropic(ResponseTemplate::new(200).set_body_string(MESSAGE)).await;
    let token = h.token();

    h.set_status(RunStatus::AwaitingGate).await;
    assert_eq!(h.call(messages(&token, &sonnet_body(false))).await.0, StatusCode::OK, "AwaitingGate: the harness closes its turn");

    for terminal in
        [RunStatus::Succeeded, RunStatus::Failed, RunStatus::TimedOut, RunStatus::Cancelled, RunStatus::Errored, RunStatus::Queued]
    {
        h.set_status(terminal).await;
        let (status, _, err) = h.call(messages(&token, &sonnet_body(false))).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{terminal:?}");
        assert_eq!(anthropic_error(&err), ("authentication_error".into(), "mage: run is not active".into()));
    }

    h.set_status(RunStatus::Running).await;
    let stale = h.token_for(TokenScope::Gateway { org: h.org, run: h.run, attempt: 2, models: vec![] });
    assert_eq!(h.call(messages(&stale, &sonnet_body(false))).await.0, StatusCode::UNAUTHORIZED, "attempt mismatch");
    let unknown =
        h.token_for(TokenScope::Gateway { org: h.org, run: RunId::from_uuid(uuid::Uuid::from_u128(99)), attempt: 1, models: vec![] });
    assert_eq!(h.call(messages(&unknown, &sonnet_body(false))).await.0, StatusCode::UNAUTHORIZED, "unknown run");
    assert_eq!(h.upstream_calls().await, 1);
}

#[tokio::test]
async fn budget_precheck_and_deny_stop_the_next_call() {
    // Each call costs 1928 µ$; the budget is 2000 µ$: call 1 allowed, call 2 crosses it (Deny),
    // call 3 is refused before reaching the upstream.
    let h = Harness::with(Options { budget_micros: 2_000, ..Options::default() }).await;
    h.mock_anthropic(ResponseTemplate::new(200).set_body_raw(SSE, "text/event-stream")).await;
    let token = h.token();
    assert_eq!(h.call(messages(&token, &sonnet_body(true))).await.0, StatusCode::OK);
    h.meter.wait_recorded(1).await;
    assert_eq!(h.call(messages(&token, &sonnet_body(true))).await.0, StatusCode::OK, "within budget at the pre-check");
    h.meter.wait_recorded(2).await;

    let (status, _, err) = h.call(messages(&token, &sonnet_body(true))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (kind, message) = anthropic_error(&err);
    assert_eq!(kind, "permission_error");
    assert!(message.contains("budget"), "{message}");
    assert_eq!(h.upstream_calls().await, 2);
    assert_eq!(h.runs.get(h.org, h.run).await.unwrap().usage_metered.cost_micros, 2 * 1928);
}

#[tokio::test]
async fn streaming_is_passed_through_and_metered_from_the_stream() {
    let h = Harness::new().await;
    h.mock_anthropic(
        ResponseTemplate::new(200)
            .insert_header("request-id", "req_up")
            .insert_header("x-envoy-upstream-service-time", "12")
            .set_body_raw(SSE, "text/event-stream"),
    )
    .await;
    let (status, headers, body) = h.call(messages(&h.token(), &sonnet_body(true))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(headers.get("content-type").unwrap(), "text/event-stream");
    assert_eq!(headers.get("request-id").unwrap(), "req_up");
    assert!(headers.get("x-envoy-upstream-service-time").is_none());
    assert_eq!(body, SSE.as_bytes(), "bytes as received, pings included");

    let recorded = h.meter.wait_recorded(1).await;
    assert_eq!(recorded, vec![(h.org, h.run, sse_usage())]);

    // Non-streaming: metered from the body's usage.
    h.anthropic.reset().await;
    h.mock_anthropic(ResponseTemplate::new(200).set_body_string(MESSAGE)).await;
    let (status, _, body) = h.call(messages(&h.token(), &sonnet_body(false))).await;
    assert_eq!((status, body.as_slice()), (StatusCode::OK, MESSAGE.as_bytes()));
    let recorded = h.meter.wait_recorded(2).await;
    assert_eq!(recorded[1].2, Usage { input_tokens: 10, output_tokens: 4, cost_micros: 90, turns: 1, wall: Duration::ZERO });
}

#[tokio::test]
async fn count_tokens_is_forwarded_and_not_metered() {
    let h = Harness::new().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages/count_tokens"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"input_tokens":42}"#))
        .expect(1)
        .mount(&h.anthropic)
        .await;
    let body = format!(r#"{{"model":"{SONNET}","messages":[{{"role":"user","content":"hi"}}]}}"#);
    let request =
        post("/v1/messages/count_tokens").header("authorization", format!("Bearer {}", h.token())).body(Body::from(body.clone())).unwrap();
    let (status, _, out) = h.call(request).await;
    assert_eq!((status, out.as_slice()), (StatusCode::OK, br#"{"input_tokens":42}"#.as_slice()));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(h.meter.recorded.lock().is_empty());
}

#[tokio::test]
async fn model_policy_and_key_are_enforced_before_the_upstream() {
    let h = Harness::new().await;
    h.mock_anthropic(ResponseTemplate::new(200).set_body_string(MESSAGE)).await;
    let token = h.token();
    let cases = [
        (r#"{"model":"claude-opus-4-1","max_tokens":10,"messages":[]}"#.to_owned(), "not allowed"),
        (format!(r#"{{"model":"{SONNET}","max_tokens":8193,"messages":[]}}"#), "max_tokens"),
        (r#"{"max_tokens":10,"messages":[]}"#.to_owned(), "model is required"),
        (format!(r#"{{"model":"{HAIKU}","model":"{SONNET}","max_tokens":10}}"#), "duplicate"),
        ("[1,2,3]".to_owned(), "request body"),
    ];
    for (body, needle) in cases {
        let (status, _, err) = h.call(messages(&token, &body)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        let (kind, message) = anthropic_error(&err);
        assert_eq!(kind, "invalid_request_error");
        assert!(message.contains(needle), "{message} should mention {needle}");
    }
    // The token's model list narrows the org allowlist.
    let haiku_only = h.token_for(TokenScope::Gateway { org: h.org, run: h.run, attempt: 1, models: vec![HAIKU.into()] });
    assert_eq!(h.call(messages(&haiku_only, &sonnet_body(false))).await.0, StatusCode::BAD_REQUEST);
    let haiku = format!(r#"{{"model":"{HAIKU}","max_tokens":8192,"messages":[]}}"#);
    assert_eq!(h.call(messages(&haiku_only, &haiku)).await.0, StatusCode::OK, "max_tokens at the cap is allowed");
    assert_eq!(h.upstream_calls().await, 1);

    let no_key = Harness::with(Options { with_key: false, ..Options::default() }).await;
    let (status, _, err) = no_key.call(messages(&no_key.token(), &sonnet_body(false))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(anthropic_error(&err), ("permission_error".into(), "mage: no model key configured".into()));
}

#[tokio::test]
async fn vertex_route_rewrites_the_body_and_filters_betas() {
    let h = Harness::new().await;
    let mut settings = h.orgs.model_settings(h.org).await.unwrap();
    settings.route = ModelRouteKind::Vertex;
    h.orgs.set_model_settings(h.org, &settings).await.unwrap();
    Mock::given(method("POST"))
        .and(path(format!("/v1/projects/mage-prod/locations/us-east5/publishers/anthropic/models/{VERTEX_SONNET}:streamRawPredict")))
        .respond_with(ResponseTemplate::new(200).set_body_raw(SSE, "text/event-stream"))
        .expect(1)
        .mount(&h.vertex)
        .await;
    let body = sonnet_body(true);
    let request = post("/v1/messages")
        .header("x-api-key", h.token())
        .header("anthropic-beta", "context-1m-2025-08-07, interleaved-thinking-2025-05-14")
        .header("anthropic-version", "2023-06-01")
        .body(Body::from(body.clone()))
        .unwrap();
    let (status, _, out) = h.call(request).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(out, SSE.as_bytes());
    assert_eq!(h.meter.wait_recorded(1).await[0].2, sse_usage(), "priced by the requested Anthropic id");

    let sent = &h.vertex.received_requests().await.unwrap()[0];
    let top: serde_json::Map<String, Value> = serde_json::from_slice(&sent.body).unwrap();
    assert!(!top.contains_key("model"));
    assert_eq!(top["anthropic_version"], "vertex-2023-10-16");
    assert_eq!(top["messages"], serde_json::json!([{"role": "user", "content": "hi"}]));
    assert_eq!(sent.headers.get("authorization").unwrap(), "Bearer ya29.adc");
    assert_eq!(sent.headers.get("anthropic-beta").unwrap(), "context-1m-2025-08-07", "betas outside the Vertex allowlist dropped");
    assert!(sent.headers.get("x-api-key").is_none());

    // count_tokens is not served on Vertex: Anthropic-shaped 404 so Claude Code falls back.
    let count = format!(r#"{{"model":"{SONNET}","messages":[]}}"#);
    let request = post("/v1/messages/count_tokens").header("x-api-key", h.token()).body(Body::from(count.clone())).unwrap();
    let (status, _, err) = h.call(request).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(anthropic_error(&err).0, "not_found_error");

    // A model without a Vertex mapping is refused.
    let haiku = format!(r#"{{"model":"{HAIKU}","max_tokens":10,"messages":[]}}"#);
    assert_eq!(h.call(messages(&h.token(), &haiku)).await.0, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn upstream_errors_are_mapped() {
    let h = Harness::new().await;
    let overloaded = r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#;
    h.mock_anthropic(
        ResponseTemplate::new(429)
            .insert_header("retry-after", "3")
            .insert_header("content-type", "application/json")
            .set_body_string(overloaded),
    )
    .await;
    let (status, headers, body) = h.call(messages(&h.token(), &sonnet_body(true))).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "upstream status passed through");
    assert_eq!(headers.get("retry-after").unwrap(), "3");
    assert_eq!(body, overloaded.as_bytes());

    // Tenant key rejected: 403 to the harness, run failed with ModelAuth, audit + settings feed.
    h.anthropic.reset().await;
    h.mock_anthropic(
        ResponseTemplate::new(401)
            .set_body_string(r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#),
    )
    .await;
    let settings_notices = |h: &Harness| h.feed.entries(h.org).iter().filter(|e| e.kind == FeedKind::Settings).count();
    let notices_before = settings_notices(&h);
    let (status, _, err) = h.call(messages(&h.token(), &sonnet_body(false))).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(anthropic_error(&err), ("permission_error".into(), "mage: tenant model key rejected".into()));
    let failed = h.meter.failed.lock().clone();
    assert_eq!(failed.len(), 1);
    assert_eq!((failed[0].0, failed[0].1, failed[0].2.code), (h.org, h.run, RunErrorCode::ModelAuth));
    let audit = h.audit.records();
    let rejected = audit.iter().find(|r| r.event.action == AuditAction::ByokKeyRejected).expect("byok.key_rejected audited");
    assert_eq!(rejected.event.org, Some(h.org));
    assert!(rejected.event.details.iter().all(|(_, v)| !v.contains(TENANT_KEY)), "never the key value");
    assert_eq!(settings_notices(&h), notices_before + 1, "one settings notice for the rejected key");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(h.meter.recorded.lock().is_empty(), "errors without usage are not metered");

    // Upstream timeout → 504 timeout_error; refused connection → 503 api_error.
    let slow = Harness::with(Options {
        timeouts: UpstreamTimeouts {
            connect: Duration::from_secs(2),
            total: Duration::from_millis(200),
            stream_idle: Duration::from_millis(200),
        },
        ..Options::default()
    })
    .await;
    slow.mock_anthropic(ResponseTemplate::new(200).set_delay(Duration::from_secs(3)).set_body_string(MESSAGE)).await;
    let (status, _, err) = slow.call(messages(&slow.token(), &sonnet_body(false))).await;
    assert_eq!((status, anthropic_error(&err).0.as_str()), (StatusCode::GATEWAY_TIMEOUT, "timeout_error"));

    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let dead = format!("http://{}", closed.local_addr().unwrap());
    drop(closed);
    let down = Harness::with(Options { anthropic_url: Some(dead), ..Options::default() }).await;
    let (status, _, err) = down.call(messages(&down.token(), &sonnet_body(false))).await;
    assert_eq!((status, anthropic_error(&err).0.as_str()), (StatusCode::SERVICE_UNAVAILABLE, "api_error"));
}

#[tokio::test]
async fn unknown_routes_and_methods_get_anthropic_404s() {
    let h = Harness::new().await;
    for (method, uri) in
        [("POST", "/llm/anthropic/v1/complete"), ("GET", "/llm/anthropic/v1/messages"), ("GET", "/llm/anthropic/v1/models")]
    {
        let request = Request::builder().method(method).uri(uri).header("x-api-key", h.token()).body(Body::empty()).unwrap();
        let (status, _, body) = h.call(request).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{method} {uri}");
        assert_eq!(anthropic_error(&body).0, "not_found_error");
    }
}

// --- streams held open: per-run cap and client disconnect -------------------------------------

/// One-connection SSE upstream that sends `message_start` and a text delta, then waits for `close`.
async fn stalling_upstream() -> (String, tokio::sync::oneshot::Sender<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (close, closed) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let mut closed = closed;
        loop {
            tokio::select! {
                accepted = listener.accept() => {
                    let (mut sock, _) = accepted.unwrap();
                    tokio::spawn(async move {
                        read_request(&mut sock).await;
                        sock.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ntransfer-encoding: chunked\r\n\r\n").await.unwrap();
                        let start = "data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-sonnet-4-5\",\"usage\":{\"input_tokens\":200,\"output_tokens\":1}}}\n\n\
                                     data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"0123456789abcdefghij\"}}\n\n";
                        sock.write_all(format!("{:x}\r\n{start}\r\n", start.len()).as_bytes()).await.unwrap();
                        let mut buf = [0u8; 1];
                        let _ = sock.read(&mut buf).await; // hold until the client goes away
                    });
                }
                _ = &mut closed => return,
            }
        }
    });
    (format!("http://{addr}"), close)
}

async fn read_request(sock: &mut TcpStream) {
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n\r\n") {
        sock.read_exact(&mut byte).await.unwrap();
        buf.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&buf).to_ascii_lowercase();
    let len = head.lines().find_map(|l| l.strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap())).unwrap_or(0);
    let mut body = vec![0u8; len];
    sock.read_exact(&mut body).await.unwrap();
}

#[tokio::test]
async fn per_run_cap_and_client_disconnect_settle_partial_usage() {
    let (upstream, _close) = stalling_upstream().await;
    let h = Harness::with(Options { per_run_in_flight: 2, anthropic_url: Some(upstream), ..Options::default() }).await;
    let token = h.token();

    let open = |h: &Harness| {
        let app = h.app.clone();
        let request = messages(&token, &sonnet_body(true));
        async move { app.oneshot(request).await.unwrap() }
    };
    let first = open(&h).await;
    let second = open(&h).await;
    assert_eq!((first.status(), second.status()), (StatusCode::OK, StatusCode::OK));

    let (status, headers, err) = h.call(messages(&token, &sonnet_body(true))).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "third concurrent call for the run");
    assert_eq!(headers.get("retry-after").unwrap(), "1");
    assert_eq!(anthropic_error(&err).0, "rate_limit_error");

    // The client reads the first chunk, then disconnects: partial usage is settled (input exact,
    // output estimated from 20 streamed text bytes = 5 tokens, no turn), and the slot is freed.
    let mut chunks = first.into_body().into_data_stream();
    let chunk = tokio::time::timeout(Duration::from_secs(5), chunks.next()).await.unwrap().unwrap().unwrap();
    assert!(chunk.starts_with(b"data: {\"type\":\"message_start\""));
    drop(chunks);
    let recorded = h.meter.wait_recorded(1).await;
    assert_eq!(recorded[0].2, Usage { input_tokens: 200, output_tokens: 5, cost_micros: 675, turns: 0, wall: Duration::ZERO });

    let third = open(&h).await;
    assert_eq!(third.status(), StatusCode::OK, "the disconnected call released its slot");
    drop((second, third));
}

#[tokio::test]
async fn shutdown_ends_open_streams_and_still_meters_them() {
    let (upstream, _close) = stalling_upstream().await;
    let h = Harness::with(Options { anthropic_url: Some(upstream), ..Options::default() }).await;
    let response = h.app.clone().oneshot(messages(&h.token(), &sonnet_body(true))).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut chunks = response.into_body().into_data_stream();
    assert!(chunks.next().await.unwrap().is_ok());

    h.shutdown.cancel();
    let rest = tokio::time::timeout(Duration::from_secs(5), chunks.next()).await.expect("stream must end on shutdown");
    assert!(rest.is_none(), "the body ends cleanly");
    drop(chunks);
    let recorded = h.meter.wait_recorded(1).await;
    assert_eq!((recorded[0].2.input_tokens, recorded[0].2.turns), (200, 0));
}
