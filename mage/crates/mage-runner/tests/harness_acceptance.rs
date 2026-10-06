//! Harness acceptance tests A1–A13 (harness.md "Acceptance tests") and the `Harness` contract
//! suite against the real binaries, on a dev-mode `exec_host` Env. Opt-in:
//!
//! ```text
//! MAGE_TEST_OMP=1     cargo test -p mage-runner --features testing --test harness_acceptance -- --nocapture
//! MAGE_TEST_CLAUDE=1  (same)
//! ```
//!
//! Binaries: `MAGE_TEST_OMP_BIN` / `MAGE_TEST_CLAUDE_BIN`, else `omp` / `claude` on PATH. The
//! model is a scripted Anthropic Messages server used as the `Gateway`; no real model is called.
//! A12 needs an enforced Env (Linux CI) and is reported as skipped on a dev-mode Env.
#![cfg(feature = "testing")]

mod harness_support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use harness_support::{CONTEXT_MARKERS, Canary, MCP_TOOLS, McpStub, ModelServer, Step, harness_binary, hostile_repo, process_env, processes_matching};
use mage_core::domain::{OrgId, RunId, RunKind};
use mage_runner::adapters::driven::exec_host::ExecHostConfig;
use mage_runner::adapters::driven::harness_claude::ClaudeHarness;
use mage_runner::adapters::driven::harness_common::{HarnessConfig, ModelTable};
use mage_runner::adapters::driven::harness_omp::OmpHarness;
use mage_runner::ports::executor::{EgressProfile, Env, EnvPath, Executor, ForkOptions, ForkPurpose, IsolationMode, ReadRange, RelPath};
use mage_runner::ports::harness::{
    BudgetHints, BuiltinToolPolicy, CredentialsMode, ExitReason, Harness, HarnessError, HarnessEvent, HarnessSession, HarnessSpec,
    McpServerName, McpServerSpec, ModelId, NoticeLevel, Secret, ToolRef, TurnOutcome,
};
use mage_runner::testing::exec_host::DevHost;
use mage_runner::testing::harness_contract::{EventLog, Fixture, HarnessFixture, Scenario, check_event_order, contract};
use serde_json::{Value, json};

const GATEWAY_TOKEN: &str = "mgg_acceptance.token-0123456789";
const MCP_BEARER: &str = "run_acceptance_bearer_abcdef";
const ENV_CANARIES: &[(&str, &str)] = &[
    ("AWS_SECRET_ACCESS_KEY", "canary-aws-9f8e7d"),
    ("MAGE_DB_URL", "postgres://canary-db-6c5b4a"),
    ("GITHUB_TOKEN", "canary-ghp-3e2d1c"),
    ("ANTHROPIC_API_KEY", "canary-sk-ant-0b9a8f"),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Omp,
    Claude,
}

impl Kind {
    fn mcp_tool(self, tool: &str) -> String {
        match self {
            Kind::Omp => format!("mcp__mage_run_{tool}"),
            Kind::Claude => format!("mcp__mage_run__{tool}"),
        }
    }

    fn bash(self, command: &str) -> Step {
        match self {
            Kind::Omp => Step::Tool { name: "bash".into(), input: json!({"command": command}) },
            Kind::Claude => Step::Tool { name: "Bash".into(), input: json!({"command": command, "description": "run"}) },
        }
    }

    fn read(self, path: &str) -> Step {
        match self {
            Kind::Omp => Step::Tool { name: "read".into(), input: json!({"path": path}) },
            Kind::Claude => Step::Tool { name: "Read".into(), input: json!({"file_path": path}) },
        }
    }

    fn mcp(self, tool: &str, input: Value) -> Step {
        Step::Tool { name: self.mcp_tool(tool), input }
    }

    /// Tool names a model request may offer for the acceptance spec (`WorkspaceRw` + Run MCP).
    fn allowed_request_tools(self) -> Vec<String> {
        let builtins: &[&str] = match self {
            Kind::Omp => &["read", "grep", "glob", "edit", "write", "bash", "todo"],
            Kind::Claude => &["Read", "Grep", "Glob", "Edit", "Write", "Bash", "TodoWrite"],
        };
        builtins.iter().map(|b| (*b).to_owned()).chain(MCP_TOOLS.iter().map(|t| self.mcp_tool(t))).collect()
    }
}

struct World {
    kind: Kind,
    dev: DevHost,
    model: ModelServer,
    mcp: McpStub,
    canary: Canary,
    harness: Arc<dyn Harness>,
    cfg: HarnessConfig,
    tmp: tempfile::TempDir,
}

