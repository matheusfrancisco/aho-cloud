//! `RunTools` over fakes (executor.md "Testing": tool set per kind, truncation markers and caps,
//! submit validation, gate-in-progress, 409 abort; plus confinement and the MCP log).

use std::sync::Arc;
use std::time::Duration;

use mage_core::domain::fix::{Glob, LockfilePolicy, PatchLimits, PatchPolicy, RegressionPlacement};
use mage_core::domain::{ArtifactKind, CommitSha, FailureSignature, GateId, GateKind, Matcher, OrgId, RepoId, RunId, RunKind, RunStatus, SignatureKind};
use mage_core::testing::{FakeClock, SeqIdGen};
use mage_core::wire::runner::{FixSpec, GateTicket, OutboundEventKind, Rejection, RejectionItem, SubmitAck, WireRunResult};
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::*;
use crate::ports::executor::{
    BundleSource, CaptureLimits, Captured, Command, EgressProfile, EnvPath, ExecOutcome, Executor, Exit, ForkOptions, ForkPurpose, LogSource,
    NullObserver, PrepareRequest, ProcessRole,
};
use crate::ports::link::LinkConflict;
use crate::testing::browser::{FakeBrowserRunner, failed};
use crate::testing::executor::{FakeEnv, FakeExecutor, outcome};
use crate::testing::link::{FakeServerLink, LinkCall};

const RECIPE: &str = r#"version = 1
ports = ["web"]

[services.web]
run = "node server.js"
ports = ["web"]
[services.web.health]
http = "http://127.0.0.1:${port.web}/"

[test]
run = "npm test"
"#;

struct H {
    tools: Arc<RunTools>,
    env: Arc<FakeEnv>,
    link: Arc<FakeServerLink>,
    browser: Arc<FakeBrowserRunner>,
    events: mpsc::UnboundedReceiver<OutboundEventKind>,
    subs: mpsc::UnboundedReceiver<SubmitOutcome>,
    _outbox: TempDir,
}

impl H {
    async fn call(&self, name: &str, args: Value) -> ToolResponse {
        self.tools.call(name, args, CancellationToken::new()).await.unwrap()
    }

    fn submits(&self) -> Vec<WireRunResult> {
        self.link
            .calls()
            .into_iter()
            .filter_map(|c| match c {
                LinkCall::SubmitResult { result, .. } => Some(result),
                _ => None,
            })
            .collect()
    }

    fn drain_subs(&mut self) -> Vec<SubmitOutcome> {
        let mut out = Vec::new();
        while let Ok(o) = self.subs.try_recv() {
            out.push(o);
        }
        out
    }
}

fn policy() -> PatchPolicy {
    PatchPolicy {
        allow_workflow_edits: false,
        lockfiles: LockfilePolicy::WithManifest,
        generated: vec![Glob::new("src/gen/**").unwrap()],
        limits: PatchLimits::default(),
    }
}

/// An env forked from a base with `files` (Prepared with `recipe`, else SourceOnly).
async fn forked(files: &[(&str, &str)], recipe: Option<&str>, purpose: ForkPurpose) -> (Arc<FakeExecutor>, Arc<FakeEnv>) {
    let exec = FakeExecutor::new();
    exec.add_bundle("bundle://x", files.iter().map(|(p, c)| ((*p).to_owned(), c.as_bytes().to_vec())).collect());
    let req = PrepareRequest {
        org_id: OrgId::from_uuid(uuid::Uuid::from_u128(1)),
        repo_id: RepoId::from_uuid(uuid::Uuid::from_u128(2)),
        sha: CommitSha::from_bytes([7; 20]),
        recipe: recipe.map(|r| mage_core::domain::recipe::RecipeSpec::parse(r).unwrap()),
        source_bundle: BundleSource { url: "bundle://x".into(), sha256: [0; 32], bytes: 0 },
    };
    let base = exec.prepare(req, &NullObserver).await.unwrap();
    let opts = ForkOptions { run_id: RunId::from_uuid(uuid::Uuid::from_u128(3)), purpose, egress: EgressProfile::Registries };
    exec.fork(&base, opts).await.unwrap();
    let env = exec.envs().pop().unwrap();
    (exec, env)
}

fn build(kind: RunKind, env: Arc<FakeEnv>, config: RunToolsConfig, fix: Option<FixSpec>) -> H {
    let outbox = tempfile::tempdir().unwrap();
    let link = FakeServerLink::new();
    let browser = Arc::new(FakeBrowserRunner::new());
    let (events_tx, events) = mpsc::unbounded_channel();
    let (subs_tx, subs) = mpsc::unbounded_channel();
    let init = RunToolsInit {
        run_id: RunId::from_uuid(uuid::Uuid::from_u128(3)),
        org_id: OrgId::from_uuid(uuid::Uuid::from_u128(1)),
        kind,
        attempt: 1,
        repo_id: Some(RepoId::from_uuid(uuid::Uuid::from_u128(2))),
        sha: Some(CommitSha::from_bytes([7; 20])),
        env: Some(env.clone() as Arc<dyn Env>),
        fix,
        outbox: outbox.path().to_path_buf(),
        events: events_tx,
        submissions: subs_tx,
    };
    let deps = RunToolsDeps { browser: browser.clone(), link: link.clone(), clock: Arc::new(FakeClock::new()), ids: Arc::new(SeqIdGen::new()) };
    let tools = Arc::new(RunTools::new(init, deps, config).unwrap());
    H { tools, env, link, browser, events, subs, _outbox: outbox }
}

