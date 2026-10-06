//! `Harness` port contract suite (harness.md "Contract tests"), run against `FakeHarness` always
//! and against `harness_omp` / `harness_claude` with real binaries behind the opt-in flags.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::ports::executor::{Env, EnvPath, FileAccess, RelPath};
use crate::ports::harness::{
    BuiltinToolPolicy, CredentialsMode, ExitReason, Harness, HarnessError, HarnessEvent, HarnessEventStream, HarnessSession, HarnessSpec,
    McpServerName, Secret, TurnOutcome,
};

/// How the model behind the harness answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scenario {
    /// Every turn answers with text and ends.
    Quick,
    /// The first turn calls a tool that takes ≥ 60 s; later turns answer with text.
    Blocking,
}

pub struct Fixture {
    pub harness: Arc<dyn Harness>,
    pub env: Arc<dyn Env>,
    /// A valid spec: `builtin_tools` other than `None` with `cwd` = workspace root, at least one
    /// MCP server, `Gateway` or `LocalLogin` credentials. The adapter must not allow direct Vertex.
    pub spec: HarnessSpec,
}

#[async_trait]
pub trait HarnessFixture: Send + Sync {
    async fn make(&self, scenario: Scenario) -> Fixture;
}

/// Runs every contract case.
pub async fn contract(fixture: &dyn HarnessFixture) {
    session_lifecycle(fixture).await;
    abort_mid_turn(fixture).await;
    spec_validation_table(fixture).await;
}

/// Per-event wait used by the suite (real harnesses with a scripted model answer in seconds).
pub const EVENT_TIMEOUT: Duration = Duration::from_secs(60);

/// Drains a session's event stream in the background so tests can wait for specific events.
pub struct EventLog {
    rx: mpsc::UnboundedReceiver<HarnessEvent>,
    pub seen: Vec<HarnessEvent>,
    ended: bool,
}

impl EventLog {
    pub fn start(mut stream: Box<dyn HarnessEventStream>) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(event) = stream.next().await {
                if tx.send(event).is_err() {
                    return;
                }
            }
        });
        Self { rx, seen: Vec::new(), ended: false }
    }

    /// Waits for the first event (after those already consumed) matching `pred`.
    pub async fn until(&mut self, what: &str, pred: impl Fn(&HarnessEvent) -> bool) -> HarnessEvent {
        let wait = async {
            loop {
                match self.rx.recv().await {
                    Some(event) => {
                        self.seen.push(event.clone());
                        if pred(&event) {
                            return Some(event);
                        }
                    }
                    None => {
                        self.ended = true;
                        return None;
                    }
                }
            }
        };
        match tokio::time::timeout(EVENT_TIMEOUT, wait).await {
            Ok(Some(event)) => event,
            Ok(None) => panic!("stream ended while waiting for {what}; events: {:#?}", self.seen),
            Err(_) => panic!("timed out waiting for {what}; events: {:#?}", self.seen),
        }
    }

    /// Events arriving within `window`.
    pub async fn quiet(&mut self, window: Duration) -> Vec<HarnessEvent> {
        let mut got = Vec::new();
        let deadline = tokio::time::Instant::now() + window;
        while let Ok(Some(event)) = tokio::time::timeout_at(deadline, self.rx.recv()).await {
            self.seen.push(event.clone());
            got.push(event);
        }
        got
    }

    /// Waits until the stream returns `None`.
    pub async fn end(&mut self) {
        if self.ended {
            return;
        }
        let drain = async {
            while let Some(event) = self.rx.recv().await {
                self.seen.push(event);
            }
        };
        if tokio::time::timeout(EVENT_TIMEOUT, drain).await.is_err() {
            panic!("event stream did not end; events: {:#?}", self.seen);
        }
        self.ended = true;
    }
}