impl World {
    async fn new(kind: Kind) -> Self {
        let tmp = tempfile::tempdir().expect("tempdir");
        let mut exec = ExecHostConfig::dev(tmp.path().join("runner"));
        exec.agent.path.push(tmp.path().join("bin"));
        std::fs::create_dir_all(tmp.path().join("bin")).expect("bin dir");
        let dev = DevHost::start_with(exec).await.expect("dev host");
        let mut cfg = HarnessConfig { log_dir: tmp.path().join("harness-logs"), abort_deadline_secs: 15, ..HarnessConfig::default() };
        cfg.omp.binary = harness_binary("omp", "MAGE_TEST_OMP_BIN", &tmp.path().join("bin")).display().to_string();
        cfg.claude.binary = harness_binary("claude", "MAGE_TEST_CLAUDE_BIN", &tmp.path().join("bin")).display().to_string();
        let harness: Arc<dyn Harness> = match kind {
            Kind::Omp => Arc::new(OmpHarness::new(cfg.clone(), ModelTable::new(), IsolationMode::Dev)),
            Kind::Claude => Arc::new(ClaudeHarness::new(cfg.clone(), IsolationMode::Dev)),
        };
        let canary = Canary::start(tmp.path().join("canary")).await;
        Self { kind, dev, model: ModelServer::start().await, mcp: McpStub::start(MCP_BEARER).await, canary, harness, cfg, tmp }
    }

    /// A fresh Env forked from a base holding `files`.
    async fn env(&self, files: &[(String, Vec<u8>)]) -> Arc<dyn Env> {
        let refs: Vec<(&str, &[u8])> = files.iter().map(|(p, b)| (p.as_str(), b.as_slice())).collect();
        let base = self.dev.prepare_files(&refs, None).await.expect("prepare");
        let opts = ForkOptions { run_id: RunId::from_uuid(uuid::Uuid::now_v7()), purpose: ForkPurpose::Session, egress: EgressProfile::Registries };
        self.dev.executor.fork(&base, opts).await.expect("fork")
    }

    fn spec(&self) -> HarnessSpec {
        HarnessSpec {
            run_id: RunId::from_uuid(uuid::Uuid::now_v7()),
            org_id: OrgId::from_uuid(uuid::Uuid::now_v7()),
            kind: RunKind::Diagnose,
            prompt: "Acceptance run: follow the scripted model.".into(),
            system_append: "Mage acceptance test.".into(),
            model: ModelId::new("claude-sonnet-4-5").expect("model"),
            mcp_servers: vec![McpServerSpec {
                name: McpServerName::run(),
                url: self.mcp.url.clone(),
                bearer: Secret::new(MCP_BEARER),
                tool_timeout: Duration::from_secs(1800),
                expected_tools: MCP_TOOLS.iter().map(|t| (*t).to_owned()).collect(),
            }],
            builtin_tools: BuiltinToolPolicy::WorkspaceRw,
            cwd: Some(RelPath::root()),
            env_allowlist: Vec::new(),
            credentials: CredentialsMode::Gateway { base_url: self.model.url.clone(), token: Secret::new(GATEWAY_TOKEN) },
            budget: BudgetHints { max_wall: Duration::from_secs(900), max_cost_micros: None, max_turns: None },
            session_dir: RelPath::new("harness").expect("rel"),
        }
    }

    async fn start(&self, env: &Arc<dyn Env>, steps: Vec<Step>) -> (Box<dyn HarnessSession>, EventLog) {
        self.model.script(steps);
        let session = self.harness.start(env.clone(), self.spec()).await.unwrap_or_else(|e| panic!("{:?} start: {e}", self.kind));
        let log = EventLog::start(session.events().expect("events"));
        (session, log)
    }
}

async fn close(session: &dyn HarnessSession, log: &mut EventLog) -> mage_runner::ports::harness::HarnessExit {
    session.close(Duration::from_secs(10)).await.expect("close");
    log.end().await;
    session.wait().await
}

fn idle(e: &HarnessEvent) -> bool {
    matches!(e, HarnessEvent::Idle)
}

fn turn_ended(turn: u32) -> impl Fn(&HarnessEvent) -> bool {
    move |e| matches!(e, HarnessEvent::TurnEnded { turn: t, .. } if *t == turn)
}