async fn harness(kind: RunKind, files: &[(&str, &str)]) -> H {
    let recipe = (kind != RunKind::Learn).then_some(RECIPE);
    let (_exec, env) = forked(files, recipe, ForkPurpose::Session).await;
    let fix = (kind == RunKind::Fix).then(|| FixSpec {
        brief: mage_core::domain::ArtifactId::from_uuid(uuid::Uuid::from_u128(9)),
        script: mage_core::domain::ArtifactId::from_uuid(uuid::Uuid::from_u128(10)),
        script_sha256: [0; 32],
        signature: sig(SignatureKind::HttpStatus, Matcher::Exact, "500"),
        policy: policy(),
        regression: RegressionPlacement::MageDir,
    });
    build(kind, env, RunToolsConfig::default(), fix)
}

fn sig(kind: SignatureKind, matcher: Matcher, value: &str) -> FailureSignature {
    FailureSignature { kind, value: value.into(), matcher }
}

fn ack(status: RunStatus, message: &str) -> SubmitAck {
    let gate = (status == RunStatus::AwaitingGate)
        .then(|| GateTicket { gate_id: GateId::from_uuid(uuid::Uuid::from_u128(5)), kind: GateKind::Repro, attempt: 1 });
    SubmitAck { status, gate, message: message.into() }
}

// ----- tool sets ---------------------------------------------------------------------------------

#[tokio::test]
async fn tool_set_per_run_kind() {
    let names = |kind| ToolSet::for_kind(kind).iter().map(|t| t.name()).collect::<Vec<_>>();
    assert_eq!(names(RunKind::Triage), ["submit_triage"]);
    assert_eq!(names(RunKind::Learn), ["exec", "read", "write", "list", "logs", "services", "submit_recipe"]);
    assert_eq!(
        names(RunKind::Diagnose),
        ["exec", "read", "write", "list", "logs", "services", "playwright_run", "http_request", "db_query", "submit_diagnosis"]
    );
    assert_eq!(names(RunKind::Fix).last(), Some(&"submit_fix"));
    assert!(!names(RunKind::Fix).contains(&"submit_diagnosis"));
    assert!(names(RunKind::Verify).is_empty());

    // Triage cannot call exec: unknown tool, no event, no MCP line.
    let mut h = build(RunKind::Triage, FakeEnv::empty(), RunToolsConfig::default(), None);
    let err = h.tools.call("exec", json!({"command": "id"}), CancellationToken::new()).await.unwrap_err();
    assert_eq!(err, UnknownTool("exec".into()));
    assert!(h.events.try_recv().is_err());
    assert!(h.tools.call("no_such_tool", json!({}), CancellationToken::new()).await.is_err());
    assert_eq!(h.tools.tool_defs().iter().map(|d| d.name).collect::<Vec<_>>(), ["submit_triage"]);
}

#[test]
fn every_tool_schema_is_a_closed_object() {
    for tool in ALL_TOOLS {
        let schema = input_schema(tool);
        assert_eq!(schema.get("type"), Some(&json!("object")), "{}", tool.name());
        assert_eq!(schema.get("additionalProperties"), Some(&json!(false)), "{} denies unknown fields", tool.name());
        if !tool.is_submit() {
            assert!(schema.get("title").is_none(), "{}", tool.name());
        }
    }
    let exec = Value::Object(input_schema(RunTool::Exec));
    assert_eq!(exec["properties"]["timeout_s"]["maximum"], json!(900));
    assert_eq!(exec["properties"]["timeout_s"]["default"], json!(120));
    let pw = Value::Object(input_schema(RunTool::PlaywrightRun));
    assert_eq!(pw["properties"]["timeout_s"]["minimum"], json!(10));
    assert_eq!(pw["required"], json!(["script"]));
    // submit_* serve the checked-in schemas verbatim.
    assert_eq!(Value::Object(input_schema(RunTool::SubmitTriage)), serde_json::from_str::<Value>(SUBMIT_TRIAGE_SCHEMA).unwrap());
}

#[tokio::test]
async fn unknown_arguments_are_rejected() {
    let h = harness(RunKind::Diagnose, &[]).await;
    let r = h.call("read", json!({"path": "a.txt", "follow": true})).await;
    assert!(!r.ok);
    assert!(r.text.contains("invalid arguments"), "{}", r.text);
}

// ----- exec ----------------------------------------------------------------------------------------

#[tokio::test]
async fn exec_output_is_capped_with_the_omission_marker() {
    let h = harness(RunKind::Diagnose, &[]).await;
    let big: Vec<u8> = (0..100_000u32).map(|i| b'a' + (i % 26) as u8).collect();
    h.env.on_exec(move |spec| {
        (spec.command == Command::Shell("make".into())).then(|| {
            Ok(ExecOutcome {
                exit: Exit::Code(2),
                stdout: Captured::new(&big, CaptureLimits::default()),
                stderr: Captured::new(b"\x1b[31mred\x1b[0m\x07", CaptureLimits::default()),
                duration: Duration::from_millis(5),
                log: LogSource::Exec(0),
            })
        })
    });
    let r = h.call("exec", json!({"command": "make"})).await;
    assert!(r.ok, "a non-zero exit is a result: {}", r.text);
    let v = r.value;
    assert_eq!(v["exit_code"], 2);
    assert_eq!(v["stdout_bytes"], 100_000);
    assert_eq!(v["truncated"], true);
    assert_eq!(v["log"], "exec:1");
    let stdout = v["stdout"].as_str().unwrap();
    let omitted = 100_000 - 8 * 1024 - 24 * 1024;
    assert!(stdout.contains(&format!("\n…[{omitted} bytes omitted; full output: logs source=exec:1]…\n")), "marker present");
    assert!(stdout.starts_with("abcdefgh"));
    assert_eq!(v["stderr"], "red", "ANSI and control characters stripped");
    let spec = h.env.execs().pop().unwrap();
    assert_eq!(spec.role, ProcessRole::Exec);
    assert_eq!(spec.timeout, Duration::from_secs(120));
}

