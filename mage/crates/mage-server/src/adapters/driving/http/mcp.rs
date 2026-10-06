//! `POST /mcp/knowledge` (server.md "Endpoint catalog", "Knowledge MCP"): the rmcp streamable HTTP
//! service, stateless (no MCP session between requests: Cloud Run spreads requests over
//! instances), JSON responses where the client accepts them, behind `KnowledgeAuth`:
//! `Authorization: Bearer mgk_…` → `AuthorizeKnowledge` → `KnowledgeScope` in the request
//! extensions. No or refused token → 401 before MCP handling; GET/DELETE → 405. Body ≤ 1 MiB.
//! `allowed_hosts` = the public host(s); any `Origin` is rejected (harness clients send none).

use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::post_service;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::adapters::driving::mcp::knowledge::{KnowledgeMcp, RunLimiter, tool_table};
use crate::app::knowledge_query::{AuthorizeKnowledge, McpLimits, Service};
use crate::error::AppError;

pub const PATH: &str = "/mcp/knowledge";
/// Request body limit of `/mcp/knowledge`.
pub const MAX_BODY_BYTES: usize = 1024 * 1024;

#[derive(Clone)]
pub struct KnowledgeMcpState {
    pub auth: Arc<AuthorizeKnowledge>,
    pub service: Arc<Service>,
    pub limits: McpLimits,
    /// Public host names (`mage.example.com`, `mage.example.com:443`); empty keeps rmcp's
    /// loopback-only default (local development).
    pub allowed_hosts: Vec<String>,
    /// Server shutdown: ends in-flight MCP streams.
    pub shutdown: CancellationToken,
}

/// The `/mcp/knowledge` router (state applied). Fails only if a tool input type does not
/// produce an object schema (a programming error caught by the tests).
pub fn router(state: KnowledgeMcpState) -> Result<Router, String> {
    let tools: Arc<[rmcp::model::Tool]> = tool_table()?.into();
    let limiter = Arc::new(RunLimiter::new(state.limits));
    let service = state.service.clone();
    let mut config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_sse_keep_alive(None)
        .with_max_request_body_bytes(MAX_BODY_BYTES)
        .with_cancellation_token(state.shutdown.child_token())
        .enforce_origin_validation();
    if !state.allowed_hosts.is_empty() {
        config = config.with_allowed_hosts(state.allowed_hosts.clone());
    }
    let mcp = StreamableHttpService::new(
        move || Ok(KnowledgeMcp::new(service.clone(), limiter.clone(), tools.clone())),
        Arc::new(NeverSessionManager::default()),
        config,
    );
    let route = post_service(mcp).get(method_not_allowed).delete(method_not_allowed);
    Ok(Router::new().route(PATH, route).layer(middleware::from_fn_with_state(state.auth.clone(), knowledge_auth)))
}

async fn method_not_allowed() -> Response {
    (StatusCode::METHOD_NOT_ALLOWED, [(header::ALLOW, "POST")]).into_response()
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim()).filter(|t| !t.is_empty())
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::WWW_AUTHENTICATE, "Bearer")],
        axum::Json(json!({ "error": { "code": "unauthenticated", "message": "a valid knowledge token is required" } })),
    )
        .into_response()
}