/// The ordering invariants of the contract: `Ready` first and once; turns strictly increasing;
/// `TurnEnded{t}` only for the open turn `t`; text and tool events inside the open turn; every
/// `ToolResult` after a `ToolCall` with the same `call_id` in the same turn; `Idle` only with no
/// turn open.
pub fn check_event_order(events: &[HarnessEvent]) -> Result<(), String> {
    if !matches!(events.first(), Some(HarnessEvent::Ready(_))) {
        return Err(format!("first event is not Ready: {:?}", events.first()));
    }
    let mut last_turn = 0;
    let mut open: Option<u32> = None;
    let mut calls: Vec<(u32, String)> = Vec::new();
    for (i, e) in events.iter().enumerate().skip(1) {
        let fail = |msg: &str| Err(format!("event {i} {e:?}: {msg}"));
        match e {
            HarnessEvent::Ready(_) => return fail("Ready twice"),
            HarnessEvent::TurnStarted { turn } => {
                if open.is_some() {
                    return fail("TurnStarted while a turn is open");
                }
                if *turn <= last_turn {
                    return fail("turn numbers must increase");
                }
                last_turn = *turn;
                open = Some(*turn);
            }
            HarnessEvent::TurnEnded { turn, .. } => {
                if open != Some(*turn) {
                    return fail("TurnEnded for a turn that is not open");
                }
                open = None;
            }
            HarnessEvent::AssistantText { turn, .. } | HarnessEvent::ToolCall { turn, .. } if open != Some(*turn) => {
                return fail("outside the open turn");
            }
            HarnessEvent::ToolCall { turn, call_id, .. } => calls.push((*turn, call_id.clone())),
            HarnessEvent::ToolResult { turn, call_id, .. } => {
                if open != Some(*turn) {
                    return fail("outside the open turn");
                }
                if !calls.iter().any(|(t, c)| t == turn && c == call_id) {
                    return fail("ToolResult without a ToolCall in the same turn");
                }
            }
            HarnessEvent::Idle => {
                if open.is_some() {
                    return fail("Idle while a turn is open");
                }
            }
            HarnessEvent::AssistantText { .. } | HarnessEvent::Usage(_) | HarnessEvent::Notice { .. } => {}
        }
    }
    Ok(())
}

fn turn_ended(turn: u32) -> impl Fn(&HarnessEvent) -> bool {
    move |e| matches!(e, HarnessEvent::TurnEnded { turn: t, .. } if *t == turn)
}

fn is_idle(e: &HarnessEvent) -> bool {
    matches!(e, HarnessEvent::Idle)
}

/// `events()` once, event order, steer when idle, abort when idle, close → `Closed`, `wait()`
/// twice, steer after close.
pub async fn session_lifecycle(fixture: &dyn HarnessFixture) {
    let f = fixture.make(Scenario::Quick).await;
    let session: Box<dyn HarnessSession> = f.harness.start(f.env.clone(), f.spec.clone()).await.expect("start");
    let mut log = EventLog::start(session.events().expect("events"));
    assert!(matches!(session.events(), Err(HarnessError::Conflict(_))), "events() twice must be Conflict");

    match log.until("TurnEnded(1)", turn_ended(1)).await {
        HarnessEvent::TurnEnded { outcome: TurnOutcome::Completed, .. } => {}
        other => panic!("first turn did not complete: {other:?}"),
    }
    log.until("Idle after turn 1", is_idle).await;

    session.abort().await.expect("abort when idle");
    let after_abort = log.quiet(Duration::from_millis(500)).await;
    assert!(after_abort.is_empty(), "abort when idle emitted {after_abort:?}");

    session.steer("contract steer".into()).await.expect("steer when idle");
    log.until("TurnStarted(2)", |e| matches!(e, HarnessEvent::TurnStarted { turn: 2 })).await;
    match log.until("TurnEnded(2)", turn_ended(2)).await {
        HarnessEvent::TurnEnded { outcome: TurnOutcome::Completed, .. } => {}
        other => panic!("steered turn did not complete: {other:?}"),
    }
    log.until("Idle after turn 2", is_idle).await;

    session.close(Duration::from_secs(10)).await.expect("close");
    log.end().await;
    let first = session.wait().await;
    assert_eq!(first.reason, ExitReason::Closed, "{first:?}");
    assert_eq!(session.wait().await, first, "wait() must be idempotent");
    session.close(Duration::from_secs(10)).await.expect("close is idempotent");
    match session.steer("after close".into()).await {
        Err(HarnessError::Conflict(_) | HarnessError::Closed) => {}
        other => panic!("steer after close: {other:?}"),
    }
    check_event_order(&log.seen).unwrap_or_else(|e| panic!("{e}\n{:#?}", log.seen));
}

