//! Claude Code adapter tests: config writers, inventory, turn bookkeeping, and golden
//! stream-json transcripts recorded from Claude Code 2.1.269 (`testdata/`, paths replaced)
//! replayed by a scripted peer on a `FakeEnv` child.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use super::files::{self, ClaudePaths};
use super::protocol::{Expectations, StartFailure};
use super::*;
use crate::adapters::driven::harness_common::ClaudeConfig;
use crate::adapters::driven::harness_common::testkit::{Peer, config, fixture_lines, summary, within};
use crate::ports::executor::{Area, EnvPath, Exit, FileAccess, RelPath};
use crate::ports::harness::{
    BuiltinToolPolicy, ExitReason, HarnessEvent, HarnessExit, KillCause, McpServerName, McpServerSpec, Secret, ToolRef,
};
use crate::testing::executor::{FakeEnv, FakeSignal};
use crate::testing::harness::sample_spec;
use crate::testing::harness_contract::{EventLog, check_event_order};

const GATEWAY_TOKEN: &str = "mgg_gateway.token-0123456789";
const RUN_BEARER: &str = "run_bearer_abcdefghijklmnop";

fn spec() -> HarnessSpec {
    let mut spec = sample_spec();
    spec.mcp_servers = vec![McpServerSpec {
        name: McpServerName::run(),
        url: "http://127.0.0.1:47812/mcp".into(),
        bearer: Secret::new(RUN_BEARER),
        tool_timeout: Duration::from_secs(1800),
        expected_tools: vec!["echo".into(), "slow".into(), "submit_test".into()],
    }];
    spec.credentials = CredentialsMode::Gateway { base_url: "http://127.0.0.1:47811".into(), token: Secret::new(GATEWAY_TOKEN) };
    spec
}

/// The `system/init` Claude Code 2.1.269 sends for [`spec`] (recorded, paths replaced).
fn init() -> Value {
    json!({
        "type": "system", "subtype": "init", "cwd": "/fake/scratch/launch", "session_id": "s-1",
        "tools": ["Bash", "Edit", "Glob", "Grep", "Read", "Write", "mcp__mage_run__echo", "mcp__mage_run__slow", "mcp__mage_run__submit_test"],
        "mcp_servers": [{"name": "mage_run", "status": "connected"}],
        "model": "claude-sonnet-4-5", "permissionMode": "default", "slash_commands": [], "apiKeySource": "apiKeyHelper",
        "claude_code_version": "2.1.269", "output_style": "default", "agents": [], "skills": [], "plugins": [],
        "capabilities": ["interrupt_receipt_v1", "interrupt_cancel_queued_v1", "msg_lifecycle_v1"],
    })
}

/// Reads the first user message, then plays `fixture` (directives: `__await user`,
/// `__await interrupt`, `__respond interrupt`).
async fn claude_peer(mut peer: Peer, fixture: Vec<Value>) -> Peer {
    let Some(first) = peer.recv().await else { return peer };
    assert_eq!(first["type"], "user");
    assert_eq!(first["client_composed"], true);
    let mut interrupt_id = String::new();
    for line in fixture {
        match line.get("__await").and_then(Value::as_str) {
            Some("user") => {
                peer.recv_until(|v| v["type"] == "user").await;
                continue;
            }
            Some("interrupt") => {
                let req = peer.recv_until(|v| v["type"] == "control_request").await;
                assert_eq!(req["request"]["subtype"], "interrupt");
                interrupt_id = req["request_id"].as_str().unwrap_or_default().to_owned();
                continue;
            }
            _ => {}
        }
        if line.get("__respond").is_some() {
            peer.send_json(&json!({"type": "control_response", "response": {"subtype": "success", "request_id": interrupt_id, "response": {"still_queued": []}}}))
                .await;
            continue;
        }
        peer.send_json(&line).await;
    }
    peer
}