/// `KnowledgeAuth`: verify the bearer and insert the `KnowledgeScope`.
async fn knowledge_auth(State(auth): State<Arc<AuthorizeKnowledge>>, mut request: Request, next: Next) -> Response {
    if request.method() != axum::http::Method::POST {
        return next.run(request).await;
    }
    let Some(token) = bearer(request.headers()) else { return unauthorized() };
    match auth.authorize(token).await {
        Ok(scope) => {
            request.extensions_mut().insert(scope);
            next.run(request).await
        }
        Err(AppError::Unavailable(message)) => {
            tracing::warn!(%message, "knowledge auth unavailable");
            (StatusCode::SERVICE_UNAVAILABLE, axum::Json(json!({ "error": { "code": "unavailable", "message": "retry later" } }))).into_response()
        }
        Err(_) => unauthorized(),
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::body::{Body, to_bytes};
    use axum::http::Request as HttpRequest;
    use mage_core::domain::{Budget, RunId, RunKind, RunStatus, RunSubject, TicketId};
    use mage_core::ports::clock::Clock;
    use mage_core::testing::SeqIdGen;
    use mage_core::wire::token::{MemoryCutoff, TokenScope};
    use serde_json::Value;
    use tower::ServiceExt;
    use uuid::Uuid;

    use super::*;
    use crate::app::auth::tokens::TokenService;
    use crate::app::knowledge_query::tests::Fx;
    use crate::app::knowledge_query::{ServiceDeps, SourceReader};
    use crate::ports::runs::{NewRun, RunStore, RunWrite};
    use crate::testing::runs::{sample_run, sample_spec};
    use crate::testing::{FakeTokenSigner, MemTokenStore};

    async fn setup() -> (Router, String, Fx) {
        let fx = Fx::new().await;
        fx.file("a.go", "package a\n").await;
        let tokens = Arc::new(TokenService::new(
            Arc::new(FakeTokenSigner::new("https://mage.test")),
            Arc::new(MemTokenStore::new(fx.clock.clone())),
            fx.clock.clone(),
            Arc::new(SeqIdGen::new()),
        ));
        let run = RunId::from_uuid(Uuid::from_u128(0x0c55));
        let budget = Budget { max_wall: Duration::from_secs(600), max_turns: 10, max_cost_micros: 1, max_gate_attempts: 1 };
        let subject = RunSubject::Ticket(TicketId::from_uuid(Uuid::from_u128(5)));
        let mut data = sample_run(fx.org, run, RunKind::Triage, subject, budget, fx.clock.now()).into_data();
        data.status = RunStatus::Running;
        let insert = NewRun { run: mage_core::domain::Run::rehydrate(data), spec: sample_spec(RunKind::Triage), create_key: None, source_ready: true, affinity: None };
        fx.runs.commit(fx.org, RunWrite { inserts: vec![insert], ..Default::default() }).await.unwrap_or_else(|e| panic!("{e}"));
        let scope = TokenScope::Knowledge {
            org: fx.org,
            run,
            attempt: 1,
            repo: fx.repo,
            sha: fx.sha,
            memory: MemoryCutoff::default(),
            disabled_layers: vec![],
        };
        let (token, _) = tokens.mint(scope, time::Duration::hours(1)).unwrap_or_else(|e| panic!("{e}"));
        let service = Arc::new(Service::new(ServiceDeps {
            knowledge: fx.knowledge.clone(),
            repos: fx.repos.clone(),
            tickets: fx.tickets.clone(),
            runs: fx.runs.clone(),
            indexer: fx.indexer.clone(),
            source: SourceReader::new(fx.knowledge.clone(), fx.artifacts.clone()),
        }));
        let state = KnowledgeMcpState {
            auth: Arc::new(AuthorizeKnowledge::new(tokens, fx.runs.clone(), fx.clock.clone())),
            service,
            limits: McpLimits::default(),
            allowed_hosts: Vec::new(),
            shutdown: CancellationToken::new(),
        };
        (router(state).unwrap_or_else(|e| panic!("{e}")), token.into_string(), fx)
    }

    fn rpc(token: Option<&str>, body: Value) -> HttpRequest<Body> {
        let mut builder = HttpRequest::builder()
            .method("POST")
            .uri(PATH)
            .header("host", "localhost")
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream");
        if let Some(token) = token {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        builder.body(Body::from(body.to_string())).unwrap_or_else(|e| panic!("{e}"))
    }

    async fn json_of(response: Response) -> Value {
        let bytes = to_bytes(response.into_body(), 1 << 20).await.unwrap_or_else(|e| panic!("{e}"));
        let text = String::from_utf8_lossy(&bytes).into_owned();
        // JSON responses; an SSE fallback carries the message in a `data:` line.
        let payload = text.lines().find_map(|l| l.strip_prefix("data:")).map(str::trim).unwrap_or(&text).to_owned();
        serde_json::from_str(&payload).unwrap_or_else(|e| panic!("{e}: {text}"))
    }

    #[tokio::test]
    async fn requests_without_a_valid_token_never_reach_mcp() {
        let (app, _, _) = setup().await;
        let list = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" });
        let response = app.clone().oneshot(rpc(None, list.clone())).await.unwrap_or_else(|e| match e {});
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let response = app.clone().oneshot(rpc(Some("mgk_forged.token.x"), list)).await.unwrap_or_else(|e| match e {});
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let get = HttpRequest::builder().method("GET").uri(PATH).header("host", "localhost").body(Body::empty()).unwrap_or_else(|e| panic!("{e}"));
        assert_eq!(app.oneshot(get).await.unwrap_or_else(|e| match e {}).status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn stateless_tools_list_and_calls_enforce_the_token_scope() {
        let (app, token, fx) = setup().await;
        let list = app
            .clone()
            .oneshot(rpc(Some(&token), serde_json::json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" })))
            .await
            .unwrap_or_else(|e| match e {});
        assert_eq!(list.status(), StatusCode::OK);
        let list = json_of(list).await;
        let names: Vec<&str> = list["result"]["tools"].as_array().into_iter().flatten().filter_map(|t| t["name"].as_str()).collect();
        assert_eq!(names, crate::app::knowledge_query::KNOWLEDGE_TOOLS.to_vec());

        let call = |id: u32, args: Value| {
            rpc(Some(&token), serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call", "params": { "name": "read_file", "arguments": args } }))
        };
        let ok = json_of(app.clone().oneshot(call(2, serde_json::json!({ "path": "a.go" }))).await.unwrap_or_else(|e| match e {})).await;
        assert_eq!(ok["result"]["isError"], false, "{ok}");
        assert_eq!(ok["result"]["structuredContent"]["text"], "     1\tpackage a");
        assert_eq!(ok["result"]["structuredContent"]["sha"], fx.sha.to_hex());

        let other_sha = "01".repeat(20);
        let out = json_of(app.clone().oneshot(call(3, serde_json::json!({ "path": "a.go", "sha": other_sha }))).await.unwrap_or_else(|e| match e {})).await;
        assert_eq!(out["result"]["isError"], true, "{out}");
        assert_eq!(out["result"]["structuredContent"]["error"]["code"], "out_of_scope");

        let bad = json_of(app.oneshot(call(4, serde_json::json!({ "path": "a.go", "nope": true }))).await.unwrap_or_else(|e| match e {})).await;
        assert_eq!(bad["error"]["code"], -32602, "{bad}");
    }
}
