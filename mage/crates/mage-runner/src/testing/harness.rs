//! `FakeHarness` (harness.md "Fake"): scripted sessions — `Step::{Emit, AwaitSteer, AwaitAbort,
//! Exit}` — that record every `start`/`steer`/`abort`/`close` call in order. The script runs
//! synchronously inside `start`/`steer`/`abort`, so behavior is deterministic; time comes only
//! from the injected `Clock`.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mage_core::domain::HarnessKind;
use mage_core::ports::clock::Clock;
use mage_core::testing::FakeClock;
use parking_lot::Mutex;
use time::OffsetDateTime;
use tokio::sync::{mpsc, watch};

use crate::ports::executor::{Env, RelPath};
use crate::ports::harness::{
    BudgetHints, BuiltinToolPolicy, CredentialModeKind, CredentialsMode, ExitReason, Harness, HarnessError, HarnessEvent,
    HarnessEventStream, HarnessExit, HarnessInfo, HarnessSession, HarnessSpec, McpServerName, McpServerSpec, ModelId, Secret, SessionInfo,
    SteerId, UsageDelta,
};

/// A valid Diagnose-shaped spec (Gateway, `WorkspaceRw` at the workspace root, both Mage MCP
/// servers) for tests.
pub fn sample_spec() -> HarnessSpec {
    HarnessSpec {
        run_id: mage_core::domain::RunId::from_uuid(uuid::Uuid::from_u128(0x0190_0000_0000_7000_8000_0000_0000_0001)),
        org_id: mage_core::domain::OrgId::from_uuid(uuid::Uuid::from_u128(0x0190_0000_0000_7000_8000_0000_0000_0002)),
        kind: mage_core::domain::RunKind::Diagnose,
        prompt: "Diagnose the failing checkout.".into(),
        system_append: "Call submit_diagnosis when done.".into(),
        model: ModelId::new("claude-sonnet-4-5").expect("model id"),
        mcp_servers: vec![
            McpServerSpec {
                name: McpServerName::knowledge(),
                url: "https://mage.example.com/mcp/knowledge".into(),
                bearer: Secret::new("mgk_knowledge.token-0123456789"),
                tool_timeout: Duration::from_secs(120),
                expected_tools: vec!["search_code".into(), "find_symbol".into()],
            },
            McpServerSpec {
                name: McpServerName::run(),
                url: "http://127.0.0.1:43117/mcp".into(),
                bearer: Secret::new("run_bearer_0123456789abcdef"),
                tool_timeout: Duration::from_secs(1800),
                expected_tools: vec!["exec".into(), "read".into(), "submit_diagnosis".into()],
            },
        ],
        builtin_tools: BuiltinToolPolicy::WorkspaceRw,
        cwd: Some(RelPath::root()),
        env_allowlist: Vec::new(),
        credentials: CredentialsMode::Gateway {
            base_url: "https://mage.example.com/llm/anthropic".into(),
            token: Secret::new("mgg_gateway.token-0123456789"),
        },
        budget: BudgetHints { max_wall: Duration::from_secs(3600), max_cost_micros: Some(5_000_000), max_turns: Some(200) },
        session_dir: RelPath::new(crate::ports::harness::DEFAULT_SESSION_DIR).expect("session dir"),
    }
}

/// One scripted step of a fake session.
#[derive(Debug, Clone)]
pub enum Step {
    Emit(HarnessEvent),
    /// Blocks until a matching `steer` (earlier unmatched steers are consumed first).
    AwaitSteer(SteerMatch),
    /// Blocks until `abort` is called while waiting here.
    AwaitAbort,
    /// The process exits: the stream ends after buffered events, `wait` returns this exit.
    Exit(HarnessExit),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SteerMatch {
    Any,
    Contains(String),
}

impl SteerMatch {
    fn matches(&self, text: &str) -> bool {
        match self {
            Self::Any => true,
            Self::Contains(needle) => text.contains(needle.as_str()),
        }
    }
}

/// A recorded call, in call order across all sessions of one `FakeHarness`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FakeCall {
    Start(HarnessSpec),
    Steer(String),
    Abort,
    Close(Duration),
}

#[derive(Default)]
struct Recorder {
    scripts: VecDeque<Result<Vec<Step>, HarnessError>>,
    calls: Vec<FakeCall>,
}