/// `abort` mid-turn → `TurnEnded(Aborted)` then `Idle`; a following `steer` works.
pub async fn abort_mid_turn(fixture: &dyn HarnessFixture) {
    let f = fixture.make(Scenario::Blocking).await;
    let session = f.harness.start(f.env.clone(), f.spec.clone()).await.expect("start");
    let mut log = EventLog::start(session.events().expect("events"));
    log.until("ToolCall in turn 1", |e| matches!(e, HarnessEvent::ToolCall { turn: 1, .. })).await;
    session.abort().await.expect("abort mid-turn");
    match log.until("TurnEnded(1)", turn_ended(1)).await {
        HarnessEvent::TurnEnded { outcome: TurnOutcome::Aborted, .. } => {}
        other => panic!("aborted turn: {other:?}"),
    }
    log.until("Idle after abort", is_idle).await;
    session.steer("after abort".into()).await.expect("steer after abort");
    log.until("TurnStarted(2)", |e| matches!(e, HarnessEvent::TurnStarted { turn: 2 })).await;
    match log.until("TurnEnded(2)", turn_ended(2)).await {
        HarnessEvent::TurnEnded { outcome: TurnOutcome::Completed, .. } => {}
        other => panic!("turn after abort: {other:?}"),
    }
    session.close(Duration::from_secs(10)).await.expect("close");
    log.end().await;
    assert_eq!(session.wait().await.reason, ExitReason::Closed);
    check_event_order(&log.seen).unwrap_or_else(|e| panic!("{e}\n{:#?}", log.seen));
}

/// Every row of harness.md "Spec validation" that the fixture's Env can express has a rejecting
/// case; rejected starts leave no session behind.
pub async fn spec_validation_table(fixture: &dyn HarnessFixture) {
    let f = fixture.make(Scenario::Quick).await;
    let base = f.spec.clone();
    assert_ne!(base.builtin_tools, BuiltinToolPolicy::None, "fixture spec must enable built-ins");
    assert!(!base.mcp_servers.is_empty(), "fixture spec needs an MCP server");
    let existing = "contract-existing";
    f.env.make_dir(&EnvPath::scratch(existing).expect("path"), FileAccess::AgentWritable).await.expect("make_dir");

    type Mutation = Box<dyn Fn(&mut HarnessSpec)>;
    let invalid: Vec<(&str, Mutation)> = vec![
        ("no built-ins but cwd set", Box::new(|s| s.builtin_tools = BuiltinToolPolicy::None)),
        ("built-ins without cwd", Box::new(|s| s.cwd = None)),
        ("cwd not a directory", Box::new(|s| s.cwd = Some(RelPath::new("no/such/dir").expect("rel")))),
        ("session_dir exists", Box::new(move |s| s.session_dir = RelPath::new(existing).expect("rel"))),
        ("duplicate MCP server", Box::new(|s| s.mcp_servers.push(s.mcp_servers[0].clone()))),
        ("non-http MCP url", Box::new(|s| s.mcp_servers[0].url = "ftp://127.0.0.1/mcp".into())),
        ("plain http to a remote host", Box::new(|s| s.mcp_servers[0].url = "http://example.com/mcp".into())),
        ("relative MCP url", Box::new(|s| s.mcp_servers[0].url = "/mcp".into())),
        ("no expected tools", Box::new(|s| s.mcp_servers[0].expected_tools.clear())),
        (
            "tool named after its server",
            Box::new(|s| {
                let name = format!("{}_x", s.mcp_servers[0].name);
                s.mcp_servers[0].expected_tools.push(name);
            }),
        ),
        ("secret too short", Box::new(|s| s.mcp_servers[0].bearer = Secret::new("short"))),
        ("secret with shell syntax", Box::new(|s| s.mcp_servers[0].bearer = Secret::new("!cat-/etc/passwd-0123456789"))),
        ("secret with ${VAR}", Box::new(|s| s.mcp_servers[0].bearer = Secret::new("${HOME}_0123456789abcdef"))),
        ("allowlist reserved name", Box::new(|s| s.env_allowlist = vec!["PATH".into()])),
        ("allowlist proxy name", Box::new(|s| s.env_allowlist = vec!["HTTPS_PROXY".into()])),
        ("allowlist credential name", Box::new(|s| s.env_allowlist = vec!["MY_API_TOKEN".into()])),
        ("allowlist harness prefix", Box::new(|s| s.env_allowlist = vec!["ANTHROPIC_BASE_URL".into()])),
        ("allowlist lowercase", Box::new(|s| s.env_allowlist = vec!["lower_case".into()])),
        ("empty prompt", Box::new(|s| s.prompt.clear())),
        ("prompt over 256 KiB", Box::new(|s| s.prompt = "x".repeat(256 * 1024 + 1))),
        ("system_append over 64 KiB", Box::new(|s| s.system_append = "x".repeat(64 * 1024 + 1))),
        ("zero max_wall", Box::new(|s| s.budget.max_wall = Duration::ZERO)),
    ];
    for (name, mutate) in &invalid {
        let mut spec = base.clone();
        mutate(&mut spec);
        match f.harness.start(f.env.clone(), spec).await {
            Err(HarnessError::Invalid(_)) => {}
            Err(other) => panic!("{name}: expected Invalid, got {other:?}"),
            Ok(_) => panic!("{name}: expected Invalid, got a session"),
        }
    }
    let mut vertex = base.clone();
    vertex.credentials = CredentialsMode::Vertex { project_id: "proj".into(), region: "us-east5".into() };
    match f.harness.start(f.env.clone(), vertex).await {
        Err(HarnessError::Unsupported(_)) => {}
        Err(other) => panic!("direct Vertex: expected Unsupported, got {other:?}"),
        Ok(_) => panic!("direct Vertex: expected Unsupported, got a session"),
    }
    // Valid server names only; the newtype rejects the rest before a spec exists.
    for bad in ["Mage", "1x", "a-b", "", &"x".repeat(33)] {
        assert!(McpServerName::new(bad).is_err(), "server name {bad:?}");
    }
}