#[tokio::test]
async fn exec_argument_rules() {
    let h = harness(RunKind::Learn, &[]).await;
    let cases = [
        (json!({}), "exactly one of command or argv"),
        (json!({"command": "a", "argv": ["b"]}), "exactly one of command or argv"),
        (json!({"command": "a", "timeout_s": 901}), "timeout_s"),
        (json!({"command": "a", "env": {"PATH": "/x"}}), "reserved"),
        (json!({"command": "a", "env": {"MAGE_RUN_ID": "x"}}), "reserved"),
        (json!({"command": "a", "env": {"lower": "x"}}), "must match"),
        (json!({"command": "a", "cwd": "../up"}), "path escapes the workspace"),
    ];
    for (args, want) in cases {
        let r = h.call("exec", args.clone()).await;
        assert!(!r.ok && r.text.contains(want), "{args}: {}", r.text);
    }
    assert!(h.env.execs().is_empty(), "nothing ran");
}

// ----- files and confinement -----------------------------------------------------------------------

#[tokio::test]
async fn read_write_list_and_confinement() {
    let long_line = "x".repeat(2500);
    let content = format!("one\n{long_line}\nthree\n");
    let h = harness(RunKind::Diagnose, &[("src/app.ts", content.as_str()), ("src/b.ts", "b\n")]).await;

    let r = h.call("read", json!({"path": "src/app.ts", "max_lines": 2})).await;
    assert!(r.ok, "{}", r.text);
    assert_eq!(r.value["start_line"], 1);
    assert_eq!(r.value["end_line"], 2);
    assert_eq!(r.value["total_lines"], 3);
    assert_eq!(r.value["next_line"], 3);
    assert_eq!(r.value["truncated"], true);
    let text = r.value["content"].as_str().unwrap();
    assert!(text.contains("…[line truncated]"));
    assert_eq!(text.lines().nth(1).unwrap().chars().count(), 2000 + "…[line truncated]".chars().count());

    let r = h.call("write", json!({"path": "src/new/c.ts", "content": "c\n"})).await;
    assert_eq!(r.value, json!({"path": "src/new/c.ts", "bytes": 2, "created": true}), "{}", r.text);
    let r = h.call("write", json!({"path": "src/new/c.ts", "content": "d\n", "mode": "create"})).await;
    assert!(!r.ok, "create on an existing file conflicts");

    let r = h.call("list", json!({"path": "src", "depth": 2})).await;
    let paths: Vec<&str> = r.value["entries"].as_array().unwrap().iter().map(|e| e["path"].as_str().unwrap()).collect();
    assert!(paths.contains(&"src/app.ts") && paths.contains(&"src/new/c.ts"), "{paths:?}");

    h.env.add_symlink(&EnvPath::ws("src/escape").unwrap(), "/etc");
    for path in ["../x", "/etc/passwd", "~/x", "src/escape/passwd"] {
        let r = h.call("read", json!({"path": path})).await;
        assert!(!r.ok, "{path}");
        assert!(r.text.contains("path escapes the workspace"), "{path}: {}", r.text);
    }
    let r = h.call("write", json!({"path": "../../etc/x", "content": "x"})).await;
    assert!(r.text.contains("path escapes the workspace"));
}

#[tokio::test]
async fn logs_tail_grep_cursor_and_cap() {
    let h = harness(RunKind::Diagnose, &[]).await;
    let source = LogSource::Service("web".into());
    for i in 0..3000 {
        h.env.push_log(&source, &format!("line {i} {}", "y".repeat(40)));
    }
    let r = h.call("logs", json!({"source": "service:web", "tail": 2000})).await;
    let lines = r.value["lines"].as_array().unwrap();
    assert!(lines.len() < 2000, "64 KiB cap drops the oldest lines");
    assert!(lines.last().unwrap().as_str().unwrap().starts_with("line 2999 "), "newest line kept");
    assert!(r.text.len() <= 64 * 1024);
    assert_eq!(r.value["truncated"], true);
    let cursor = r.value["cursor"].as_str().unwrap().to_owned();
    h.env.push_log(&source, "fresh ERROR");
    let r = h.call("logs", json!({"source": "service:web", "since": cursor, "grep": "ERROR"})).await;
    assert_eq!(r.value["lines"], json!(["fresh ERROR"]));
    for (args, want) in [
        (json!({"source": "nope"}), "unknown log source"),
        (json!({"source": "mcp", "grep": "(unclosed"}), "invalid grep"),
        (json!({"source": "mcp", "tail": 2001}), "tail"),
        (json!({"source": "mcp", "since": "abc"}), "invalid cursor"),
    ] {
        let r = h.call("logs", args).await;
        assert!(!r.ok && r.text.contains(want), "{}", r.text);
    }
}