pub struct FakeHarness {
    kind: HarnessKind,
    clock: Arc<dyn Clock>,
    supported: Vec<CredentialModeKind>,
    allow_direct_vertex: bool,
    recorder: Arc<Mutex<Recorder>>,
}

impl FakeHarness {
    /// Supports `Gateway` and `LocalLogin` (dev Envs only), no direct Vertex — like the omp adapter.
    pub fn new(kind: HarnessKind) -> Self {
        Self {
            kind,
            clock: Arc::new(FakeClock::new()),
            supported: vec![CredentialModeKind::Gateway, CredentialModeKind::LocalLogin],
            allow_direct_vertex: false,
            recorder: Arc::new(Mutex::new(Recorder::default())),
        }
    }

    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    /// Credential policy used by spec validation.
    pub fn with_credentials(mut self, supported: Vec<CredentialModeKind>, allow_direct_vertex: bool) -> Self {
        self.supported = supported;
        self.allow_direct_vertex = allow_direct_vertex;
        self
    }

    /// Script for the next `start` (FIFO).
    pub fn push_script(&self, steps: Vec<Step>) {
        self.recorder.lock().scripts.push_back(Ok(steps));
    }

    /// The next `start` fails with `err` (after spec validation).
    pub fn push_start_error(&self, err: HarnessError) {
        self.recorder.lock().scripts.push_back(Err(err));
    }

    pub fn calls(&self) -> Vec<FakeCall> {
        self.recorder.lock().calls.clone()
    }

    pub fn started_specs(&self) -> Vec<HarnessSpec> {
        self.calls()
            .into_iter()
            .filter_map(|c| match c {
                FakeCall::Start(spec) => Some(spec),
                _ => None,
            })
            .collect()
    }
}

#[async_trait]
impl Harness for FakeHarness {
    fn kind(&self) -> HarnessKind {
        self.kind
    }

    async fn probe(&self) -> Result<HarnessInfo, HarnessError> {
        let mut modes = self.supported.clone();
        if self.allow_direct_vertex && !modes.contains(&CredentialModeKind::Vertex) {
            modes.push(CredentialModeKind::Vertex);
        }
        Ok(HarnessInfo { kind: self.kind, version: "fake".into(), credential_modes: modes })
    }

    async fn start(&self, env: Arc<dyn Env>, spec: HarnessSpec) -> Result<Box<dyn HarnessSession>, HarnessError> {
        self.recorder.lock().calls.push(FakeCall::Start(spec.clone()));
        spec.validate_in(env.as_ref(), &self.supported, self.allow_direct_vertex).await?;
        let script = self.recorder.lock().scripts.pop_front().ok_or_else(|| HarnessError::Unavailable("FakeHarness: no script queued".into()))??;
        let info = script
            .iter()
            .find_map(|s| match s {
                Step::Emit(HarnessEvent::Ready(info)) => Some(info.clone()),
                _ => None,
            })
            .unwrap_or_else(|| SessionInfo {
                harness: self.kind,
                version: "fake".into(),
                native_session_id: format!("fake-{}", spec.run_id),
                model: spec.model.as_str().to_owned(),
                tools: Vec::new(),
                pid: 0,
                pgid: 0,
            });
        let (tx, rx) = mpsc::unbounded_channel();
        let session = FakeSession {
            info,
            clock: self.clock.clone(),
            started: self.clock.now(),
            recorder: self.recorder.clone(),
            exit: watch::channel(None).0,
            state: Mutex::new(State {
                steps: script.into(),
                waiting: None,
                queued: VecDeque::new(),
                tx: Some(tx),
                rx: Some(rx),
                usage: UsageDelta::default(),
                closing: false,
                steers: 0,
            }),
        };
        session.run(&mut session.state.lock());
        Ok(Box::new(session))
    }
}

enum Waiting {
    Steer(SteerMatch),
    Abort,
}

struct State {
    steps: VecDeque<Step>,
    waiting: Option<Waiting>,
    queued: VecDeque<String>,
    tx: Option<mpsc::UnboundedSender<HarnessEvent>>,
    rx: Option<mpsc::UnboundedReceiver<HarnessEvent>>,
    usage: UsageDelta,
    closing: bool,
    steers: u64,
}

struct FakeSession {
    info: SessionInfo,
    clock: Arc<dyn Clock>,
    started: OffsetDateTime,
    recorder: Arc<Mutex<Recorder>>,
    exit: watch::Sender<Option<HarnessExit>>,
    state: Mutex<State>,
}