struct Run {
    session: Box<dyn HarnessSession>,
    log: EventLog,
    peer: tokio::task::JoinHandle<Peer>,
    env: Arc<FakeEnv>,
    _dir: tempfile::TempDir,
}

async fn start_run(fixture: Vec<Value>, spec: HarnessSpec, tweak: impl FnOnce(&mut HarnessConfig)) -> Run {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut cfg = config(dir.path());
    tweak(&mut cfg);
    let env = FakeEnv::empty();
    let peer = tokio::spawn(claude_peer(Peer::new(env.push_child()), fixture));
    let session = ClaudeHarness::new(cfg, IsolationMode::Dev).start(env.clone(), spec).await.expect("start");
    let log = EventLog::start(session.events().expect("events"));
    Run { session, log, peer, env, _dir: dir }
}

impl Run {
    async fn close(mut self) -> (Vec<HarnessEvent>, HarnessExit, Arc<FakeEnv>, tempfile::TempDir) {
        let peer = within("peer script", self.peer).await.expect("peer");
        let exiter = tokio::spawn(peer.exit_on_eof(Exit::Code(0)));
        self.session.close(Duration::from_secs(10)).await.expect("close");
        within("peer exit", exiter).await.expect("peer exit");
        self.log.end().await;
        let exit = within("session exit", self.session.wait()).await;
        (self.log.seen, exit, self.env, self._dir)
    }

    async fn idle(&mut self) {
        self.log.until("idle", |e| matches!(e, HarnessEvent::Idle)).await;
    }
}

fn summaries(events: &[HarnessEvent]) -> Vec<String> {
    events.iter().map(summary).collect()
}

fn with_init(rest: &[Value]) -> Vec<Value> {
    std::iter::once(init()).chain(rest.iter().cloned()).collect()
}

const READY: &str = "ready claude-sonnet-4-5 tools=9";
const MSG_USAGE: &str = "usage in=11 out=1 cr=0 cw=0 cost=None calls=1";

#[tokio::test]
async fn golden_tool_round_trip() {
    let mut run = start_run(fixture_lines(include_str!("testdata/tool_round_trip.jsonl")), spec(), |_| {}).await;
    run.idle().await;
    let (events, exit, ..) = run.close().await;
    assert_eq!(
        summaries(&events),
        vec![
            READY,
            "turn_started 1",
            MSG_USAGE,
            "call 1 run.echo {\"text\":\"hi\"}",
            "result 1 run.echo ok",
            "text 1 echoed",
            MSG_USAGE,
            "usage in=0 out=0 cr=0 cw=0 cost=Some(246) calls=0",
            "turn_ended 1 completed",
            "idle",
        ]
    );
    check_event_order(&events).unwrap();
    assert_eq!(exit.reason, ExitReason::Closed);
    assert_eq!(exit.usage.model_calls, 2);
    assert_eq!(exit.usage.cost_micros, Some(246));
}

#[tokio::test]
async fn golden_steer_mid_turn() {
    let mut run = start_run(fixture_lines(include_str!("testdata/steer.jsonl")), spec(), |_| {}).await;
    run.log.until("tool call", |e| matches!(e, HarnessEvent::ToolCall { .. })).await;
    run.session.steer("marker-1".into()).await.expect("steer");
    run.idle().await;
    let (events, ..) = run.close().await;
    assert_eq!(
        summaries(&events),
        vec![
            READY,
            "turn_started 1",
            MSG_USAGE,
            "call 1 run.slow {\"ms\":3000}",
            "result 1 run.slow ok",
            "text 1 after steer",
            MSG_USAGE,
            "usage in=0 out=0 cr=0 cw=0 cost=Some(246) calls=0",
            "turn_ended 1 completed",
            "idle",
        ]
    );
    check_event_order(&events).unwrap();
}