#[tokio::test]
async fn whole_result_cap_cuts_the_largest_field() {
    let big = "z\n".repeat(60_000);
    let (_exec, env) = forked(&[("big.txt", big.as_str())], Some(RECIPE), ForkPurpose::Session).await;
    let h = build(RunKind::Diagnose, env, RunToolsConfig { max_response_bytes: 4096, ..RunToolsConfig::default() }, None);
    let r = h.call("read", json!({"path": "big.txt"})).await;
    assert!(r.ok);
    assert!(r.text.len() <= 4096, "{}", r.text.len());
    assert!(r.value["content"].as_str().unwrap().ends_with(shape::CUT_MARKER));
    assert_eq!(r.value["path"], "big.txt", "small fields intact");
}

#[tokio::test]
async fn tool_budget_events_and_mcp_log() {
    let (_exec, env) = forked(&[("a.txt", "a\n")], Some(RECIPE), ForkPurpose::Session).await;
    let mut h = build(RunKind::Diagnose, env, RunToolsConfig { max_calls: 2, ..RunToolsConfig::default() }, None);
    assert!(h.call("read", json!({"path": "a.txt"})).await.ok);
    assert!(!h.call("read", json!({"path": "missing.txt"})).await.ok);
    let r = h.call("read", json!({"path": "a.txt"})).await;
    assert_eq!((r.ok, r.text.as_str()), (false, "tool budget exhausted"));

    let mut events = Vec::new();
    while let Ok(e) = h.events.try_recv() {
        events.push(e);
    }
    assert_eq!(events.len(), 6, "ToolCall + ToolResult per call");
    assert!(matches!(&events[0], OutboundEventKind::ToolCall { name, args_preview } if name == "run.read" && args_preview.contains("a.txt")));
    assert!(matches!(&events[3], OutboundEventKind::ToolResult { ok: false, .. }));
    let lines = h.env.mcp_lines();
    assert_eq!(lines.len(), 3);
    let first: Value = serde_json::from_str(&lines[0]).unwrap();
    assert_eq!(first["tool"], "read");
    assert_eq!(first["ok"], true);
}