impl FakeSession {
    fn exited(&self) -> bool {
        self.exit.borrow().is_some()
    }

    /// Runs steps until the next await, `Exit`, or the end of the script.
    fn run(&self, st: &mut State) {
        st.waiting = None;
        while let Some(step) = st.steps.pop_front() {
            match step {
                Step::Emit(event) => {
                    if let HarnessEvent::Usage(delta) = &event {
                        st.usage = st.usage.add(delta);
                    }
                    if let Some(tx) = &st.tx {
                        let _ = tx.send(event);
                    }
                }
                Step::AwaitSteer(m) => match st.queued.iter().position(|t| m.matches(t)) {
                    Some(i) => {
                        st.queued.remove(i);
                    }
                    None => {
                        st.waiting = Some(Waiting::Steer(m));
                        return;
                    }
                },
                Step::AwaitAbort => {
                    st.waiting = Some(Waiting::Abort);
                    return;
                }
                Step::Exit(exit) => {
                    self.finish(st, exit);
                    return;
                }
            }
        }
    }

    fn finish(&self, st: &mut State, exit: HarnessExit) {
        st.tx = None;
        st.steps.clear();
        st.waiting = None;
        self.exit.send_replace(Some(exit));
    }

    fn wall(&self) -> Duration {
        Duration::try_from(self.clock.now() - self.started).unwrap_or_default()
    }
}

#[async_trait]
impl HarnessSession for FakeSession {
    fn info(&self) -> &SessionInfo {
        &self.info
    }

    fn events(&self) -> Result<Box<dyn HarnessEventStream>, HarnessError> {
        match self.state.lock().rx.take() {
            Some(rx) => Ok(Box::new(FakeStream { rx })),
            None => Err(HarnessError::Conflict("events() already taken".into())),
        }
    }

    async fn steer(&self, text: String) -> Result<SteerId, HarnessError> {
        self.recorder.lock().calls.push(FakeCall::Steer(text.clone()));
        let mut st = self.state.lock();
        if self.exited() {
            return Err(HarnessError::Closed);
        }
        if st.closing {
            return Err(HarnessError::Conflict("session is closing".into()));
        }
        st.steers += 1;
        let id = SteerId::new(format!("fake-p-{}", st.steers));
        let resumes = matches!(&st.waiting, Some(Waiting::Steer(m)) if m.matches(&text));
        if resumes {
            self.run(&mut st);
        } else {
            st.queued.push_back(text);
        }
        Ok(id)
    }

    async fn abort(&self) -> Result<(), HarnessError> {
        self.recorder.lock().calls.push(FakeCall::Abort);
        let mut st = self.state.lock();
        if self.exited() {
            return Err(HarnessError::Closed);
        }
        if matches!(st.waiting, Some(Waiting::Abort)) {
            self.run(&mut st);
        }
        Ok(())
    }

    async fn close(&self, grace: Duration) -> Result<(), HarnessError> {
        self.recorder.lock().calls.push(FakeCall::Close(grace));
        let mut st = self.state.lock();
        if self.exited() {
            return Ok(());
        }
        st.closing = true;
        let exit = HarnessExit {
            reason: if grace.is_zero() { ExitReason::Killed(crate::ports::harness::KillCause::CloseGraceExpired) } else { ExitReason::Closed },
            exit_code: if grace.is_zero() { None } else { Some(0) },
            signal: if grace.is_zero() { Some(9) } else { None },
            stderr_tail: String::new(),
            stderr_log: None,
            transcript: None,
            usage: st.usage,
            wall: self.wall(),
        };
        self.finish(&mut st, exit);
        Ok(())
    }

    async fn wait(&self) -> HarnessExit {
        let mut rx = self.exit.subscribe();
        // The sender lives in `self`, so this only returns once an exit is published.
        let exit = rx.wait_for(Option::is_some).await.ok().and_then(|v| v.clone());
        exit.unwrap_or_else(|| unreachable!("fake exit sender outlives the session"))
    }
}

struct FakeStream {
    rx: mpsc::UnboundedReceiver<HarnessEvent>,
}

#[async_trait]
impl HarnessEventStream for FakeStream {
    async fn next(&mut self) -> Option<HarnessEvent> {
        self.rx.recv().await
    }
}