#[tokio::test]
async fn golden_abort_then_idle_steer() {
    let mut run = start_run(fixture_lines(include_str!("testdata/abort.jsonl")), spec(), |_| {}).await;
    run.log.until("tool call", |e| matches!(e, HarnessEvent::ToolCall { .. })).await;
    run.session.abort().await.expect("abort");
    run.idle().await;
    run.session.steer("marker-2".into()).await.expect("steer");
    run.log.until("turn 2 end", |e| matches!(e, HarnessEvent::TurnEnded { turn: 2, .. })).await;
    run.idle().await;
    let (events, ..) = run.close().await;
    assert_eq!(
        summaries(&events),
        vec![
            READY,
            "turn_started 1",
            MSG_USAGE,
            "call 1 run.slow {\"ms\":30000}",
            "result 1 run.slow err",
            "usage in=0 out=0 cr=0 cw=0 cost=Some(138) calls=0",
            "turn_ended 1 aborted",
            "idle",
            "turn_started 2",
            "text 2 after idle steer",
            MSG_USAGE,
            "usage in=0 out=0 cr=0 cw=0 cost=Some(108) calls=0",
            "turn_ended 2 completed",
            "idle",
        ]
    );
    check_event_order(&events).unwrap();
}

fn text_turn(id: &str, text: &str, cost: f64, result: Value) -> Vec<Value> {
    let mut result = result;
    result["total_cost_usd"] = json!(cost);
    vec![
        json!({"type": "stream_event", "event": {"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": text}}}),
        json!({"type": "assistant", "message": {"id": id, "content": [{"type": "text", "text": text}], "usage": {"input_tokens": 5, "output_tokens": 2, "cache_read_input_tokens": 3, "cache_creation_input_tokens": 1}}}),
        // The same message id again (one assistant message per content block): counted once.
        json!({"type": "assistant", "message": {"id": id, "content": [], "usage": {"input_tokens": 5, "output_tokens": 2}}}),
        result,
    ]
}

#[tokio::test]
async fn failures_notices_and_output_after_result() {
    let success = json!({"type": "result", "subtype": "success", "is_error": false});
    let mut fixture = vec![
        json!({"type": "system", "subtype": "compact_boundary"}),
        json!({"type": "system", "subtype": "api_retry", "attempt": 1, "max_retries": 10, "retry_delay_ms": 500, "error_status": 529, "error": "overloaded"}),
        json!({"type": "brand_new_message_type", "x": 1}),
        json!({"type": "control_request", "request_id": "cr-1", "request": {"subtype": "can_use_tool", "tool_name": "WebFetch"}}),
    ];
    fixture.extend(text_turn("m1", "one", 0.001, json!({"type": "result", "subtype": "error_during_execution", "is_error": true, "api_error_status": 529, "errors": ["overloaded"]})));
    // Output right after a result opens the next turn (a message queued as the turn ended).
    fixture.extend(text_turn("m2", "two", 0.0015, success.clone()));
    fixture.push(json!({"__await": "user"}));
    fixture.extend(text_turn("m3", "three", 0.0015, json!({"type": "result", "subtype": "error_max_budget_usd", "is_error": true})));
    let mut run = start_run(with_init(&fixture), spec(), |_| {}).await;
    run.log.until("turn 2", |e| matches!(e, HarnessEvent::TurnEnded { turn: 2, .. })).await;
    run.idle().await;
    run.session.steer("go on".into()).await.unwrap();
    run.log.until("turn 3", |e| matches!(e, HarnessEvent::TurnEnded { turn: 3, .. })).await;
    run.idle().await;
    // The deny went out before the steer's user message, so `__await user` already read it.
    let peer = within("peer script", run.peer).await.expect("peer");
    let reply = peer.received.iter().find(|v| v["type"] == "control_response").expect("control_response for cr-1");
    assert_eq!(reply["response"]["request_id"], "cr-1");
    assert_eq!(reply["response"]["response"]["behavior"], "deny");
    run.peer = tokio::spawn(async move { peer });
    let (events, ..) = run.close().await;
    let usage = "usage in=5 out=2 cr=3 cw=1 cost=None calls=1";
    assert_eq!(
        summaries(&events),
        vec![
            READY,
            "turn_started 1",
            "notice info context compacted",
            "notice warning model request retry 1/10 in 500 ms: 529 overloaded",
            "notice warning harness control request can_use_tool denied",
            "text 1 one",
            usage,
            "usage in=0 out=0 cr=0 cw=0 cost=Some(1000) calls=0",
            "turn_ended 1 failed overloaded retryable=true status=Some(529)",
            "turn_started 2",
            "text 2 two",
            usage,
            "usage in=0 out=0 cr=0 cw=0 cost=Some(500) calls=0",
            "turn_ended 2 completed",
            "idle",
            "turn_started 3",
            "text 3 three",
            usage,
            "turn_ended 3 failed budget retryable=false status=None",
            "idle",
        ]
    );
    check_event_order(&events).unwrap();
}