#[tokio::test]
async fn shutdown_and_cancel_stop_in_flight_calls() {
    let h = harness(RunKind::Diagnose, &[("e2e/repro.spec.ts", "test")]).await;
    h.browser.set_delay(Duration::from_secs(30));
    let cancel = CancellationToken::new();
    let call = {
        let tools = h.tools.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move { tools.call("playwright_run", json!({"script": "e2e/repro.spec.ts"}), cancel).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    cancel.cancel();
    let r = call.await.unwrap().unwrap();
    assert_eq!((r.ok, r.text.as_str()), (false, "cancelled"));
    assert_eq!(h.browser.cancelled(), 1, "the in-flight run was dropped");

    let call = {
        let tools = h.tools.clone();
        tokio::spawn(async move { tools.call("playwright_run", json!({"script": "e2e/repro.spec.ts"}), CancellationToken::new()).await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    h.tools.shutdown();
    let r = call.await.unwrap().unwrap();
    assert!(!r.ok && r.text.contains("shutting down"));
    assert_eq!(h.browser.cancelled(), 2);
    assert!(!h.call("read", json!({"path": "x"})).await.ok, "no new calls after shutdown");
}

// ----- playwright_run, http_request, db_query ------------------------------------------------------

#[tokio::test]
async fn playwright_run_uploads_recordings_within_the_budget() {
    let (_exec, env) = forked(&[("e2e/repro.spec.ts", "test")], Some(RECIPE), ForkPurpose::Session).await;
    let h = build(RunKind::Diagnose, env, RunToolsConfig { max_explore_uploads: 3, ..RunToolsConfig::default() }, None);
    h.browser.set_default(Ok(failed("Error: expected 42", &[500])), true);
    let r = h.call("playwright_run", json!({"script": "e2e/repro.spec.ts"})).await;
    assert!(r.ok, "{}", r.text);
    assert_eq!(r.value["status"], "failed");
    assert_eq!(r.value["http_statuses"], json!([500]));
    assert!(r.value["artifacts"]["video"].is_string());
    assert!(r.value["artifacts"]["trace"].is_string());
    assert!(r.value.get("note").is_none());
    let names: Vec<String> = h.link.artifacts().into_iter().map(|a| a.name).collect();
    assert_eq!(names, ["explore-1.video.webm", "explore-1.trace.zip"]);
    let call = h.browser.calls().pop().unwrap();
    assert_eq!(call.opts.label, "explore-1");
    assert!(call.opts.video && call.opts.trace && call.opts.har);
    assert_eq!(call.opts.timeout, Duration::from_secs(180));

    // Third upload hits the limit of 3: the trace stays local and the result says so.
    let r = h.call("playwright_run", json!({"script": "e2e/repro.spec.ts"})).await;
    assert!(r.value["artifacts"]["video"].is_string());
    assert!(r.value["artifacts"]["trace"].is_null());
    assert!(r.value["note"].as_str().unwrap().contains("kept local"));
    assert_eq!(h.link.artifacts().len(), 3);

    for (args, want) in [
        (json!({"script": "e2e/repro.ts"}), "must be *.spec.ts"),
        (json!({"script": "e2e/repro.spec.ts", "timeout_s": 5}), "timeout_s"),
        (json!({"script": "../repro.spec.ts"}), "path escapes the workspace"),
    ] {
        let r = h.call("playwright_run", args).await;
        assert!(!r.ok && r.text.contains(want), "{}", r.text);
    }
}

#[tokio::test]
async fn http_request_reaches_only_env_ports() {
    let h = harness(RunKind::Diagnose, &[]).await;
    let web = h.env.ports().get("web").unwrap().port();
    for (args, want) in [
        (json!({"method": "GET", "url": "http://169.254.169.254/latest"}), "only 127.0.0.1"),
        (json!({"method": "GET", "url": "http://127.0.0.1:1/x"}), "only 127.0.0.1"),
        (json!({"method": "GET", "url": format!("https://127.0.0.1:{web}/")}), "only http://"),
        (json!({"method": "GET", "url": format!("http://u:p@127.0.0.1:{web}/")}), "credentials"),
        (json!({"method": "GET", "port": "db"}), "unknown port"),
        (json!({"method": "GET"}), "exactly one of url or port"),
        (json!({"method": "GET", "port": "web", "path": "x"}), "must start with /"),
        (json!({"method": "POST", "port": "web", "body": "a", "body_base64": "YQ=="}), "exclusive"),
    ] {
        let r = h.call("http_request", args.clone()).await;
        assert!(!r.ok && r.text.contains(want), "{args}: {}", r.text);
    }

    let listener = match tokio::net::TcpListener::bind(("127.0.0.1", web)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("skipping the live request: port {web} is taken ({e})");
            return;
        }
    };
    let app = axum::Router::new()
        .route("/hello", axum::routing::post(|body: String| async move { (axum::http::StatusCode::CREATED, [("x-echo", "1")], format!("got {body}")) }))
        .route("/bin", axum::routing::get(|| async { vec![0xffu8, 0xfe, 0x00] }))
        .route("/redirect", axum::routing::get(|| async { axum::response::Redirect::temporary("/hello") }))
        .route("/big", axum::routing::get(|| async { "b".repeat(40_000) }));
    tokio::spawn(async move { axum::serve(listener, app).await });
    let r = h.call("http_request", json!({"method": "POST", "port": "web", "path": "/hello", "body": "ping"})).await;
    assert!(r.ok, "{}", r.text);
    assert_eq!(r.value["status"], 201);
    assert_eq!(r.value["body"], "got ping");
    assert_eq!(r.value["headers"]["x-echo"], "1");
    let r = h.call("http_request", json!({"method": "GET", "url": format!("http://localhost:{web}/bin")})).await;
    assert_eq!(r.value["body_base64"], "//4A");
    let r = h.call("http_request", json!({"method": "GET", "port": "web", "path": "/redirect"})).await;
    assert_eq!(r.value["status"], 307, "redirects are not followed");
    let r = h.call("http_request", json!({"method": "GET", "port": "web", "path": "/big"})).await;
    assert_eq!(r.value["body_truncated"], true);
    assert_eq!(r.value["body"].as_str().unwrap().len(), 32 * 1024);
}

#[tokio::test]
async fn db_query_needs_a_database_and_caps_cells() {
    let h = harness(RunKind::Diagnose, &[]).await;
    let r = h.call("db_query", json!({"sql": "select 1"})).await;
    assert!(!r.ok && r.text.contains("no database"), "{}", r.text);

    let with_db = format!("{RECIPE}\n[database]\nmigrate = \"npm run migrate\"\n");
    let (_exec, env) = forked(&[], Some(&with_db), ForkPurpose::Session).await;
    let h = build(RunKind::Diagnose, env, RunToolsConfig::default(), None);
    use crate::ports::executor::{DbColumn, DbRows, DbValue};
    h.env.script_db(Ok(DbRows {
        columns: vec![DbColumn { name: "note".into(), type_name: "text".into() }],
        rows: (0..100).map(|_| vec![DbValue::Text("n".repeat(5000))]).collect(),
        row_count: 100,
        truncated: false,
    }));
    let r = h.call("db_query", json!({"sql": "select note from notes where id = $1", "params": [7]})).await;
    assert!(r.ok, "{}", r.text);
    let rows = r.value["rows"].as_array().unwrap();
    assert!(rows.len() < 100, "64 KiB total");
    assert_eq!(r.value["truncated"], true);
    assert!(rows[0][0].as_str().unwrap().len() <= 4 * 1024 + 3, "cell capped at 4 KiB");
    let r = h.call("db_query", json!({"sql": "x", "params": [[1]]})).await;
    assert!(r.text.contains("params[0]"));
}

// ----- services ---------------------------------------------------------------------------------------

#[tokio::test]
async fn services_actions_per_kind() {
    let with_db = format!("{RECIPE}\n[database]\nmigrate = \"npm run migrate\"\n");
    let (_exec, env) = forked(&[], Some(&with_db), ForkPurpose::Session).await;
    let h = build(RunKind::Diagnose, env, RunToolsConfig::default(), None);
    let r = h.call("services", json!({"action": "start"})).await;
    assert!(r.ok, "{}", r.text);
    assert_eq!(r.value["all_healthy"], true);
    assert_eq!(r.value["services"][0]["name"], "web");
    assert_eq!(r.value["services"][0]["state"], "healthy");
    assert!(r.value["services"][0]["ports"]["web"].is_u64());
    let r = h.call("services", json!({"action": "validate"})).await;
    assert!(!r.ok && r.text.contains("not available"), "{}", r.text);
    assert!(h.call("services", json!({"action": "db_reset"})).await.ok);
    assert_eq!(h.env.db_resets(), 1);

    let l = harness(RunKind::Learn, &[]).await;
    let r = l.call("services", json!({"action": "validate"})).await;
    assert!(!r.ok && r.text.contains("write the draft recipe to .mage/env.toml first"), "{}", r.text);
    l.env.set_file(&EnvPath::ws(".mage/env.toml").unwrap(), b"version = 2\n");
    let r = l.call("services", json!({"action": "validate"})).await;
    assert!(!r.ok && r.text.contains("[version]"), "{}", r.text);
    l.env.set_file(&EnvPath::ws(".mage/env.toml").unwrap(), RECIPE.as_bytes());
    let r = l.call("services", json!({"action": "validate"})).await;
    assert!(r.ok, "{}", r.text);
    assert!(r.value["issues"].is_array());
    let r = l.call("services", json!({"action": "start"})).await;
    assert!(r.ok, "the draft recipe was installed: {}", r.text);
    assert_eq!(r.value["services"][0]["name"], "web");
    let r = l.call("services", json!({"action": "db_reset"})).await;
    assert!(!r.ok, "db_reset is Diagnose/Fix only");
}

// ----- submits ----------------------------------------------------------------------------------------

#[tokio::test]
async fn submit_triage_validates_then_forwards() {
    let mut h = build(RunKind::Triage, FakeEnv::empty(), RunToolsConfig::default(), None);
    let r = h.call("submit_triage", json!({"summary": "s", "causes": []})).await;
    assert!(!r.ok && r.text.contains("missing_info: is required"), "{}", r.text);
    assert!(h.submits().is_empty(), "no POST /result on a schema error");

    h.link.script_submit(Err(LinkError::Rejected(Rejection {
        gate: None,
        message: "REJECTED (attempt 1 of 2).".into(),
        items: vec![RejectionItem { path: "causes[0].evidence[0]".into(), reason: "path not found".into() }],
    })));
    let ok_args = json!({"summary": "s", "causes": [], "missing_info": ["which org?"]});
    let r = h.call("submit_triage", ok_args.clone()).await;
    assert!(!r.ok);
    assert_eq!(r.text, "REJECTED (attempt 1 of 2).\n- causes[0].evidence[0]: path not found");
    assert!(matches!(h.drain_subs().as_slice(), [SubmitOutcome::Rejected(_)]));

    h.link.script_submit(Ok(ack(RunStatus::Succeeded, "Accepted 0 causes. End your turn now.")));
    let r = h.call("submit_triage", ok_args).await;
    assert!(r.ok);
    assert_eq!(r.text, "Accepted 0 causes. End your turn now.", "the server message verbatim");
    assert!(matches!(&h.submits()[1], WireRunResult::Triage(t) if t.missing_info == ["which org?"]));
    assert!(matches!(h.drain_subs().as_slice(), [SubmitOutcome::Accepted { ack, .. }] if ack.status == RunStatus::Succeeded));
}

#[tokio::test]
async fn conflict_409_reports_run_abort() {
    let mut h = build(RunKind::Triage, FakeEnv::empty(), RunToolsConfig::default(), None);
    h.link.script_submit(Err(LinkError::Conflict(LinkConflict::LeaseLost)));
    let r = h.call("submit_triage", json!({"summary": "s", "causes": [], "missing_info": ["q"]})).await;
    assert_eq!((r.ok, r.text.as_str()), (false, "run cancelled"));
    assert_eq!(h.drain_subs(), [SubmitOutcome::Conflict(LinkConflict::LeaseLost)]);
    h.link.script_submit(Err(LinkError::Unavailable("timeout".into())));
    let r = h.call("submit_triage", json!({"summary": "s", "causes": [], "missing_info": ["q"]})).await;
    assert_eq!(r.text, "submission service unavailable, call submit_triage again");
}

#[tokio::test]
async fn submit_recipe_errors_sync_warning_and_success() {
    let mut h = harness(RunKind::Learn, &[]).await;
    let r = h.call("submit_recipe", json!({"toml": "version = 1\nports = [\"Bad Port\"]\n", "notes": ""})).await;
    assert!(!r.ok && r.text.contains("[bad_name]"), "{}", r.text);
    let r = h.call("submit_recipe", json!({"toml": "version = 1\nbogus = 1\n", "notes": ""})).await;
    assert!(!r.ok && r.text.contains("does not parse"), "{}", r.text);
    assert!(h.submits().is_empty());

    h.link.script_submit(Ok(ack(RunStatus::AwaitingGate, "submitted (gate: Boot, attempt 1). End your turn now.")));
    let r = h.call("submit_recipe", json!({"toml": RECIPE, "notes": "n"})).await;
    assert!(r.ok, "{}", r.text);
    assert_eq!(r.value["submitted"], true);
    let want_hash = mage_core::domain::recipe::RecipeSpec::parse(RECIPE).unwrap().hash();
    assert_eq!(r.value["recipe_hash"], json!(want_hash));
    let codes: Vec<&str> = r.value["warnings"].as_array().unwrap().iter().map(|w| w["code"].as_str().unwrap()).collect();
    assert!(codes.contains(&"workspace_out_of_sync"), "{codes:?}");
    assert!(matches!(&h.submits()[0], WireRunResult::Learn(p) if p.toml == RECIPE && p.notes == "n"));
    assert!(h.tools.gate_pending());

    // A second submit while the Boot gate runs.
    let r = h.call("submit_recipe", json!({"toml": RECIPE, "notes": ""})).await;
    assert!(!r.ok && r.text.starts_with("gate in progress"), "{}", r.text);
    assert_eq!(h.submits().len(), 1);
    assert!(h.call("list", json!({})).await.ok, "other tools stay usable");
    h.tools.gate_feedback_received();
    h.env.set_file(&EnvPath::ws(".mage/env.toml").unwrap(), RECIPE.as_bytes());
    h.link.script_submit(Ok(ack(RunStatus::AwaitingGate, "again")));
    let r = h.call("submit_recipe", json!({"toml": RECIPE, "notes": ""})).await;
    assert!(r.ok);
    assert!(!r.value["warnings"].as_array().unwrap().iter().any(|w| w["code"] == "workspace_out_of_sync"));
    assert_eq!(h.drain_subs().len(), 2);
}

#[tokio::test]
async fn submit_diagnosis_rejections_consume_no_attempt() {
    let h = harness(RunKind::Diagnose, &[("e2e/repro.spec.ts", "test"), ("repro.sh", "curl"), ("notes.md", "x")]).await;
    let pw_sig = json!({"kind": "assertion_text", "value": "expected 42", "matcher": "contains"});
    let cases = [
        (json!({"verdict": "reproduced", "summary": "s"}), "script"),
        (json!({"verdict": "reproduced", "summary": "s", "script": "missing.spec.ts", "signature": pw_sig}), "does not exist"),
        (json!({"verdict": "reproduced", "summary": "s", "script": "notes.md", "signature": pw_sig}), "script must be"),
        (json!({"verdict": "reproduced", "summary": "s", "script": "../x.sh", "signature": pw_sig}), "path escapes the workspace"),
        (
            json!({"verdict": "reproduced", "summary": "s", "script": "e2e/repro.spec.ts", "signature": {"kind": "exit_code", "value": "1", "matcher": "exact"}}),
            "exit_code needs a shell script",
        ),
        (json!({"verdict": "reproduced", "summary": "s", "script": "repro.sh", "signature": pw_sig}), "assertion_text needs a Playwright script"),
        (
            json!({"verdict": "reproduced", "summary": "s", "script": "repro.sh", "signature": {"kind": "error_string", "value": "(unclosed", "matcher": "regex"}}),
            "signature: ",
        ),
        (
            json!({"verdict": "reproduced", "summary": "s", "script": "repro.sh", "signature": {"kind": "http_status", "value": "5xx", "matcher": "exact"}}),
            "three digits",
        ),
        (
            json!({"verdict": "reproduced", "summary": "s", "script": "repro.sh", "signature": {"kind": "http_status", "value": "500", "matcher": "contains"}}),
            "matcher exact",
        ),
        (json!({"verdict": "inconclusive", "summary": "s"}), "reason"),
    ];
    for (args, want) in cases {
        let r = h.call("submit_diagnosis", args.clone()).await;
        assert!(!r.ok, "{args}");
        assert!(r.text.contains(want), "{args}: {}", r.text);
    }
    assert!(h.submits().is_empty());
    assert!(h.link.artifacts().is_empty(), "no script uploaded");
}

#[tokio::test]
async fn submit_diagnosis_precheck_requires_the_signature() {
    let h = harness(RunKind::Diagnose, &[("e2e/repro.spec.ts", "test")]).await;
    h.browser.push(Ok(failed("Error: expected 41 to be 42", &[404])));
    let args = json!({
        "verdict": "reproduced", "summary": "s", "script": "e2e/repro.spec.ts",
        "signature": {"kind": "http_status", "value": "500", "matcher": "exact"},
    });
    let r = h.call("submit_diagnosis", args.clone()).await;
    assert!(!r.ok);
    assert_eq!(r.text, "signature not observed when your script ran here; observed: [\"404\"]");
    assert!(h.submits().is_empty(), "no POST /result");
    let precheck = h.browser.calls().pop().unwrap();
    assert!(!precheck.opts.video && !precheck.opts.trace && !precheck.opts.har, "precheck records nothing");

    h.browser.push(Ok(failed("Error: boom", &[500])));
    h.link.script_submit(Ok(ack(RunStatus::AwaitingGate, "submitted (gate: Repro, attempt 1). End your turn now.")));
    let mut args = args;
    args["fix_sites"] = json!([{"path": "src/export.ts", "start_line": 3, "end_line": 9}]);
    let r = h.call("submit_diagnosis", args).await;
    assert!(r.ok, "{}", r.text);
    assert_eq!(r.value["gate"], "repro");
    let artifacts = h.link.artifacts();
    assert_eq!(artifacts.len(), 1);
    assert_eq!((artifacts[0].kind, artifacts[0].name.as_str()), (ArtifactKind::Script, "s2-repro.spec.ts"));
    assert_eq!(artifacts[0].bytes, b"test");
    let WireRunResult::Diagnose(d) = &h.submits()[0] else { panic!("diagnose result") };
    assert_eq!(d.script, Some(artifacts[0].id));
    assert_eq!(d.fix_sites[0].path.as_str(), "src/export.ts");
}

#[tokio::test]
async fn submit_diagnosis_shell_precheck_observes_exit_code_and_status_lines() {
    let h = harness(RunKind::Diagnose, &[("repro.sh", "curl -s ...")]).await;
    h.env.on_exec(|spec| {
        (spec.role == ProcessRole::Precheck).then(|| Ok(outcome(Exit::Code(3), b"MAGE_HTTP_STATUS=500\n", b"")))
    });
    h.link.script_submit(Ok(ack(RunStatus::AwaitingGate, "ok")));
    let r = h
        .call(
            "submit_diagnosis",
            json!({"verdict": "reproduced", "summary": "s", "script": "repro.sh", "signature": {"kind": "exit_code", "value": "3", "matcher": "exact"}}),
        )
        .await;
    assert!(r.ok, "{}", r.text);
    let spec = h.env.execs().pop().unwrap();
    assert_eq!(spec.command, Command::Argv(vec!["bash".into(), "/fake/ws/repro.sh".into()]));
    assert_eq!(spec.timeout, Duration::from_secs(300));
    assert_eq!(h.link.artifacts()[0].name, "repro.sh", "first submission keeps the file name");

    // not_reproduced skips the precheck.
    h.tools.gate_feedback_received();
    let before = h.env.execs().len();
    h.link.script_submit(Ok(ack(RunStatus::Succeeded, "recorded")));
    let r = h
        .call(
            "submit_diagnosis",
            json!({"verdict": "not_reproduced", "summary": "s", "script": "repro.sh", "signature": {"kind": "exit_code", "value": "9", "matcher": "exact"}}),
        )
        .await;
    assert!(r.ok, "{}", r.text);
    assert_eq!(h.env.execs().len(), before);
}

#[tokio::test]
async fn submit_fix_empty_deny_binary_and_exclusions() {
    let files = [
        ("src/app.ts", "a\n"),
        ("package.json", "{}\n"),
        ("package-lock.json", "{}\n"),
        (".github/workflows/ci.yml", "on: push\n"),
        (".mage/env.toml", RECIPE),
    ];
    let h = harness(RunKind::Fix, &files).await;
    let r = h.call("submit_fix", json!({"summary": "fix"})).await;
    assert!(!r.ok && r.text.starts_with("no changes"), "{}", r.text);

    // Only excluded paths changed: still no changes, and the agent learns why.
    let ws = |p: &str| EnvPath::ws(p).unwrap();
    h.env.set_file(&ws(".mage/repro/x.spec.ts"), b"repro\n");
    h.env.set_file(&ws("package-lock.json"), b"{\"v\":2}\n");
    let r = h.call("submit_fix", json!({"summary": "fix"})).await;
    assert!(!r.ok && r.text.contains("no changes") && r.text.contains("package-lock.json"), "{}", r.text);

    h.env.set_file(&ws(".github/workflows/ci.yml"), b"on: [push, pull_request]\n");
    h.env.set_file(&ws("src/app.ts"), b"b\n");
    let r = h.call("submit_fix", json!({"summary": "fix"})).await;
    assert!(!r.ok && r.text.contains(".github/workflows/ci.yml"), "{}", r.text);
    h.env.set_file(&ws(".github/workflows/ci.yml"), b"on: push\n");

    h.env.set_file(&ws("src/logo.png"), &[0x89, b'P', b'N', b'G', 0, 1, 2]);
    let r = h.call("submit_fix", json!({"summary": "fix"})).await;
    assert!(!r.ok && r.text.contains("binary") && r.text.contains("src/logo.png"), "{}", r.text);
    assert!(h.submits().is_empty() && h.link.artifacts().is_empty());
}

#[tokio::test]
async fn submit_fix_uploads_the_computed_patch() {
    let files = [("src/app.ts", "a\n"), ("package.json", "{}\n"), ("package-lock.json", "{}\n"), ("src/gen/api.ts", "g\n")];
    let mut h = harness(RunKind::Fix, &files).await;
    let ws = |p: &str| EnvPath::ws(p).unwrap();
    h.env.set_file(&ws("src/app.ts"), b"b\n");
    h.env.set_file(&ws("package-lock.json"), b"{\"v\":2}\n");
    h.env.set_file(&ws("src/gen/api.ts"), b"regenerated\n");
    h.env.set_file(&ws(".mage/repro/x.spec.ts"), b"repro\n");
    h.link.script_submit(Ok(ack(RunStatus::AwaitingGate, "submitted (gate: Fix, attempt 1). End your turn now.")));
    let r = h.call("submit_fix", json!({"summary": "Fix the off-by-one."})).await;
    assert!(r.ok, "{}", r.text);
    assert_eq!(r.value["files"], json!([{"path": "src/app.ts", "change": "modify"}]));
    let excluded: Vec<&str> = r.value["excluded"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(excluded, [".mage/repro/x.spec.ts", "package-lock.json", "src/gen/api.ts"]);
    let patch = &h.link.artifacts()[0];
    assert_eq!((patch.kind, patch.name.as_str()), (ArtifactKind::Patch, "fix-1.patch"));
    let text = String::from_utf8(patch.bytes.clone()).unwrap();
    assert!(text.contains("+++ b/src/app.ts") && !text.contains("package-lock") && !text.contains(".mage/"), "{text}");
    let WireRunResult::Fix(result) = &h.submits()[0] else { panic!("fix result") };
    assert_eq!(result.patch, patch.id);
    assert_eq!(result.summary, "Fix the off-by-one.");
    assert_eq!((result.stats.files, result.stats.added, result.stats.removed), (1, 1, 1));
    assert!(matches!(h.drain_subs().as_slice(), [SubmitOutcome::Accepted { .. }]));
    let r = h.call("submit_fix", json!({"summary": "again"})).await;
    assert!(r.text.starts_with("gate in progress"));
}

#[test]
fn artifact_names_are_storage_safe() {
    assert_eq!(artifact_name("Repro.Spec.TS"), "repro.spec.ts");
    assert_eq!(artifact_name("_my repro!.sh"), "my-repro-.sh");
    assert_eq!(artifact_name("é.sh"), "sh", "leading non-alphanumerics dropped");
    assert_eq!(artifact_name("..."), "file");
    let long = format!("{}.spec.ts", "a".repeat(200));
    let name = artifact_name(&long);
    assert_eq!(name.len(), 128);
    assert!(name.ends_with(".spec.ts"));
    for n in ["Repro.Spec.TS", "_my repro!.sh", "é.sh", "...", long.as_str()] {
        assert!(mage_core::wire::runner::ArtifactSpec::name_valid(&artifact_name(n)), "{n}");
    }
}