#[cfg(test)]
mod tests {
    use mage_core::domain::HarnessKind;

    use super::*;
    use crate::ports::harness::{HarnessExit, NoticeLevel, SessionInfo, ToolRef, UsageDelta};
    use crate::testing::executor::FakeEnv;
    use crate::testing::harness::{FakeCall, FakeHarness, SteerMatch, Step, sample_spec};

    fn info() -> SessionInfo {
        SessionInfo {
            harness: HarnessKind::Omp,
            version: "fake".into(),
            native_session_id: "fake-session".into(),
            model: "claude-sonnet-4-5".into(),
            tools: vec![ToolRef::Mcp { server: McpServerName::run(), tool: "exec".into() }],
            pid: 7,
            pgid: 7,
        }
    }

    fn quick_turn(turn: u32, text: &str) -> Vec<Step> {
        vec![
            Step::Emit(HarnessEvent::TurnStarted { turn }),
            Step::Emit(HarnessEvent::AssistantText { turn, text: text.into() }),
            Step::Emit(HarnessEvent::Usage(UsageDelta { input_tokens: 10, output_tokens: 2, model_calls: 1, ..UsageDelta::default() })),
            Step::Emit(HarnessEvent::TurnEnded { turn, outcome: TurnOutcome::Completed }),
            Step::Emit(HarnessEvent::Idle),
        ]
    }

    /// A well-behaved harness, scripted.
    fn script(scenario: Scenario) -> Vec<Step> {
        let mut steps = vec![Step::Emit(HarnessEvent::Ready(info()))];
        match scenario {
            Scenario::Quick => steps.extend(quick_turn(1, "hello")),
            Scenario::Blocking => {
                let tool = ToolRef::Mcp { server: McpServerName::run(), tool: "exec".into() };
                steps.extend([
                    Step::Emit(HarnessEvent::TurnStarted { turn: 1 }),
                    Step::Emit(HarnessEvent::ToolCall { turn: 1, call_id: "c1".into(), tool: tool.clone(), args_preview: "{}".into() }),
                    Step::AwaitAbort,
                    Step::Emit(HarnessEvent::ToolResult { turn: 1, call_id: "c1".into(), tool, ok: false, preview: "cancelled".into() }),
                    Step::Emit(HarnessEvent::TurnEnded { turn: 1, outcome: TurnOutcome::Aborted }),
                    Step::Emit(HarnessEvent::Idle),
                ]);
            }
        }
        steps.push(Step::AwaitSteer(SteerMatch::Any));
        steps.extend(quick_turn(2, "steered"));
        steps
    }

    struct FakeFixture;