#[tokio::test]
async fn interrupt_carries_cancel_queued_when_advertised() {
    let fixture = with_init(&[json!({"type": "assistant", "message": {"id": "m1", "content": [], "usage": {}}})]);
    let mut run = start_run(fixture, spec(), |c| c.abort_deadline_secs = 1).await;
    run.log.until("usage", |e| matches!(e, HarnessEvent::Usage(_))).await;
    // Nobody answers the interrupt: SIGINT, then terminate.
    let err = run.session.abort().await.unwrap_err();
    assert!(matches!(err, HarnessError::Unavailable(_)), "{err:?}");
    run.log.end().await;
    let exit = within("session exit", run.session.wait()).await;
    assert_eq!(exit.reason, ExitReason::Killed(KillCause::AbortUnanswered));
    let mut peer = within("peer script", run.peer).await.expect("peer");
    let signals = peer.signals();
    assert_eq!(signals.first(), Some(&FakeSignal::Interrupt), "{signals:?}");
    assert!(signals.contains(&FakeSignal::Terminate));
    // The peer never answered: the request it got carries `cancel_queued` (advertised in init).
    let req = peer.recv_until(|v| v["type"] == "control_request").await;
    assert_eq!(req["request"], json!({"subtype": "interrupt", "cancel_queued": true}));
}

async fn start_err(fixture: Vec<Value>, spec: HarnessSpec) -> (HarnessError, Vec<FakeSignal>) {
    let dir = tempfile::tempdir().unwrap();
    let env = FakeEnv::empty();
    let peer = Peer::new(env.push_child());
    let ctl = peer.control();
    let task = tokio::spawn(claude_peer(peer, fixture));
    let err = match ClaudeHarness::new(config(dir.path()), IsolationMode::Dev).start(env, spec).await {
        Err(e) => e,
        Ok(_) => panic!("start succeeded"),
    };
    task.abort();
    (err, ctl.signals())
}

#[tokio::test]
async fn init_violations_fail_closed_and_kill() {
    let cases: Vec<(&str, Box<dyn Fn(&mut Value)>)> = vec![
        ("extra MCP server", Box::new(|i| i["mcp_servers"].as_array_mut().unwrap().push(json!({"name": "repo_server", "status": "connected"})))),
        ("extra MCP tool", Box::new(|i| i["tools"].as_array_mut().unwrap().push(json!("mcp__mage_run__exfil")))),
        ("extra built-in", Box::new(|i| i["tools"].as_array_mut().unwrap().push(json!("WebFetch")))),
        ("plugin", Box::new(|i| i["plugins"] = json!([{"name": "p"}]))),
        ("skill", Box::new(|i| i["skills"] = json!(["deploy"]))),
        ("agent", Box::new(|i| i["agents"] = json!(["repo-agent"]))),
        ("cwd", Box::new(|i| i["cwd"] = json!("/fake/ws"))),
        ("permission mode", Box::new(|i| i["permissionMode"] = json!("bypassPermissions"))),
        ("model", Box::new(|i| i["model"] = json!("claude-opus-4-1"))),
        ("api key source", Box::new(|i| i["apiKeySource"] = json!("ANTHROPIC_API_KEY"))),
    ];
    for (name, mutate) in cases {
        let mut bad = init();
        mutate(&mut bad);
        let (err, signals) = start_err(vec![bad], spec()).await;
        assert!(matches!(err, HarnessError::IsolationViolation(_)), "{name}: {err:?}");
        assert!(signals.contains(&FakeSignal::Terminate), "{name}: process not terminated");
    }
}