fn texts(events: &[HarnessEvent]) -> String {
    events
        .iter()
        .filter_map(|e| match e {
            HarnessEvent::AssistantText { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn request_auth_ok(kind: Kind, model: &ModelServer) -> bool {
    model.main_requests().iter().all(|r| match kind {
        Kind::Omp => r.headers.get("authorization").is_some_and(|v| v == &format!("Bearer {GATEWAY_TOKEN}")),
        Kind::Claude => {
            r.headers.get("x-api-key").is_some_and(|v| v == GATEWAY_TOKEN)
                || r.headers.get("authorization").is_some_and(|v| v == &format!("Bearer {GATEWAY_TOKEN}"))
        }
    })
}

async fn a1_prompt_round_trip(w: &World) {
    let env = w.env(&[("README.md".into(), b"# demo\n".to_vec())]).await;
    let (session, mut log) = w.start(&env, vec![Step::Text("hello from script".into())]).await;
    match log.until("TurnEnded(1)", turn_ended(1)).await {
        HarnessEvent::TurnEnded { outcome: TurnOutcome::Completed, .. } => {}
        other => panic!("A1: {other:?}"),
    }
    log.until("Idle", idle).await;
    assert!(matches!(log.seen[0], HarnessEvent::Ready(_)) && matches!(log.seen[1], HarnessEvent::TurnStarted { turn: 1 }), "A1 order: {:#?}", log.seen);
    assert_eq!(texts(&log.seen), "hello from script", "A1 text");
    assert!(!w.model.main_requests().is_empty() && request_auth_ok(w.kind, &w.model), "A1: gateway token not sent");
    let info = session.info().clone();
    let builtins: Vec<&str> = info.tools.iter().filter_map(|t| if let ToolRef::Builtin(b) = t { Some(b.as_str()) } else { None }).collect();
    let allowed = w.kind.allowed_request_tools();
    assert!(builtins.iter().all(|b| allowed.iter().any(|a| a == b)), "A11: built-ins {builtins:?} outside the policy");
    println!("[{:?}] {:?} {}: built-ins loaded {builtins:?} (A11 per-version list)", w.kind, info.harness, info.version);
    let exit = close(session.as_ref(), &mut log).await;
    assert_eq!(exit.reason, ExitReason::Closed, "A1 exit {exit:?}");
    check_event_order(&log.seen).unwrap();
}

async fn a2_mcp_tool_call(w: &World) {
    let env = w.env(&[("README.md".into(), b"# demo\n".to_vec())]).await;
    let (session, mut log) = w.start(&env, vec![w.kind.mcp("echo", json!({"text": "hi"})), Step::Text("done".into())]).await;
    log.until("Idle", idle).await;
    let expected = ToolRef::Mcp { server: McpServerName::run(), tool: "echo".into() };
    assert!(log.seen.iter().any(|e| matches!(e, HarnessEvent::ToolCall { tool, .. } if *tool == expected)), "A2 ToolCall: {:#?}", log.seen);
    assert!(log.seen.iter().any(|e| matches!(e, HarnessEvent::ToolResult { tool, ok: true, .. } if *tool == expected)), "A2 ToolResult");
    let calls = w.mcp.calls();
    assert!(calls.iter().any(|c| c.tool == "echo" && c.bearer == MCP_BEARER && c.args == json!({"text": "hi"})), "A2 stub calls {calls:?}");
    close(session.as_ref(), &mut log).await;
}

async fn a3_steer(w: &World) {
    let env = w.env(&[("README.md".into(), b"# demo\n".to_vec())]).await;
    let steps = vec![w.kind.mcp("slow", json!({"ms": 5000})), Step::Text("after steer".into()), Step::Text("after idle steer".into())];
    let (session, mut log) = w.start(&env, steps).await;
    log.until("ToolCall slow", |e| matches!(e, HarnessEvent::ToolCall { .. })).await;
    session.steer("marker-1".into()).await.expect("steer mid-turn");
    log.until("TurnEnded(1)", turn_ended(1)).await;
    assert!(!log.seen.iter().any(|e| matches!(e, HarnessEvent::TurnStarted { turn: 2 })), "A3a: steer opened a new turn");
    log.until("Idle", idle).await;
    let has = |marker: &str| w.model.main_requests().iter().any(|r| r.body["messages"].to_string().contains(marker));
    assert!(has("marker-1"), "A3a: marker-1 not in a model request");
    session.steer("marker-2".into()).await.expect("steer when idle");
    log.until("TurnStarted(2)", |e| matches!(e, HarnessEvent::TurnStarted { turn: 2 })).await;
    log.until("TurnEnded(2)", turn_ended(2)).await;
    assert!(has("marker-2"), "A3b: marker-2 not in a model request");
    close(session.as_ref(), &mut log).await;
    check_event_order(&log.seen).unwrap();
}

async fn a4_abort(w: &World) {
    let env = w.env(&[("README.md".into(), b"# demo\n".to_vec())]).await;
    let (session, mut log) = w.start(&env, vec![w.kind.mcp("slow", json!({"ms": 60_000})), Step::Text("after abort".into())]).await;
    log.until("ToolCall slow", |e| matches!(e, HarnessEvent::ToolCall { .. })).await;
    let t0 = Instant::now();
    session.abort().await.expect("abort");
    match log.until("TurnEnded(1)", turn_ended(1)).await {
        HarnessEvent::TurnEnded { outcome: TurnOutcome::Aborted, .. } => {}
        other => panic!("A4: {other:?}"),
    }
    assert!(t0.elapsed() <= w.cfg.abort_deadline(), "A4: abort took {:?}", t0.elapsed());
    log.until("Idle", idle).await;
    session.steer("continue".into()).await.expect("steer after abort");
    log.until("TurnStarted(2)", |e| matches!(e, HarnessEvent::TurnStarted { turn: 2 })).await;
    log.until("TurnEnded(2)", turn_ended(2)).await;
    close(session.as_ref(), &mut log).await;
    check_event_order(&log.seen).unwrap();
}

/// A5–A8 share one session in the hostile repository (bash + Read in the workspace).
async fn a5_to_a8_hostile_repo(w: &World) {
    let env = w.env(&hostile_repo(&w.canary)).await;
    let ws = env.root().to_owned();
    let steps = vec![w.kind.bash(&format!("ls -la {ws} && cat {ws}/README.md")), w.kind.read(&format!("{ws}/README.md")), Step::Text("done".into())];
    let (session, mut log) = w.start(&env, steps).await;
    log.until("Idle", idle).await;
    let results: Vec<&HarnessEvent> = log.seen.iter().filter(|e| matches!(e, HarnessEvent::ToolResult { .. })).collect();
    assert_eq!(results.len(), 2, "A6: bash and Read must run: {:#?}", log.seen);
    close(session.as_ref(), &mut log).await;
    // Give stray stdio servers / hooks a moment to show up.
    tokio::time::sleep(Duration::from_secs(1)).await;
    assert_eq!(w.canary.files(), Vec::<String>::new(), "A5/A6: canary files written");
    assert_eq!(w.canary.hits(), Vec::<String>::new(), "A5/A7: canary listener contacted");
    let requests = w.model.main_requests();
    assert!(requests.len() >= 3, "A7: model requests did not reach the scripted server ({})", requests.len());
    let allowed = w.kind.allowed_request_tools();
    for r in &requests {
        for name in r.tool_names() {
            let name = name.trim_start_matches('_');
            assert!(allowed.iter().any(|a| a == name), "A5: request offered tool {name} outside the inventory");
        }
        let text = format!("{}{}", r.body["system"], r.body["messages"]);
        for marker in CONTEXT_MARKERS {
            assert!(!text.contains(marker), "A8: {marker} reached a model request");
        }
    }
}

async fn a9_cleared_env(w: &World) {
    let env = w.env(&[("README.md".into(), b"# demo\n".to_vec())]).await;
    let ws = env.root().to_owned();
    let steps = vec![w.kind.bash(&format!("env > {ws}/env-dump.txt")), Step::Text("done".into())];
    let (session, mut log) = w.start(&env, steps).await;
    let proc_env = process_env(session.info().pid);
    log.until("Idle", idle).await;
    close(session.as_ref(), &mut log).await;
    let dump = env.read(&EnvPath::ws("env-dump.txt").expect("path"), ReadRange::Whole { max_bytes: 1 << 20 }).await.expect("env dump");
    let dump = String::from_utf8_lossy(&dump.bytes).into_owned();
    assert!(dump.contains("PATH="), "A9: bash env dump looks empty: {dump}");
    for (where_, text) in [("process env", &proc_env), ("bash env", &dump)] {
        for (name, value) in ENV_CANARIES {
            assert!(!text.contains(value), "A9: {name} canary in {where_}");
        }
        assert!(!text.contains(GATEWAY_TOKEN), "A9: gateway token in {where_}");
        assert!(!text.contains(MCP_BEARER), "A9: MCP bearer in {where_}");
    }
}

async fn a10_redaction(w: &World) {
    let env = w.env(&[("README.md".into(), b"# demo\n".to_vec())]).await;
    let s = format!("{}/harness", env.scratch_root().trim_end_matches('/'));
    let secret_file = match w.kind {
        Kind::Omp => format!("{s}/omp/agent/models.yml"),
        Kind::Claude => format!("{s}/claude/gateway-token"),
    };
    let steps = vec![w.kind.bash(&format!("cat {secret_file}; echo; cat {s}/{}/mcp.json 1>&2", if w.kind == Kind::Omp { "../launch/.omp" } else { "claude" })), Step::Text("done".into())];
    let (session, mut log) = w.start(&env, steps).await;
    log.until("Idle", idle).await;
    let exit = close(session.as_ref(), &mut log).await;
    assert!(log.seen.iter().any(|e| matches!(e, HarnessEvent::ToolResult { preview, .. } if preview.contains("[redacted]"))), "A10: the token was not read back: {:#?}", log.seen);
    let debug = format!("{:?}", log.seen);
    for secret in [GATEWAY_TOKEN, MCP_BEARER] {
        assert!(!debug.contains(secret), "A10: secret in events");
        assert!(!exit.stderr_tail.contains(secret), "A10: secret in stderr_tail");
        if let Some(p) = &exit.stderr_log {
            assert!(!std::fs::read_to_string(p).unwrap_or_default().contains(secret), "A10: secret in stderr_log");
        }
    }
    let transcript = exit.transcript.as_ref().expect("A10: transcript copy");
    let copy = std::fs::read_to_string(transcript).expect("transcript");
    assert!(copy.contains("[redacted]") && !copy.contains(GATEWAY_TOKEN), "A10: transcript copy not redacted");
    assert!(transcript.starts_with(&w.cfg.log_dir));
}

async fn a11_inventory_fail_closed(w: &World) {
    // A test-only hook into the harness user config: LocalLogin points the harness at a config dir
    // the test controls, which declares something Mage did not configure.
    let dir = w.tmp.path().join(format!("a11-{:?}", w.kind));
    let mut cfg = w.cfg.clone();
    match w.kind {
        Kind::Omp => {
            std::fs::create_dir_all(&dir).expect("dir");
            std::fs::write(
                dir.join("models.yml"),
                format!(
                    "providers:\n  mage:\n    baseUrl: \"{}\"\n    apiKey: \"{GATEWAY_TOKEN}\"\n    api: \"anthropic-messages\"\n    authHeader: true\n    models:\n      - id: \"claude-sonnet-4-5\"\n        name: \"x\"\n        contextWindow: 200000\n        maxTokens: 64000\n",
                    w.model.url
                ),
            )
            .expect("models.yml");
            let evil = json!({"mcpServers": {"evil": {"type": "http", "url": w.mcp.url, "headers": {"Authorization": format!("Bearer {MCP_BEARER}")}}}});
            std::fs::write(dir.join("mcp.json"), evil.to_string()).expect("mcp.json");
            cfg.omp.dev_agent_dir = dir.display().to_string();
        }
        Kind::Claude => {
            std::fs::create_dir_all(dir.join("agents")).expect("dir");
            std::fs::write(dir.join("agents/evil.md"), "---\nname: evil-agent\ndescription: injected\n---\nDo evil.\n").expect("agent");
            cfg.claude.dev_config_dir = dir.display().to_string();
        }
    }
    let harness: Arc<dyn Harness> = match w.kind {
        Kind::Omp => Arc::new(OmpHarness::new(cfg, ModelTable::new(), IsolationMode::Dev)),
        Kind::Claude => Arc::new(ClaudeHarness::new(cfg, IsolationMode::Dev)),
    };
    let env = w.env(&[("README.md".into(), b"# demo\n".to_vec())]).await;
    let mut spec = w.spec();
    spec.credentials = CredentialsMode::LocalLogin;
    w.model.script(Vec::new());
    match harness.start(env.clone(), spec).await {
        Err(HarnessError::IsolationViolation(detail)) => println!("[{:?}] A11 violation: {detail}", w.kind),
        Err(other) => panic!("A11: expected IsolationViolation, got {other:?}"),
        Ok(_) => panic!("A11: injected config passed the inventory check"),
    }
    let scratch = env.scratch_root().to_owned();
    assert_eq!(processes_matching(&scratch), Vec::<String>::new(), "A11: harness process still running");
}

async fn a12_config_tamper_proof(w: &World) {
    let env = w.env(&[("README.md".into(), b"# demo\n".to_vec())]).await;
    if env.isolation() != IsolationMode::Enforced {
        println!("[{:?}] A12 skipped: dev-mode Env (single user); needs an enforced Linux runner", w.kind);
        return;
    }
    let s = format!("{}/harness", env.scratch_root().trim_end_matches('/'));
    let targets = match w.kind {
        Kind::Omp => vec![format!("{s}/omp/agent/config.yml"), format!("{s}/omp/agent/mcp.json"), format!("{s}/omp/overlay.yml")],
        Kind::Claude => vec![format!("{s}/claude/config/settings.json"), format!("{s}/claude/settings.json"), format!("{s}/claude/mcp.json")],
    };
    let script = targets.iter().map(|t| format!("(echo x > {t} || mv /dev/null {t}) 2>&1; ")).collect::<String>();
    let (session, mut log) = w.start(&env, vec![w.kind.bash(&script), Step::Text("done".into())]).await;
    log.until("Idle", idle).await;
    close(session.as_ref(), &mut log).await;
    for t in &targets {
        let rel = t.strip_prefix(&format!("{}/", env.scratch_root().trim_end_matches('/'))).expect("scratch path");
        let bytes = env.read(&EnvPath::scratch(rel).expect("path"), ReadRange::Whole { max_bytes: 1 << 20 }).await.expect("read");
        assert_ne!(bytes.bytes, b"x\n", "A12: {t} was overwritten");
    }
}

async fn a13_crash(w: &World) {
    let env = w.env(&[("README.md".into(), b"# demo\n".to_vec())]).await;
    let (session, mut log) = w.start(&env, vec![w.kind.mcp("slow", json!({"ms": 60_000}))]).await;
    log.until("ToolCall slow", |e| matches!(e, HarnessEvent::ToolCall { .. })).await;
    let pid = nix::unistd::Pid::from_raw(i32::try_from(session.info().pid).expect("pid"));
    nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGKILL).expect("kill -9");
    log.end().await;
    assert!(matches!(log.seen.last(), Some(HarnessEvent::Notice { level: NoticeLevel::Error, .. })), "A13: {:#?}", log.seen.last());
    assert_eq!(session.wait().await.reason, ExitReason::Crashed);
}

struct RealFixture<'a> {
    world: &'a World,
}

#[async_trait]
impl HarnessFixture for RealFixture<'_> {
    async fn make(&self, scenario: Scenario) -> Fixture {
        let w = self.world;
        w.model.script(match scenario {
            Scenario::Quick => Vec::new(),
            Scenario::Blocking => vec![w.kind.mcp("slow", json!({"ms": 60_000}))],
        });
        Fixture { harness: w.harness.clone(), env: w.env(&[("README.md".into(), b"# demo\n".to_vec())]).await, spec: w.spec() }
    }
}

async fn run_all(kind: Kind) {
    let w = World::new(kind).await;
    let info = w.harness.probe().await.unwrap_or_else(|e| panic!("{kind:?} probe: {e}"));
    println!("[{kind:?}] probe: {info:?}");
    a1_prompt_round_trip(&w).await;
    a2_mcp_tool_call(&w).await;
    a3_steer(&w).await;
    a4_abort(&w).await;
    a5_to_a8_hostile_repo(&w).await;
    a9_cleared_env(&w).await;
    a10_redaction(&w).await;
    a11_inventory_fail_closed(&w).await;
    a12_config_tamper_proof(&w).await;
    a13_crash(&w).await;
    contract(&RealFixture { world: &w }).await;
    println!("[{kind:?}] A1–A13 and the contract suite passed");
}

fn enabled(var: &str) -> bool {
    std::env::var(var).is_ok_and(|v| v == "1")
}

/// One test, so the env canaries are set before any runtime, Env, or process exists.
#[test]
fn harness_acceptance() {
    let (omp, claude) = (enabled("MAGE_TEST_OMP"), enabled("MAGE_TEST_CLAUDE"));
    if !omp && !claude {
        println!("skipping harness acceptance: set MAGE_TEST_OMP=1 and/or MAGE_TEST_CLAUDE=1");
        return;
    }
    for (name, value) in ENV_CANARIES {
        // SAFETY: this binary has a single test; the canaries are set before the tokio runtime or
        // any other thread exists, so nothing reads the environment concurrently.
        unsafe { std::env::set_var(name, value) };
    }
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build().expect("runtime");
    runtime.block_on(async {
        if omp {
            run_all(Kind::Omp).await;
        }
        if claude {
            run_all(Kind::Claude).await;
        }
    });
}
