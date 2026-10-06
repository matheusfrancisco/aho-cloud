//! omp adapter tests: config writers, tool naming, inventory, and golden transcripts recorded
//! from omp 18.3.1 (`testdata/`, secrets and paths replaced) replayed by a scripted peer on a
//! `FakeEnv` child.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use super::files::{self, OmpPaths};
use super::protocol::{ToolNames, mint_mcp_tool_name};
use super::*;
use crate::adapters::driven::harness_common::testkit::{Peer, config, fixture_lines, summary, within};
use crate::adapters::driven::harness_common::{ModelMeta, OmpConfig};
use crate::ports::executor::{Area, EnvPath, Exit, FileAccess, RelPath};
use crate::ports::harness::{
    BuiltinToolPolicy, CredentialsMode, ExitReason, HarnessEvent, HarnessExit, KillCause, McpServerName, McpServerSpec, NoticeLevel, Secret, ToolRef,
    TurnOutcome,
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

const WORKSPACE_TOOLS: &[&str] = &["read", "grep", "glob", "edit", "write", "bash", "todo"];
const MCP_TOOLS: &[&str] = &["mcp__mage_run_echo", "mcp__mage_run_slow", "mcp__mage_run_submit_test"];

struct PeerOpts {
    ready: Value,
    /// `dumpTools` per `get_state` call (the last entry repeats).
    dump_tools: Vec<Vec<&'static str>>,
    provider: &'static str,
    model: &'static str,
}

impl Default for PeerOpts {
    fn default() -> Self {
        Self {
            ready: json!({"type": "ready", "protocolVersion": 1, "supportedProtocolVersions": [1, 2], "maxFrameBytes": 1_048_576, "maxReassembledFrameBytes": 67_108_864}),
            dump_tools: vec![WORKSPACE_TOOLS.iter().chain(MCP_TOOLS).copied().collect()],
            provider: "mage",
            model: "claude-sonnet-4-5",
        }
    }
}

const SESSION_FILE: &str = "/fake/scratch/harness/omp/sessions/2026-10-05_s1.jsonl";

/// Answers the handshake like omp 18.3.1, then plays `fixture` (see the directive format in
/// `testdata/`). Returns the peer for the test to finish (close / crash).
async fn omp_peer(mut peer: Peer, opts: PeerOpts, fixture: Vec<Value>) -> Peer {
    peer.send_json(&opts.ready).await;
    let mut prompts: Vec<String> = Vec::new();
    let mut get_state = 0;
    loop {
        let Some(cmd) = peer.recv().await else { return peer };
        let id = cmd["id"].as_str().unwrap_or_default().to_owned();
        let ty = cmd["type"].as_str().unwrap_or_default().to_owned();
        let resp = |data: Value| json!({"id": id, "type": "response", "command": ty, "success": true, "data": data});
        match ty.as_str() {
            "negotiate_protocol" => peer.send_json(&resp(json!({"protocolVersion": 2}))).await,
            "set_event_filter" => peer.send_json(&resp(json!({"events": cmd["events"]}))).await,
            "set_steering_mode" => peer.send_json(&json!({"id": id, "type": "response", "command": ty, "success": true})).await,
            "get_state" => {
                let tools = &opts.dump_tools[get_state.min(opts.dump_tools.len() - 1)];
                get_state += 1;
                let dump: Vec<Value> = tools.iter().map(|n| json!({"name": n, "description": "", "parameters": {}})).collect();
                let data = json!({
                    "model": {"id": opts.model, "provider": opts.provider},
                    "sessionFile": SESSION_FILE, "sessionId": "01a10d96-s1", "dumpTools": dump,
                });
                peer.send_json(&resp(data)).await;
            }
            "prompt" => {
                prompts.push(id.clone());
                peer.send_json(&json!({"id": id, "type": "response", "command": "prompt", "success": true})).await;
                break;
            }
            other => panic!("unexpected handshake command {other}"),
        }
    }
    let mut abort_id = String::new();
    for mut line in fixture {
        match line.get("__await").and_then(Value::as_str) {
            Some("prompt") => {
                let cmd = peer.recv_until(|v| v["type"] == "prompt").await;
                let id = cmd["id"].as_str().unwrap_or_default().to_owned();
                assert_eq!(cmd["streamingBehavior"], "steer");
                prompts.push(id.clone());
                peer.send_json(&json!({"id": id, "type": "response", "command": "prompt", "success": true})).await;
                continue;
            }
            Some("abort") => {
                abort_id = peer.recv_until(|v| v["type"] == "abort").await["id"].as_str().unwrap_or_default().to_owned();
                continue;
            }
            _ => {}
        }
        if line.get("__respond").is_some() {
            peer.send_json(&json!({"id": abort_id, "type": "response", "command": "abort", "success": true})).await;
            continue;
        }
        if let Some(n) = line.get("id").and_then(Value::as_str).and_then(|s| s.strip_prefix("$p")).and_then(|n| n.parse::<usize>().ok()) {
            line["id"] = Value::String(prompts[n - 1].clone());
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
    cfg: HarnessConfig,
}

async fn start_run(opts: PeerOpts, fixture: &str, spec: HarnessSpec, tweak: impl FnOnce(&mut HarnessConfig)) -> Run {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut cfg = config(dir.path());
    tweak(&mut cfg);
    let env = FakeEnv::empty();
    let peer = Peer::new(env.push_child());
    let peer = tokio::spawn(omp_peer(peer, opts, fixture_lines(fixture)));
    let harness = OmpHarness::new(cfg.clone(), ModelTable::new(), IsolationMode::Dev);
    let session = harness.start(env.clone(), spec).await.expect("start");
    let log = EventLog::start(session.events().expect("events"));
    Run { session, log, peer, env, _dir: dir, cfg }
}

impl Run {
    /// Closes, lets the peer exit 0 on stdin EOF, drains the stream.
    async fn close(mut self) -> (Vec<HarnessEvent>, HarnessExit, Arc<FakeEnv>, HarnessConfig, tempfile::TempDir) {
        let peer = within("peer script", self.peer).await.expect("peer");
        let exiter = tokio::spawn(peer.exit_on_eof(Exit::Code(0)));
        self.session.close(Duration::from_secs(10)).await.expect("close");
        within("peer exit", exiter).await.expect("peer exit");
        self.log.end().await;
        let exit = within("session exit", self.session.wait()).await;
        (self.log.seen, exit, self.env, self.cfg, self._dir)
    }
}

fn summaries(events: &[HarnessEvent]) -> Vec<String> {
    events.iter().map(summary).collect()
}

const READY: &str = "ready claude-sonnet-4-5 tools=10";
const CALL_USAGE: &str = "usage in=11 out=7 cr=0 cw=0 cost=Some(138) calls=1";
const TEXT_USAGE: &str = "usage in=11 out=5 cr=0 cw=0 cost=Some(108) calls=1";

#[tokio::test]
async fn golden_tool_round_trip() {
    let mut run = start_run(PeerOpts::default(), include_str!("testdata/tool_round_trip.jsonl"), spec(), |_| {}).await;
    run.log.until("idle", |e| matches!(e, HarnessEvent::Idle)).await;
    let (events, exit, env, cfg, _dir) = run.close().await;
    assert_eq!(
        summaries(&events),
        vec![
            READY,
            "turn_started 1",
            CALL_USAGE,
            "call 1 read {\"path\":\"/fake/ws/file.txt\"}",
            "result 1 read ok",
            "text 1 the file says hello",
            TEXT_USAGE,
            "turn_ended 1 completed",
            "idle",
        ]
    );
    check_event_order(&events).unwrap();
    assert_eq!(exit.reason, ExitReason::Closed);
    assert_eq!(exit.exit_code, Some(0));
    assert_eq!(exit.usage.model_calls, 2);
    assert_eq!(exit.usage.cost_micros, Some(246));
    // No transcript file in the env: the exit says so instead of failing.
    assert_eq!(exit.transcript, None);
    assert!(exit.stderr_log.as_deref().is_some_and(|p| p.starts_with(&cfg.log_dir)));
    assert_eq!(env.spawns().len(), 1);
}

#[tokio::test]
async fn golden_mcp_tool_is_normalized() {
    let mut run = start_run(PeerOpts::default(), include_str!("testdata/mcp_tool.jsonl"), spec(), |_| {}).await;
    run.log.until("idle", |e| matches!(e, HarnessEvent::Idle)).await;
    let (events, ..) = run.close().await;
    let s = summaries(&events);
    assert!(s.contains(&"call 1 run.echo {\"text\":\"hi\"}".to_owned()), "{s:#?}");
    assert!(s.contains(&"result 1 run.echo ok".to_owned()), "{s:#?}");
    match events.iter().find(|e| matches!(e, HarnessEvent::ToolCall { .. })) {
        Some(HarnessEvent::ToolCall { tool, .. }) => assert_eq!(tool, &ToolRef::Mcp { server: McpServerName::run(), tool: "echo".into() }),
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn golden_steer_mid_turn_folds_into_the_turn() {
    let mut run = start_run(PeerOpts::default(), include_str!("testdata/steer.jsonl"), spec(), |_| {}).await;
    run.log.until("tool call", |e| matches!(e, HarnessEvent::ToolCall { .. })).await;
    run.session.steer("marker-1".into()).await.expect("steer");
    run.log.until("idle", |e| matches!(e, HarnessEvent::Idle)).await;
    let (events, exit, ..) = run.close().await;
    assert_eq!(
        summaries(&events),
        vec![
            READY,
            "turn_started 1",
            CALL_USAGE,
            "call 1 bash {\"command\":\"sleep 3; echo slept\"}",
            "result 1 bash ok",
            "text 1 after steer",
            TEXT_USAGE,
            "turn_ended 1 completed",
            "idle",
        ]
    );
    check_event_order(&events).unwrap();
    assert_eq!(exit.reason, ExitReason::Closed);
}

#[tokio::test]
async fn golden_abort_then_idle_steer() {
    let mut run = start_run(PeerOpts::default(), include_str!("testdata/abort.jsonl"), spec(), |_| {}).await;
    run.log.until("tool call", |e| matches!(e, HarnessEvent::ToolCall { .. })).await;
    run.session.abort().await.expect("abort");
    run.log.until("idle", |e| matches!(e, HarnessEvent::Idle)).await;
    run.session.steer("marker-2".into()).await.expect("steer");
    run.log.until("turn 2 end", |e| matches!(e, HarnessEvent::TurnEnded { turn: 2, .. })).await;
    run.log.until("idle", |e| matches!(e, HarnessEvent::Idle)).await;
    let (events, ..) = run.close().await;
    assert_eq!(
        summaries(&events),
        vec![
            READY,
            "turn_started 1",
            CALL_USAGE,
            "call 1 bash {\"command\":\"sleep 30; echo slept\"}",
            "result 1 bash err",
            "usage in=0 out=0 cr=0 cw=0 cost=Some(0) calls=1",
            "turn_ended 1 aborted",
            "idle",
            "turn_started 2",
            "text 2 after idle steer",
            TEXT_USAGE,
            "turn_ended 2 completed",
            "idle",
        ]
    );
    check_event_order(&events).unwrap();
}

#[tokio::test]
async fn golden_provider_error() {
    let mut run = start_run(PeerOpts::default(), include_str!("testdata/error.jsonl"), spec(), |_| {}).await;
    run.log.until("idle", |e| matches!(e, HarnessEvent::Idle)).await;
    let (events, ..) = run.close().await;
    let err = r#"401 {"type":"error","error":{"type":"authentication_error","message":"scripted failure"}}"#;
    assert_eq!(
        summaries(&events),
        vec![
            READY.to_owned(),
            "turn_started 1".to_owned(),
            "usage in=0 out=0 cr=0 cw=0 cost=Some(0) calls=1".to_owned(),
            format!("notice error {err}"),
            format!("turn_ended 1 failed {err} retryable=false status=Some(401)"),
            "idle".to_owned(),
        ]
    );
}

#[tokio::test]
async fn compaction_retry_unknown_frames_and_host_requests() {
    let fixture = [
        r#"{"type":"agent_start"}"#,
        r#"{"type":"advisor_cost_changed"}"#,
        r#"{"type":"available_commands_update","commands":[]}"#,
        r#"{"type":"auto_compaction_start","reason":"threshold","action":"context-full"}"#,
        r#"{"type":"auto_compaction_end","action":"context-full","aborted":false,"willRetry":false}"#,
        r#"{"type":"auto_retry_start","attempt":1,"maxAttempts":3,"delayMs":2000,"errorMessage":"529 overloaded"}"#,
        r#"{"type":"auto_retry_end","success":true,"attempt":2}"#,
        r#"{"type":"notice","level":"warning","message":"disk almost full","source":"session-persistence"}"#,
        r#"{"type":"extension_ui_request","id":"ui_7","method":"confirm","title":"Continue?"}"#,
        r#"{"type":"host_tool_call","id":"host_1","toolCallId":"t","toolName":"echo_host","arguments":{}}"#,
        r#"{"type":"message_update","assistantMessageEvent":{"type":"thinking_delta","delta":"hmm"}}"#,
        r#"{"type":"message_update","assistantMessageEvent":{"type":"text_delta","delta":"done"}}"#,
        r#"{"type":"prompt_result","id":"$p1","agentInvoked":true,"status":"completed","sessionSettled":true}"#,
        r#"{"type":"session_settled"}"#,
    ]
    .join("\n");
    let mut run = start_run(PeerOpts::default(), &fixture, spec(), |_| {}).await;
    run.log.until("idle", |e| matches!(e, HarnessEvent::Idle)).await;
    let peer = within("peer script", run.peer).await.expect("peer");
    // The adapter answered the UI and host-tool requests (cancel / error), in any order.
    let mut peer = peer;
    let mut replies = Vec::new();
    for _ in 0..2 {
        replies.push(peer.recv().await.expect("reply"));
    }
    replies.sort_by_key(|r| r["type"].as_str().unwrap_or_default().to_owned());
    assert_eq!(replies[0], json!({"type": "extension_ui_response", "id": "ui_7", "cancelled": true}));
    assert_eq!(replies[1]["type"], "host_tool_result");
    assert_eq!(replies[1]["isError"], true);
    run.peer = tokio::spawn(async move { peer });
    let (events, ..) = run.close().await;
    assert_eq!(
        summaries(&events),
        vec![
            READY,
            "turn_started 1",
            "notice info context compaction started (threshold)",
            "notice info context compaction finished",
            "notice warning model request retry 1/3 in 2000 ms: 529 overloaded",
            "notice warning model request succeeded after 2 attempts",
            "notice warning disk almost full",
            "notice warning harness UI request confirm cancelled (no UI)",
            "notice error unexpected host tool call echo_host",
            "text 1 done",
            "turn_ended 1 completed",
            "idle",
        ]
    );
}

#[tokio::test]
async fn chunked_frames_are_reassembled_end_to_end() {
    use base64::Engine as _;
    let big = "x".repeat(3000);
    let frame = json!({"type": "message_update", "assistantMessageEvent": {"type": "text_delta", "delta": big}}).to_string();
    let parts: Vec<&[u8]> = frame.as_bytes().chunks(1000).collect();
    let mut lines = vec![r#"{"type":"agent_start"}"#.to_owned()];
    for (i, p) in parts.iter().enumerate() {
        lines.push(
            json!({"type": "rpc_chunk", "chunkId": "rpc-1", "index": i, "count": parts.len(), "byteLength": frame.len(),
                   "data": base64::engine::general_purpose::STANDARD.encode(p)})
            .to_string(),
        );
    }
    lines.push(r#"{"type":"prompt_result","id":"$p1","agentInvoked":true,"status":"completed","sessionSettled":true}"#.into());
    let mut run = start_run(PeerOpts::default(), &lines.join("\n"), spec(), |_| {}).await;
    run.log.until("idle", |e| matches!(e, HarnessEvent::Idle)).await;
    let (events, ..) = run.close().await;
    let text: String = events
        .iter()
        .filter_map(|e| match e {
            HarnessEvent::AssistantText { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(text, big);
}

#[tokio::test]
async fn interleaved_chunk_is_a_protocol_error() {
    let fixture = [
        r#"{"type":"agent_start"}"#,
        r#"{"type":"rpc_chunk","chunkId":"rpc-1","index":0,"count":2,"byteLength":10,"data":"e30="}"#,
        r#"{"type":"session_settled"}"#,
    ]
    .join("\n");
    let mut run = start_run(PeerOpts::default(), &fixture, spec(), |_| {}).await;
    run.log.end().await;
    let exit = within("session exit", run.session.wait()).await;
    assert!(matches!(&exit.reason, ExitReason::ProtocolError(m) if m.contains("interrupted")), "{exit:?}");
    assert!(matches!(run.log.seen.last(), Some(HarnessEvent::Notice { level: NoticeLevel::Error, .. })));
    assert!(within("peer script", run.peer).await.expect("peer").signals().contains(&FakeSignal::Terminate));
}

#[tokio::test]
async fn oversized_line_is_a_protocol_error() {
    let mut opts = PeerOpts::default();
    opts.ready["maxFrameBytes"] = json!(200);
    let fixture = format!("{{\"type\":\"notice\",\"level\":\"info\",\"message\":\"{}\"}}", "y".repeat(2000));
    let mut run = start_run(opts, &fixture, spec(), |_| {}).await;
    run.log.end().await;
    let exit = within("session exit", run.session.wait()).await;
    assert!(matches!(&exit.reason, ExitReason::ProtocolError(m) if m.contains("longer than 1224")), "{exit:?}");
}

#[tokio::test]
async fn crash_ends_stream_with_notice_and_crashed() {
    let fixture = r#"{"type":"agent_start"}"#;
    let mut run = start_run(PeerOpts::default(), fixture, spec(), |_| {}).await;
    let mut peer = within("peer script", run.peer).await.expect("peer");
    peer.stderr(&format!("fatal: token {GATEWAY_TOKEN} rejected\n")).await;
    peer.crash(Exit::Code(1));
    run.log.end().await;
    let exit = within("session exit", run.session.wait()).await;
    assert_eq!(exit.reason, ExitReason::Crashed);
    assert_eq!(exit.exit_code, Some(1));
    match run.log.seen.last() {
        Some(HarnessEvent::Notice { level: NoticeLevel::Error, text }) => {
            assert!(text.starts_with("harness exited unexpectedly: exit 1"), "{text}");
            assert!(text.contains("token [redacted] rejected"), "{text}");
        }
        other => panic!("{other:?}"),
    }
    assert!(exit.stderr_tail.contains("[redacted]") && !exit.stderr_tail.contains(GATEWAY_TOKEN));
    let log = std::fs::read_to_string(exit.stderr_log.expect("stderr log")).expect("read log");
    assert!(!log.contains(GATEWAY_TOKEN));
    assert!(matches!(run.session.steer("x".into()).await, Err(HarnessError::Closed)));
}

#[tokio::test]
async fn transcript_copy_is_redacted() {
    let mut run = start_run(PeerOpts::default(), include_str!("testdata/tool_round_trip.jsonl"), spec(), |_| {}).await;
    run.log.until("idle", |e| matches!(e, HarnessEvent::Idle)).await;
    let path = EnvPath::new(Area::Scratch, RelPath::new("harness/omp/sessions/2026-10-05_s1.jsonl").unwrap());
    run.env.set_file(&path, format!("{{\"apiKey\":\"{GATEWAY_TOKEN}\",\"bearer\":\"{RUN_BEARER}\"}}\n").as_bytes());
    let (_, exit, _env, _cfg, _dir) = run.close().await;
    let copy = exit.transcript.expect("transcript");
    let text = std::fs::read_to_string(&copy).expect("read transcript");
    assert_eq!(text, "{\"apiKey\":\"[redacted]\",\"bearer\":\"[redacted]\"}\n");
    assert!(copy.ends_with(format!("{}.transcript.jsonl", spec().run_id)));
}

#[tokio::test]
async fn abort_unanswered_terminates() {
    let fixture = r#"{"type":"agent_start"}"#;
    let mut run = start_run(PeerOpts::default(), fixture, spec(), |c| c.abort_deadline_secs = 1).await;
    run.log.until("turn", |e| matches!(e, HarnessEvent::TurnStarted { .. })).await;
    assert!(matches!(run.session.abort().await, Err(HarnessError::Unavailable(_))));
    run.log.end().await;
    let exit = within("session exit", run.session.wait()).await;
    assert_eq!(exit.reason, ExitReason::Killed(KillCause::AbortUnanswered));
    assert!(run.log.seen.iter().any(|e| matches!(e, HarnessEvent::Notice { level: NoticeLevel::Error, text } if text.contains("abort"))));
}

#[tokio::test]
async fn stall_aborts_the_turn() {
    let fixture = [
        r#"{"type":"agent_start"}"#,
        r#"{"__await":"abort"}"#,
        r#"{"__respond":"abort"}"#,
        r#"{"type":"prompt_result","id":"$p1","agentInvoked":true,"status":"aborted","sessionSettled":true}"#,
    ]
    .join("\n");
    let mut run = start_run(PeerOpts::default(), &fixture, spec(), |c| c.stall_timeout_secs = 1).await;
    run.log.until("aborted", |e| matches!(e, HarnessEvent::TurnEnded { outcome: TurnOutcome::Aborted, .. })).await;
    assert!(run.log.seen.iter().any(|e| matches!(e, HarnessEvent::Notice { level: NoticeLevel::Warning, text } if text.contains("silent"))));
    let (_, exit, ..) = run.close().await;
    assert_eq!(exit.reason, ExitReason::Closed);
}

#[tokio::test]
async fn hard_close_kills() {
    let mut run = start_run(PeerOpts::default(), r#"{"type":"agent_start"}"#, spec(), |_| {}).await;
    run.session.close(Duration::ZERO).await.expect("close");
    run.log.end().await;
    assert_eq!(within("session exit", run.session.wait()).await.reason, ExitReason::Killed(KillCause::CloseGraceExpired));
    assert!(matches!(run.session.steer("x".into()).await, Err(HarnessError::Closed)));
}

async fn start_err(opts: PeerOpts, spec: HarnessSpec) -> (HarnessError, Vec<FakeSignal>) {
    let dir = tempfile::tempdir().unwrap();
    let env = FakeEnv::empty();
    let peer = Peer::new(env.push_child());
    let signals = peer.control();
    let peer = tokio::spawn(omp_peer(peer, opts, Vec::new()));
    let harness = OmpHarness::new(config(dir.path()), ModelTable::new(), IsolationMode::Dev);
    let err = match harness.start(env, spec).await {
        Err(e) => e,
        Ok(_) => panic!("start succeeded"),
    };
    peer.abort();
    (err, signals.signals())
}

#[tokio::test]
async fn inventory_extra_mcp_tool_is_an_isolation_violation() {
    let mut opts = PeerOpts::default();
    opts.dump_tools = vec![WORKSPACE_TOOLS.iter().chain(MCP_TOOLS).copied().chain(["mcp__evil_exfiltrate"]).collect()];
    let (err, signals) = start_err(opts, spec()).await;
    assert!(matches!(&err, HarnessError::IsolationViolation(m) if m.contains("mcp__evil_exfiltrate")), "{err:?}");
    assert!(signals.contains(&FakeSignal::Terminate), "process must be terminated");
}

#[tokio::test]
async fn inventory_unexpected_builtin_and_model_fail_closed() {
    let mut opts = PeerOpts::default();
    opts.dump_tools = vec![["read", "task"].into_iter().chain(MCP_TOOLS.iter().copied()).collect()];
    let (err, _) = start_err(opts, spec()).await;
    assert!(matches!(&err, HarnessError::IsolationViolation(m) if m.contains("task")), "{err:?}");
    let opts = PeerOpts { provider: "anthropic", ..PeerOpts::default() };
    let (err, _) = start_err(opts, spec()).await;
    assert!(matches!(&err, HarnessError::IsolationViolation(m) if m.contains("model")), "{err:?}");
}

#[tokio::test]
async fn inventory_waits_for_mcp_readiness() {
    let mut opts = PeerOpts::default();
    opts.dump_tools = vec![WORKSPACE_TOOLS.to_vec(), WORKSPACE_TOOLS.iter().chain(MCP_TOOLS).copied().collect()];
    let mut run = start_run(opts, r#"{"type":"prompt_result","id":"$p1","agentInvoked":true,"status":"completed","sessionSettled":true}"#, spec(), |_| {}).await;
    run.log.until("idle", |e| matches!(e, HarnessEvent::Idle)).await;
    match &run.log.seen[0] {
        HarnessEvent::Ready(info) => assert_eq!(info.tools.len(), 10),
        other => panic!("{other:?}"),
    }
    run.close().await;
}

#[tokio::test]
async fn omp_without_protocol_v2_is_not_found() {
    let opts = PeerOpts { ready: json!({"type": "ready", "protocolVersion": 1, "supportedProtocolVersions": [1]}), ..PeerOpts::default() };
    let (err, _) = start_err(opts, spec()).await;
    assert!(matches!(&err, HarnessError::NotFound(m) if m.contains("v2")), "{err:?}");
}

#[tokio::test]
async fn session_files_argv_and_env() {
    let mut run = start_run(PeerOpts::default(), r#"{"type":"agent_start"}"#, spec(), |_| {}).await;
    let env = run.env.clone();
    let spawn = &env.spawns()[0];
    assert_eq!(spawn.cwd, EnvPath::scratch("launch").unwrap());
    assert_eq!(spawn.role, crate::ports::executor::ProcessRole::Harness);
    let s = "/fake/scratch/harness";
    let tail: Vec<&str> = spawn.argv[1..].iter().map(String::as_str).collect();
    assert_eq!(
        tail,
        vec![
            "--mode", "rpc", "--no-ui", "--session-dir", &format!("{s}/omp/sessions"), "--config", &format!("{s}/omp/overlay.yml"),
            "--model", "mage/claude-sonnet-4-5", "--append-system-prompt", &format!("{s}/omp/system-append.md"),
            "--no-extensions", "--no-skills", "--no-rules", "--no-lsp", "--no-title", "--no-pty", "--auto-approve",
            "--max-time", "3900", "--add-dir", "/fake/ws", "--tools", "read,grep,glob,edit,write,bash,todo",
        ]
    );
    let vars: std::collections::BTreeMap<&str, &str> = spawn.env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    assert_eq!(vars["PI_CODING_AGENT_DIR"], format!("{s}/omp/agent"));
    assert_eq!(vars["OMP_MCP_STARTUP_TIMEOUT_MS"], "0");
    assert_eq!(vars["PI_BASH_NO_LOGIN"], "1");
    assert!(!vars.contains_key("OMP_MCP_TIMEOUT_MS"));
    let joined = format!("{:?}{:?}", spawn.argv, spawn.env);
    assert!(!joined.contains(GATEWAY_TOKEN) && !joined.contains(RUN_BEARER), "secret in argv/env");

    let access: std::collections::BTreeMap<String, FileAccess> =
        env.put_files().into_iter().chain(env.made_dirs()).map(|(p, a)| (p.rel.as_str().to_owned(), a)).collect();
    assert_eq!(access["harness"], FileAccess::AgentReadOnly);
    assert_eq!(access["harness/omp"], FileAccess::AgentReadOnly);
    assert_eq!(access["harness/omp/agent"], FileAccess::AgentCreateOnly);
    assert_eq!(access["harness/omp/sessions"], FileAccess::AgentWritable);
    for f in ["harness/omp/agent/models.yml", "harness/omp/agent/config.yml", "harness/omp/agent/mcp.json", "harness/omp/overlay.yml", "harness/omp/system-append.md", "launch/.omp/mcp.json"] {
        assert_eq!(access[f], FileAccess::AgentReadOnly, "{f}");
    }
    let file = |p: &str| String::from_utf8(env.file(&EnvPath::scratch(p).unwrap()).expect(p)).unwrap();
    let mcp: Value = serde_json::from_str(&file("launch/.omp/mcp.json")).unwrap();
    assert_eq!(
        mcp,
        json!({"mcpServers": {"mage_run": {"type": "http", "url": "http://127.0.0.1:47812/mcp",
            "headers": {"Authorization": format!("Bearer {RUN_BEARER}")}, "timeout": 1_800_000}}})
    );
    assert_eq!(
        file("harness/omp/agent/models.yml"),
        format!(
            "providers:\n  mage:\n    api: \"anthropic-messages\"\n    apiKey: \"{GATEWAY_TOKEN}\"\n    authHeader: true\n    baseUrl: \"http://127.0.0.1:47811\"\n    models:\n      - contextWindow: 200000\n        id: \"claude-sonnet-4-5\"\n        input: [\"text\",\"image\"]\n        maxTokens: 64000\n        name: \"claude-sonnet-4-5 (Mage gateway)\"\n        reasoning: true\n"
        )
    );
    let overlay = file("harness/omp/overlay.yml");
    for needle in [
        "  enableProjectConfig: true\n",
        "disabledProviders: [\"claude\",\"claude-plugins\",\"claude-md\",\"agent-plugins\",\"agents\",\"agents-md\",\"codex\",\"gemini\",\"opencode\",\"cursor\",\"windsurf\",\"cline\",\"github\",\"vscode\",\"mcp-json\",\"ssh-json\",\"omp-plugins\"]\n",
        "  approvalMode: \"yolo\"\n  xdev: false\n",
        "  autoBackground:\n    enabled: false\n",
        "memory:\n  backend: \"off\"\n",
        "  default: \"mage/claude-sonnet-4-5\"\n",
        "async:\n  enabled: false\n",
    ] {
        assert!(overlay.contains(needle), "overlay lacks {needle:?}:\n{overlay}");
    }
    let append = file("harness/omp/system-append.md");
    assert!(append.starts_with("Workspace: /fake/ws (read/write).  Your shell starts in an empty directory: run `cd /fake/ws` first"));
    assert!(append.ends_with("\n\nCall submit_diagnosis when done."));
    run.session.close(Duration::ZERO).await.unwrap();
    run.log.end().await;
}

#[test]
fn argv_and_env_per_policy_and_credentials() {
    let paths = OmpPaths { session: "/s/harness".into(), workspace: None };
    let mut spec = spec();
    spec.builtin_tools = BuiltinToolPolicy::None;
    spec.cwd = None;
    spec.credentials = CredentialsMode::LocalLogin;
    let argv = files::argv("omp", &spec, &paths);
    assert!(argv.contains(&"--no-tools".to_owned()) && !argv.contains(&"--tools".to_owned()) && !argv.contains(&"--add-dir".to_owned()));
    let model = argv.iter().position(|a| a == "--model").unwrap();
    assert_eq!(argv[model + 1], "claude-sonnet-4-5");
    assert!(files::models_yml(&spec, ModelMeta::default()).is_none());
    let cfg = OmpConfig { dev_agent_dir: "/dev/omp-agent".into(), ..OmpConfig::default() };
    let vars = files::env_vars(&spec, &cfg, &paths);
    assert!(vars.contains(&("PI_CODING_AGENT_DIR".into(), "/dev/omp-agent".into())));
    assert!(String::from_utf8(files::overlay_yml(&spec)).unwrap().contains("  smol: \"claude-sonnet-4-5\"\n"));
    spec.builtin_tools = BuiltinToolPolicy::ReadOnly;
    let argv = files::argv("omp", &spec, &OmpPaths { session: "/s/h".into(), workspace: Some("/ws/sub".into()) });
    let tools = argv.iter().position(|a| a == "--tools").unwrap();
    assert_eq!(argv[tools + 1], "read,grep,glob");
    assert!(argv.windows(2).any(|w| w[0] == "--add-dir" && w[1] == "/ws/sub"));
}

#[test]
fn yaml_scalars_cannot_inject_structure() {
    let doc = json!({"a": {"b": "x: y\n- z", "n": 1, "list": ["p", "q"]}, "weird key": true, "empty": {}});
    assert_eq!(files::yaml(&doc), "a:\n  b: \"x: y\\n- z\"\n  list: [\"p\",\"q\"]\n  n: 1\nempty: {}\n\"weird key\": true\n");
}

#[test]
fn mcp_tool_names_follow_omp_minting() {
    assert_eq!(mint_mcp_tool_name("mage_run", "exec"), "mcp__mage_run_exec");
    assert_eq!(mint_mcp_tool_name("mage_run", "mage_run_exec"), "mcp__mage_run_exec");
    assert_eq!(mint_mcp_tool_name("Mage-Run", "Read File"), "mcp__mage_run_read_file");
    assert_eq!(mint_mcp_tool_name("puppeteer", "puppeteer_screenshot"), "mcp__puppeteer_screenshot");
    let names = ToolNames::new(&spec().mcp_servers);
    assert_eq!(names.normalize("mcp__mage_run_submit_test"), ToolRef::Mcp { server: McpServerName::run(), tool: "submit_test".into() });
    assert_eq!(names.normalize("mcp__mage_run_other"), ToolRef::Mcp { server: McpServerName::run(), tool: "other".into() });
    assert_eq!(names.normalize("bash"), ToolRef::Builtin("bash".into()));
    assert_eq!(names.normalize("mcp__unknown_x"), ToolRef::Builtin("mcp__unknown_x".into()));
}

#[test]
fn inventory_check_table() {
    let spec = spec();
    let names = ToolNames::new(&spec.mcp_servers);
    let state = |tools: &[&str], provider: &str, model: &str| {
        json!({"model": {"id": model, "provider": provider}, "dumpTools": tools.iter().map(|n| json!({"name": n})).collect::<Vec<_>>()})
    };
    let all: Vec<&str> = WORKSPACE_TOOLS.iter().chain(MCP_TOOLS).copied().collect();
    assert_eq!(check_inventory(&state(&all, "mage", "claude-sonnet-4-5"), &spec, &names), Ok(()));
    assert!(matches!(check_inventory(&state(&all[..8], "mage", "claude-sonnet-4-5"), &spec, &names), Err(InventoryError::Missing(m)) if m.len() == 2));
    assert!(matches!(check_inventory(&state(&all, "mage", "claude-opus"), &spec, &names), Err(InventoryError::Violation(_))));
    let mut none = spec.clone();
    none.builtin_tools = BuiltinToolPolicy::None;
    none.cwd = None;
    assert!(matches!(check_inventory(&state(&all, "mage", "claude-sonnet-4-5"), &none, &names), Err(InventoryError::Violation(m)) if m.contains("read")));
    assert_eq!(check_inventory(&state(MCP_TOOLS, "mage", "claude-sonnet-4-5"), &none, &names), Ok(()));
    let mut local = spec.clone();
    local.credentials = CredentialsMode::LocalLogin;
    assert_eq!(check_inventory(&state(&all, "anthropic", "claude-sonnet-4-5"), &local, &names), Ok(()));
}