#[tokio::test]
async fn mcp_not_connected_and_startup_failure_are_unavailable() {
    let mut pending = init();
    pending["mcp_servers"] = json!([{"name": "mage_run", "status": "failed"}]);
    let (err, _) = start_err(vec![pending], spec()).await;
    assert!(matches!(&err, HarnessError::Unavailable(m) if m.contains("failed")), "{err:?}");
    let mut missing = init();
    missing["tools"] = json!(["Bash", "mcp__mage_run__echo"]);
    let (err, _) = start_err(vec![missing], spec()).await;
    assert!(matches!(&err, HarnessError::Unavailable(m) if m.contains("mcp__mage_run__slow")), "{err:?}");
    let startup = json!({"type": "result", "subtype": "error_during_execution", "is_error": true, "startup_failure_reason": "invalid model"});
    let (err, _) = start_err(vec![startup], spec()).await;
    assert!(matches!(&err, HarnessError::Unavailable(m) if m == "invalid model"), "{err:?}");
}

#[tokio::test]
async fn later_init_is_rechecked() {
    let mut evil = init();
    evil["mcp_servers"].as_array_mut().unwrap().push(json!({"name": "late", "status": "connected"}));
    let fixture = with_init(&[json!({"type": "result", "subtype": "success", "is_error": false, "total_cost_usd": 0.0}), json!({"__await": "user"}), evil]);
    let mut run = start_run(fixture, spec(), |_| {}).await;
    run.idle().await;
    run.session.steer("next".into()).await.unwrap();
    run.log.end().await;
    let exit = within("session exit", run.session.wait()).await;
    assert!(matches!(&exit.reason, ExitReason::IsolationViolation(m) if m.contains("late")), "{exit:?}");
}

