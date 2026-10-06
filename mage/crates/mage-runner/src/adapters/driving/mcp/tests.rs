//! Run MCP adapter over real HTTP on loopback (executor.md "Testing": 401 without or with a wrong
//! bearer, `Origin` rejected, unknown tool, a cancelled call kills its work), driven by the rmcp
//! client.

use std::sync::Arc;
use std::time::Duration;

use mage_core::domain::{CommitSha, OrgId, RepoId, RunId, RunKind, RunStatus};
use mage_core::testing::{FakeClock, SeqIdGen};
use mage_core::wire::runner::SubmitAck;
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;

use super::*;
use crate::app::runner::tools::{RunToolsConfig, RunToolsDeps, RunToolsInit};
use crate::ports::executor::{Env, EnvPath};
use crate::ports::runner_state::RunnerState;
use crate::testing::browser::FakeBrowserRunner;
use crate::testing::executor::FakeEnv;
use crate::testing::link::FakeServerLink;
use crate::testing::runner_state::MemRunnerState;

const RUN: RunId = RunId::from_uuid(uuid::Uuid::from_u128(0x42));

struct H {
    server: RunMcpServer,
    state: Arc<MemRunnerState>,
    tools: Arc<RunTools>,
    browser: Arc<FakeBrowserRunner>,
    link: Arc<FakeServerLink>,
    _outbox: tempfile::TempDir,
}

/// A free loopback port to start the pool at.
async fn free_port() -> u16 {
    tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap().port()
}

async fn harness(kind: RunKind, start: u16) -> H {
    let clock: Arc<dyn Clock> = Arc::new(FakeClock::new());
    let state = MemRunnerState::new(clock.clone());
    let env = FakeEnv::empty();
    env.set_file(&EnvPath::ws("e2e/repro.spec.ts").unwrap(), b"test");
    let outbox = tempfile::tempdir().unwrap();
    let link = FakeServerLink::new();
    let browser = Arc::new(FakeBrowserRunner::new());
    let (events, _) = mpsc::unbounded_channel();
    let (submissions, _) = mpsc::unbounded_channel();
    let init = RunToolsInit {
        run_id: RUN,
        org_id: OrgId::from_uuid(uuid::Uuid::from_u128(1)),
        kind,
        attempt: 1,
        repo_id: Some(RepoId::from_uuid(uuid::Uuid::from_u128(2))),
        sha: Some(CommitSha::from_bytes([3; 20])),
        env: Some(env as Arc<dyn Env>),
        fix: None,
        outbox: outbox.path().to_path_buf(),
        events,
        submissions,
    };
    let deps = RunToolsDeps { browser: browser.clone(), link: link.clone(), clock: clock.clone(), ids: Arc::new(SeqIdGen::new()) };
    let tools = Arc::new(RunTools::new(init, deps, RunToolsConfig::default()).unwrap());
    let server = RunMcpServer::new(state.clone(), clock, RunMcpConfig { port_range: [start, start.saturating_add(20)] });
    H { server, state, tools, browser, link, _outbox: outbox }
}

async fn client(endpoint: &RunMcpEndpoint) -> rmcp::service::RunningService<rmcp::RoleClient, ()> {
    let config = StreamableHttpClientTransportConfig::with_uri(endpoint.url.as_str()).auth_header(endpoint.bearer.clone());
    ().serve(StreamableHttpClientTransport::from_config(config)).await.unwrap()
}