    #[async_trait]
    impl HarnessFixture for FakeFixture {
        async fn make(&self, scenario: Scenario) -> Fixture {
            let harness = FakeHarness::new(HarnessKind::Omp);
            harness.push_script(script(scenario));
            Fixture { harness: Arc::new(harness), env: FakeEnv::empty(), spec: sample_spec() }
        }
    }

    #[tokio::test]
    async fn fake_harness_passes_the_contract() {
        contract(&FakeFixture).await;
    }

    #[tokio::test]
    async fn fake_records_calls_in_order_and_queues_early_steers() {
        let harness = FakeHarness::new(HarnessKind::ClaudeCode);
        harness.push_script(vec![
            Step::Emit(HarnessEvent::Ready(info())),
            Step::AwaitSteer(SteerMatch::Contains("gate".into())),
            Step::Emit(HarnessEvent::Notice { level: NoticeLevel::Info, text: "got gate feedback".into() }),
            Step::Exit(HarnessExit {
                reason: ExitReason::Crashed,
                exit_code: Some(1),
                signal: None,
                stderr_tail: "boom".into(),
                stderr_log: None,
                transcript: None,
                usage: UsageDelta::default(),
                wall: Duration::ZERO,
            }),
        ]);
        let spec = sample_spec();
        let session = harness.start(FakeEnv::empty(), spec.clone()).await.unwrap();
        let mut log = EventLog::start(session.events().unwrap());
        session.steer("unrelated".into()).await.unwrap();
        assert!(log.quiet(Duration::from_millis(50)).await.iter().all(|e| matches!(e, HarnessEvent::Ready(_))));
        session.steer("gate failed: retry".into()).await.unwrap();
        log.until("notice", |e| matches!(e, HarnessEvent::Notice { .. })).await;
        log.end().await;
        assert_eq!(session.wait().await.reason, ExitReason::Crashed);
        assert!(matches!(session.steer("late".into()).await, Err(HarnessError::Closed)));
        assert_eq!(
            harness.calls(),
            vec![
                FakeCall::Start(spec.clone()),
                FakeCall::Steer("unrelated".into()),
                FakeCall::Steer("gate failed: retry".into()),
                FakeCall::Steer("late".into()),
            ]
        );
        assert_eq!(harness.started_specs(), vec![spec]);
    }

    #[tokio::test]
    async fn fake_start_errors_and_empty_queue() {
        let harness = FakeHarness::new(HarnessKind::Omp);
        harness.push_start_error(HarnessError::IsolationViolation("extra tool".into()));
        assert!(matches!(harness.start(FakeEnv::empty(), sample_spec()).await, Err(HarnessError::IsolationViolation(_))));
        assert!(matches!(harness.start(FakeEnv::empty(), sample_spec()).await, Err(HarnessError::Unavailable(_))));
    }

    #[test]
    fn order_checker_rejects_violations() {
        let ready = HarnessEvent::Ready(info());
        let tool = ToolRef::Builtin("bash".into());
        let bad: Vec<Vec<HarnessEvent>> = vec![
            vec![HarnessEvent::Idle],
            vec![ready.clone(), HarnessEvent::TurnStarted { turn: 1 }, HarnessEvent::Idle],
            vec![ready.clone(), HarnessEvent::TurnEnded { turn: 1, outcome: TurnOutcome::Completed }],
            vec![ready.clone(), HarnessEvent::TurnStarted { turn: 1 }, HarnessEvent::TurnStarted { turn: 2 }],
            vec![
                ready.clone(),
                HarnessEvent::TurnStarted { turn: 1 },
                HarnessEvent::ToolResult { turn: 1, call_id: "x".into(), tool: tool.clone(), ok: true, preview: String::new() },
            ],
            vec![ready.clone(), HarnessEvent::AssistantText { turn: 1, text: "t".into() }],
            vec![ready.clone(), ready.clone()],
        ];
        for events in bad {
            assert!(check_event_order(&events).is_err(), "{events:?}");
        }
        let good = vec![
            ready,
            HarnessEvent::TurnStarted { turn: 1 },
            HarnessEvent::ToolCall { turn: 1, call_id: "x".into(), tool: tool.clone(), args_preview: String::new() },
            HarnessEvent::ToolResult { turn: 1, call_id: "x".into(), tool, ok: true, preview: String::new() },
            HarnessEvent::TurnEnded { turn: 1, outcome: TurnOutcome::Completed },
            HarnessEvent::Idle,
        ];
        check_event_order(&good).unwrap();
    }
}