#[tokio::test]
async fn session_files_argv_env_and_transcript() {
    let mut run = start_run(with_init(&[json!({"type": "result", "subtype": "success", "is_error": false, "total_cost_usd": 0.0})]), spec(), |_| {}).await;
    run.idle().await;
    let env = run.env.clone();
    let spawn = env.spawns()[0].clone();
    assert_eq!(spawn.cwd, EnvPath::scratch("launch").unwrap());
    let argv = &spawn.argv;
    let s = "/fake/scratch/harness";
    let value = |flag: &str| argv[argv.iter().position(|a| a == flag).unwrap_or_else(|| panic!("{flag} missing")) + 1].clone();
    assert_eq!(&argv[1..8], ["-p", "--input-format", "stream-json", "--output-format", "stream-json", "--verbose", "--include-partial-messages"]);
    assert_eq!(value("--model"), "claude-sonnet-4-5");
    assert_eq!(value("--mcp-config"), format!("{s}/claude/mcp.json"));
    assert!(argv.contains(&"--strict-mcp-config".to_owned()));
    assert_eq!(value("--settings"), format!("{s}/claude/settings.json"));
    assert_eq!(value("--setting-sources"), "user");
    assert_eq!(value("--permission-mode"), "dontAsk");
    assert_eq!(value("--tools"), "Read,Grep,Glob,Edit,Write,Bash,TodoWrite");
    assert_eq!(value("--allowedTools"), "Read Grep Glob Edit Write Bash TodoWrite mcp__mage_run");
    assert_eq!(value("--disallowedTools"), "WebFetch,WebSearch,Agent");
    assert!(argv.contains(&"--disable-slash-commands".to_owned()));
    assert!(value("--append-system-prompt").starts_with("Workspace: /fake/ws (read/write)."));
    assert_eq!(value("--add-dir"), "/fake/ws");
    assert_eq!(value("--max-budget-usd"), "5.000000");
    let session_id = value("--session-id");
    assert_eq!(session_id.len(), 36);
    let vars: std::collections::BTreeMap<&str, &str> = spawn.env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    assert_eq!(vars["CLAUDE_CONFIG_DIR"], format!("{s}/claude/config"));
    assert_eq!(vars["ANTHROPIC_BASE_URL"], "http://127.0.0.1:47811");
    for flag in ["CLAUDE_CODE_SUBPROCESS_ENV_SCRUB", "CLAUDE_CODE_DISABLE_CLAUDE_MDS", "CLAUDE_CODE_DISABLE_BACKGROUND_TASKS", "CLAUDE_AGENT_SDK_DISABLE_BUILTIN_AGENTS"] {
        assert_eq!(vars[flag], "1", "{flag}");
    }
    assert_eq!(vars["MCP_TOOL_TIMEOUT"], "1800000");
    assert!(!vars.contains_key("ENABLE_TOOL_SEARCH") && !vars.contains_key("ANTHROPIC_API_KEY"));
    let joined = format!("{:?}{:?}", spawn.argv, spawn.env);
    assert!(!joined.contains(GATEWAY_TOKEN) && !joined.contains(RUN_BEARER), "secret in argv/env");

    let access: std::collections::BTreeMap<String, FileAccess> =
        env.put_files().into_iter().chain(env.made_dirs()).map(|(p, a)| (p.rel.as_str().to_owned(), a)).collect();
    assert_eq!(access["harness/claude/config"], FileAccess::AgentCreateOnly);
    for f in ["harness", "harness/claude", "harness/claude/config/settings.json", "harness/claude/settings.json", "harness/claude/mcp.json", "harness/claude/gateway-token", "harness/claude/system-append.md"] {
        assert_eq!(access[f], FileAccess::AgentReadOnly, "{f}");
    }
    let file = |p: &str| String::from_utf8(env.file(&EnvPath::scratch(p).unwrap()).expect(p)).unwrap();
    assert_eq!(file("harness/claude/gateway-token"), GATEWAY_TOKEN);
    let settings: Value = serde_json::from_str(&file("harness/claude/settings.json")).unwrap();
    assert_eq!(
        settings,
        json!({"disableAllHooks": true, "apiKeyHelper": format!("cat '{s}/claude/gateway-token'"),
               "permissions": {"defaultMode": "dontAsk", "deny": ["WebFetch", "WebSearch", "Agent"]}})
    );
    assert_eq!(file("harness/claude/config/settings.json"), "{\"disableAllHooks\": true}\n");
    let mcp: Value = serde_json::from_str(&file("harness/claude/mcp.json")).unwrap();
    assert_eq!(mcp["mcpServers"]["mage_run"]["headers"]["Authorization"], format!("Bearer {RUN_BEARER}"));

    let transcript = EnvPath::new(Area::Scratch, RelPath::new(&format!("harness/claude/config/projects/-fake-scratch-launch/{session_id}.jsonl")).unwrap());
    env.set_file(&transcript, format!("{{\"k\":\"{GATEWAY_TOKEN}\"}}\n").as_bytes());
    let (_, exit, _env, _dir) = run.close().await;
    let copy = std::fs::read_to_string(exit.transcript.expect("transcript copy")).unwrap();
    assert_eq!(copy, "{\"k\":\"[redacted]\"}\n");
}