fn args(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

#[tokio::test]
async fn guard_rejects_missing_or_wrong_bearer_origin_and_host() {
    let h = harness(RunKind::Triage, free_port().await).await;
    let handle = h.server.start(h.tools.clone()).await.unwrap();
    let endpoint = handle.endpoint().clone();
    assert!(endpoint.url.starts_with("http://127.0.0.1:") && endpoint.url.ends_with("/mcp"));
    assert_eq!(base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(&endpoint.bearer).unwrap().len(), 32);
    let port: u16 = endpoint.url.trim_start_matches("http://127.0.0.1:").trim_end_matches("/mcp").parse().unwrap();

    let http = reqwest::Client::builder().no_proxy().build().unwrap();
    let body = json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"});
    let post = |auth: Option<String>, extra: Option<(&'static str, String)>| {
        let mut req = http
            .post(&endpoint.url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .json(&body);
        if let Some(auth) = auth {
            req = req.header("authorization", auth);
        }
        if let Some((name, value)) = extra {
            req = req.header(name, value);
        }
        req.send()
    };
    assert_eq!(post(None, None).await.unwrap().status(), 401);
    assert_eq!(post(Some("Bearer wrong".into()), None).await.unwrap().status(), 401);
    let mut almost = endpoint.bearer.clone();
    almost.pop();
    almost.push('x');
    assert_eq!(post(Some(format!("Bearer {almost}")), None).await.unwrap().status(), 401, "same length, wrong value");
    let good = Some(format!("Bearer {}", endpoint.bearer));
    assert_eq!(post(good.clone(), Some(("origin", "http://evil.example".into()))).await.unwrap().status(), 403);
    assert_eq!(post(good.clone(), Some(("host", format!("localhost:{port}")))).await.unwrap().status(), 403);
    assert_eq!(post(good.clone(), Some(("host", "127.0.0.1:1".into()))).await.unwrap().status(), 403);
    let other_path = http.post(endpoint.url.replace("/mcp", "/other")).header("authorization", good.clone().unwrap()).send().await.unwrap();
    assert_eq!(other_path.status(), 404);
    handle.shutdown().await;
}

#[tokio::test]
async fn lists_and_calls_tools_over_mcp() {
    let h = harness(RunKind::Triage, free_port().await).await;
    let handle = h.server.start(h.tools.clone()).await.unwrap();
    let mcp = client(handle.endpoint()).await;

    let tools = mcp.list_tools(None).await.unwrap().tools;
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    assert_eq!(names, ["submit_triage"], "only the Triage tool set is listed");
    assert_eq!(tools[0].input_schema.get("$id"), Some(&json!("mage:submit_triage:v1")));

    let unknown = mcp.call_tool(CallToolRequestParams::new("exec").with_arguments(args(json!({"command": "id"})))).await;
    assert!(unknown.is_err(), "a tool outside the set is a protocol error");

    let bad = mcp.call_tool(CallToolRequestParams::new("submit_triage").with_arguments(args(json!({"summary": "s"})))).await.unwrap();
    assert_eq!(bad.is_error, Some(true));
    let text = serde_json::to_string(&bad.content).unwrap();
    assert!(text.contains("missing_info: is required"), "{text}");

    h.link.script_submit(Ok(SubmitAck { status: RunStatus::Succeeded, gate: None, message: "Accepted. End your turn now.".into() }));
    let ok = mcp
        .call_tool(CallToolRequestParams::new("submit_triage").with_arguments(args(json!({"summary": "s", "causes": [], "missing_info": ["q"]}))))
        .await
        .unwrap();
    assert_eq!(ok.is_error, Some(false));
    assert!(serde_json::to_string(&ok.content).unwrap().contains("Accepted. End your turn now."));
    let _ = mcp.cancel().await;
    handle.shutdown().await;
}

#[tokio::test]
async fn shutdown_cancels_in_flight_calls_and_releases_the_port() {
    let h = harness(RunKind::Diagnose, free_port().await).await;
    h.browser.set_delay(Duration::from_secs(60));
    let handle = h.server.start(h.tools.clone()).await.unwrap();
    assert_eq!(h.state.port_leases().await.unwrap().iter().filter(|l| l.owner == PortOwner::RunMcp(RUN)).count(), 1);
    let mcp = Arc::new(client(handle.endpoint()).await);
    let call = {
        let mcp = mcp.clone();
        tokio::spawn(async move {
            mcp.call_tool(CallToolRequestParams::new("playwright_run").with_arguments(args(json!({"script": "e2e/repro.spec.ts"})))).await
        })
    };
    for _ in 0..100 {
        if !h.browser.calls().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(h.browser.calls().len(), 1, "the call reached the browser runner");
    tokio::time::timeout(Duration::from_secs(10), handle.shutdown()).await.expect("shutdown returns promptly");
    assert_eq!(h.browser.cancelled(), 1, "the in-flight run was dropped");
    assert!(h.tools.is_shut_down());
    assert!(h.state.port_leases().await.unwrap().iter().all(|l| l.owner != PortOwner::RunMcp(RUN)), "port released");
    let _ = tokio::time::timeout(Duration::from_secs(10), call).await;
    handle.shutdown().await;
}

#[tokio::test]
async fn a_taken_port_is_quarantined_and_another_leased() {
    let blocker = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let taken = blocker.local_addr().unwrap().port();
    let h = harness(RunKind::Triage, taken).await;
    let handle = h.server.start(h.tools.clone()).await.unwrap();
    assert!(!handle.endpoint().url.contains(&format!(":{taken}/")), "{}", handle.endpoint().url);
    let leases = h.state.port_leases().await.unwrap();
    assert!(leases.iter().all(|l| l.port != taken), "the taken port is not leased");
    handle.shutdown().await;
    drop(blocker);
}