#[test]
fn argv_and_env_per_mode_and_policy() {
    let paths = ClaudePaths { session: "/s/h".into(), workspace: None };
    let mut spec = spec();
    spec.builtin_tools = BuiltinToolPolicy::None;
    spec.cwd = None;
    spec.budget.max_cost_micros = None;
    spec.credentials = CredentialsMode::LocalLogin;
    let argv = files::argv("claude", &spec, &paths, "sid", "");
    let tools = argv.iter().position(|a| a == "--tools").unwrap();
    assert_eq!(argv[tools + 1], "");
    let allowed = argv.iter().position(|a| a == "--allowedTools").unwrap();
    assert_eq!(argv[allowed + 1], "mcp__mage_run");
    assert!(!argv.iter().any(|a| a == "--add-dir" || a == "--max-budget-usd" || a == "--append-system-prompt"));
    let cfg = ClaudeConfig { dev_config_dir: "/dev/claude".into(), ..ClaudeConfig::default() };
    let vars = files::env_vars(&spec, &cfg, &paths);
    assert!(vars.contains(&("CLAUDE_CONFIG_DIR".into(), "/dev/claude".into())));
    assert!(vars.contains(&("ENABLE_TOOL_SEARCH".into(), "false".into())));
    assert!(!vars.iter().any(|(k, _)| k == "ANTHROPIC_BASE_URL"));
    let settings: Value = serde_json::from_slice(&files::settings_json(&spec, &paths)).unwrap();
    assert!(settings.get("apiKeyHelper").is_none());
    spec.credentials = CredentialsMode::Vertex { project_id: "proj".into(), region: "us-east5".into() };
    let vars = files::env_vars(&spec, &cfg, &paths);
    for want in [("CLAUDE_CODE_USE_VERTEX", "1"), ("ANTHROPIC_VERTEX_PROJECT_ID", "proj"), ("CLOUD_ML_REGION", "us-east5"), ("CLAUDE_CONFIG_DIR", "/s/h/claude/config")] {
        assert!(vars.contains(&(want.0.into(), want.1.into())), "{want:?}");
    }
    spec.builtin_tools = BuiltinToolPolicy::ReadOnly;
    assert_eq!(files::allowed_tools(&spec), "Read Grep Glob mcp__mage_run");
    assert_eq!(files::project_dir_name("/mage/scratch/launch"), "-mage-scratch-launch");
    assert_eq!(files::project_dir_name("/a/b.c d"), "-a-b-c-d");
}

#[tokio::test]
async fn vertex_requires_opt_in() {
    let dir = tempfile::tempdir().unwrap();
    let mut spec = spec();
    spec.credentials = CredentialsMode::Vertex { project_id: "proj".into(), region: "us-east5".into() };
    let harness = ClaudeHarness::new(config(dir.path()), IsolationMode::Dev);
    assert!(matches!(harness.start(FakeEnv::empty(), spec.clone()).await, Err(HarnessError::Unsupported(_))));
    let info = harness.probe().await.unwrap();
    assert_eq!(info.version, "2.1.269");
    assert!(!info.credential_modes.contains(&CredentialModeKind::Vertex));
    let mut cfg = config(dir.path());
    cfg.allow_direct_vertex = true;
    let enforced = ClaudeHarness::new(cfg, IsolationMode::Enforced).probe().await.unwrap();
    assert_eq!(enforced.credential_modes, vec![CredentialModeKind::Gateway, CredentialModeKind::Vertex]);
}

#[test]
fn expectation_normalizes_names() {
    let e = Expectations::new(&spec().mcp_servers, BuiltinToolPolicy::WorkspaceRw, "/x".into(), "m".into(), true, "2.1.269".into());
    assert_eq!(e.normalize("mcp__mage_run__echo"), ToolRef::Mcp { server: McpServerName::run(), tool: "echo".into() });
    assert_eq!(e.normalize("mcp__mage_run__other_tool"), ToolRef::Mcp { server: McpServerName::run(), tool: "other_tool".into() });
    assert_eq!(e.normalize("Bash"), ToolRef::Builtin("Bash".into()));
    assert!(matches!(e.check(&json!({"tools": [], "mcp_servers": []})), Err(StartFailure::Unavailable(_))));
}
