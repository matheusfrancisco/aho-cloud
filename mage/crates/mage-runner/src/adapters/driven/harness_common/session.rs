//! Protocol-independent session runtime shared by `harness_omp` and `harness_claude`: process
//! supervision through `EnvChild`, the stdout reader with limits, stderr capture, the event pump,
//! turn bookkeeping, steer/abort/close/wait, the stall watchdog, and `HarnessExit` assembly
//! (redacted transcript copy included). Each adapter supplies a [`Protocol`].

use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use mage_core::domain::{HarnessKind, RunId};
use parking_lot::Mutex;
use tokio::io::{AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, watch};

use super::lines::{LineError, LineReader, MAX_TOTAL_OUTPUT};
use super::pump::{EventSink, EventStream, pump};
use super::redact::{Redactor, tail};
use super::stderr::StderrCapture;
use crate::ports::executor::{Env, EnvChild, EnvPath, ExecError, Exit, ProcessRole, ReadRange, SpawnSpec};
use crate::ports::harness::{
    ExitReason, HarnessError, HarnessEvent, HarnessEventStream, HarnessExit, HarnessSession, KillCause, LAUNCH_DIR, NoticeLevel,
    PROMPT_MAX_BYTES, SessionInfo, SteerId, TurnOutcome, UsageDelta,
};

/// `EnvChild::terminate` grace after the adapter decided to kill.
pub const TERMINATE_GRACE: Duration = Duration::from_secs(5);
/// Claude: wait after SIGINT before terminating.
pub const INTERRUPT_WAIT: Duration = Duration::from_secs(5);
/// Each handshake command response.
pub const HANDSHAKE_STEP: Duration = Duration::from_secs(10);
/// MCP readiness (inventory complete).
pub const MCP_READINESS: Duration = Duration::from_secs(30);
/// Native transcript copy cap.
pub const TRANSCRIPT_MAX: u64 = 64 * 1024 * 1024;
/// Steer text prefix: a message can never begin with `/` (slash commands, skill prompts).
pub const STEER_PREFIX: &str = "[mage] ";

#[derive(Debug, Clone, Copy)]
pub struct Timing {
    pub stall: Duration,
    pub abort_deadline: Duration,
}

/// Adapter-internal session state (harness.md "Session state machine").
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Starting,
    Busy,
    Idle,
    Aborting,
    Closing,
    Exited,
}

#[derive(Debug)]
pub struct CoreState {
    pub phase: Phase,
    pub turn: u32,
    pub turn_open: bool,
    idle_sent: bool,
    kill: Option<ExitReason>,
    close_requested: bool,
    usage: UsageDelta,
    last_frame: Instant,
    /// Pending deferred `Idle` (Claude: 1.5 s after `result` without new output).
    idle_due: Option<Instant>,
}

/// Where the native transcript lives after exit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranscriptSource {
    Env(EnvPath),
    /// LocalLogin: the developer's config dir on the runner host.
    Host(PathBuf),
}

/// What an adapter plugs into the shared runtime.
#[async_trait]
pub trait Protocol: Send + Sync + 'static {
    /// One stdout line. `Err` is a protocol violation: the process is terminated with
    /// `ExitReason::ProtocolError` and later lines are ignored.
    async fn on_line(&self, core: &SessionCore, line: Vec<u8>) -> Result<(), String>;
    /// Stdout ended (EOF, read error, or limit): release every pending waiter.
    fn on_eof(&self);
    /// Command line delivering a user message (`text` already carries [`STEER_PREFIX`]).
    fn steer_line(&self, core: &SessionCore, id: &str, text: &str) -> Vec<u8>;
    /// Command line cancelling the current turn.
    fn abort_line(&self, id: &str) -> Vec<u8>;
    /// Claude: SIGINT before terminating when an abort is not honored.
    fn interrupt_on_abort_timeout(&self) -> bool {
        false
    }
    /// Native transcript location, resolved once after exit.
    async fn transcript(&self, core: &SessionCore) -> Option<TranscriptSource>;
}

type StdinSlot = tokio::sync::Mutex<Option<Pin<Box<dyn AsyncWrite + Send>>>>;

async fn write_stdin(stdin: &StdinSlot, mut line: Vec<u8>) -> Result<(), HarnessError> {
    line.push(b'\n');
    let mut stdin = stdin.lock().await;
    let Some(w) = stdin.as_mut() else { return Err(HarnessError::Closed) };
    let write = async {
        w.write_all(&line).await?;
        w.flush().await
    };
    match tokio::time::timeout(HANDSHAKE_STEP, write).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => Err(HarnessError::Unavailable(format!("harness stdin: {e}"))),
        Err(_) => Err(HarnessError::Unavailable("harness stdin blocked".into())),
    }
}

enum ProcCmd {
    Terminate(Duration),
    Interrupt,
}

/// Runtime shared by the reader, the finalizer, the watchdog, and the session methods.
pub struct SessionCore {
    pub kind: HarnessKind,
    pub run_id: RunId,
    pub env: Arc<dyn Env>,
    pub redactor: Redactor,
    pub pid: u32,
    pub timing: Timing,
    /// Physical stdout line limit; omp tightens it from `ready`.
    pub max_line: Arc<AtomicUsize>,
    sink: EventSink,
    stream: Mutex<Option<EventStream>>,
    stdin: Arc<StdinSlot>,
    state: Mutex<CoreState>,
    changed: watch::Sender<u64>,
    proc_tx: mpsc::UnboundedSender<ProcCmd>,
    proc_exit: watch::Receiver<Option<Result<Exit, String>>>,
    exit: watch::Sender<Option<HarnessExit>>,
    idle_tx: mpsc::UnboundedSender<Instant>,
    ids: AtomicU64,
    started: Instant,
    stderr: StderrCapture,
    log_dir: PathBuf,
}

pub struct Shared<P: Protocol> {
    pub core: SessionCore,
    pub proto: P,
}

/// Everything `launch` needs besides the Env and the protocol.
pub struct LaunchSpec {
    pub kind: HarnessKind,
    pub run_id: RunId,
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
    pub redactor: Redactor,
    pub log_dir: PathBuf,
    pub timing: Timing,
    pub max_line: usize,
}

/// Spawns the harness through `Env::spawn` (cwd = scratch `launch/`) and starts the reader,
/// stderr capture, supervisor, finalizer, and watchdog tasks.
pub async fn launch<P: Protocol>(env: Arc<dyn Env>, spec: LaunchSpec, proto: P) -> Result<Arc<Shared<P>>, HarnessError> {
    tokio::fs::create_dir_all(&spec.log_dir)
        .await
        .map_err(|e| HarnessError::Unavailable(format!("harness log_dir {}: {e}", spec.log_dir.display())))?;
    let spawn = SpawnSpec { argv: spec.argv, cwd: EnvPath::scratch(LAUNCH_DIR)?, env: spec.env, role: ProcessRole::Harness };
    let mut child = env.spawn(spawn).await?;
    let pipes = (child.take_stdin(), child.take_stdout(), child.take_stderr());
    let (Some(stdin), Some(stdout), Some(stderr)) = pipes else {
        let _ = child.terminate(Duration::ZERO).await;
        return Err(HarnessError::Unavailable("harness process has no stdio pipes".into()));
    };
    let pid = child.pid();
    let (sink, stream) = pump(spec.redactor.clone());
    let stderr_log = spec.log_dir.join(format!("{}.stderr.log", spec.run_id));
    let stderr = StderrCapture::start(stderr, spec.redactor.clone(), Some(stderr_log)).await;
    let (proc_tx, proc_rx) = mpsc::unbounded_channel();
    let (proc_exit_tx, proc_exit) = watch::channel(None);
    tokio::spawn(supervise(child, proc_rx, proc_exit_tx));
    let max_line = Arc::new(AtomicUsize::new(spec.max_line));
    let (idle_tx, idle_rx) = mpsc::unbounded_channel();
    let core = SessionCore {
        kind: spec.kind,
        run_id: spec.run_id,
        env,
        redactor: spec.redactor,
        pid,
        timing: spec.timing,
        max_line: max_line.clone(),
        sink,
        stream: Mutex::new(Some(stream)),
        stdin: Arc::new(tokio::sync::Mutex::new(Some(stdin))),
        state: Mutex::new(CoreState {
            phase: Phase::Starting,
            turn: 0,
            turn_open: false,
            idle_sent: false,
            kill: None,
            close_requested: false,
            usage: UsageDelta::default(),
            last_frame: Instant::now(),
            idle_due: None,
        }),
        changed: watch::channel(0).0,
        proc_tx,
        proc_exit,
        exit: watch::channel(None).0,
        idle_tx,
        ids: AtomicU64::new(0),
        started: Instant::now(),
        stderr,
        log_dir: spec.log_dir,
    };
    let shared = Arc::new(Shared { core, proto });
    let reader = tokio::spawn(read_stdout(shared.clone(), LineReader::new(stdout, max_line, MAX_TOTAL_OUTPUT)));
    tokio::spawn(finalize(shared.clone(), reader));
    tokio::spawn(watchdog(Arc::downgrade(&shared)));
    tokio::spawn(idle_timer(Arc::downgrade(&shared), idle_rx));
    Ok(shared)
}

async fn supervise(
    mut child: Box<dyn EnvChild>,
    mut rx: mpsc::UnboundedReceiver<ProcCmd>,
    exit: watch::Sender<Option<Result<Exit, String>>>,
) {
    let result = loop {
        tokio::select! {
            biased;
            cmd = rx.recv() => match cmd {
                Some(ProcCmd::Interrupt) => {
                    if let Err(e) = child.interrupt() {
                        tracing::warn!(error = %e, "harness interrupt failed");
                    }
                }
                Some(ProcCmd::Terminate(grace)) => break child.terminate(grace).await,
                None => break child.terminate(TERMINATE_GRACE).await,
            },
            r = child.wait() => break r,
        }
    };
    exit.send_replace(Some(result.map_err(|e: ExecError| e.to_string())));
}

async fn read_stdout<P: Protocol>(shared: Arc<Shared<P>>, mut lines: LineReader) {
    let core = &shared.core;
    let mut failed = false;
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                {
                    let mut st = core.state.lock();
                    st.last_frame = Instant::now();
                    st.idle_due = None;
                }
                if failed {
                    continue;
                }
                if let Err(msg) = shared.proto.on_line(core, line).await {
                    tracing::warn!(run_id = %core.run_id, error = %msg, "harness protocol error");
                    failed = true;
                    core.kill(ExitReason::ProtocolError(msg), TERMINATE_GRACE);
                }
            }
            Ok(None) => break,
            Err(LineError::Oversized { limit }) => {
                core.kill(ExitReason::ProtocolError(format!("stdout line longer than {limit} bytes")), TERMINATE_GRACE);
                break;
            }
            Err(LineError::OutputLimit) => {
                core.kill(ExitReason::Killed(KillCause::OutputLimit), TERMINATE_GRACE);
                break;
            }
            Err(LineError::Io(e)) => {
                tracing::debug!(run_id = %core.run_id, error = %e, "harness stdout read failed");
                break;
            }
        }
    }
    shared.proto.on_eof();
    core.bump();
}

/// After stdout ends and the process exits: final notice, end of stream, transcript copy,
/// `HarnessExit`.
async fn finalize<P: Protocol>(shared: Arc<Shared<P>>, mut reader: tokio::task::JoinHandle<()>) {
    let core = &shared.core;
    tokio::select! {
        _ = &mut reader => {}
        _ = core.wait_process() => {
            // A grandchild may hold stdout open; give the reader a moment, then stop it.
            if tokio::time::timeout(Duration::from_secs(2), &mut reader).await.is_err() {
                reader.abort();
                shared.proto.on_eof();
            }
        }
    }
    if tokio::time::timeout(TERMINATE_GRACE, core.wait_process()).await.is_err() {
        // Stdout ended but the process lingers: make sure it is gone.
        let _ = core.proc_tx.send(ProcCmd::Terminate(TERMINATE_GRACE));
    }
    let exit = core.wait_process().await;
    let (stderr_tail, stderr_log) = core.stderr.finish(Duration::from_secs(2)).await;
    let (reason, usage) = {
        let mut st = core.state.lock();
        st.phase = Phase::Exited;
        let reason = match (&st.kill, st.close_requested) {
            (Some(reason), _) => reason.clone(),
            (None, true) => ExitReason::Closed,
            (None, false) => ExitReason::Crashed,
        };
        (reason, st.usage)
    };
    let exit_text = match &exit {
        Ok(e) => e.to_string(),
        Err(e) => format!("wait failed: {e}"),
    };
    let notice = match &reason {
        ExitReason::Closed => None,
        ExitReason::Crashed => Some((
            NoticeLevel::Error,
            format!("harness exited unexpectedly: {exit_text}, stderr: {}", tail(&stderr_tail, 2048).trim()),
        )),
        ExitReason::Killed(KillCause::AbortUnanswered) => None,
        ExitReason::Killed(KillCause::CloseGraceExpired) => {
            Some((NoticeLevel::Warning, "harness did not exit within the close grace; terminated".to_owned()))
        }
        ExitReason::Killed(KillCause::Stalled) => Some((NoticeLevel::Error, "harness stalled; terminated".to_owned())),
        ExitReason::Killed(KillCause::OutputLimit) => {
            Some((NoticeLevel::Error, "harness exceeded the 512 MiB output limit; terminated".to_owned()))
        }
        ExitReason::ProtocolError(m) => Some((NoticeLevel::Error, format!("harness protocol error: {m}; terminated"))),
        ExitReason::IsolationViolation(m) => Some((NoticeLevel::Error, format!("harness isolation violation: {m}; terminated"))),
    };
    if let Some((level, text)) = notice {
        core.sink.emit(HarnessEvent::Notice { level, text });
    }
    core.sink.close();
    core.bump();
    let transcript = match shared.proto.transcript(core).await {
        Some(source) => core.copy_transcript(source).await,
        None => None,
    };
    let (exit_code, signal) = match exit {
        Ok(Exit::Code(c)) => (Some(c), None),
        Ok(Exit::Signal(s)) => (None, Some(s)),
        _ => (None, None),
    };
    core.exit.send_replace(Some(HarnessExit {
        reason,
        exit_code,
        signal,
        stderr_tail,
        stderr_log,
        transcript,
        usage,
        wall: core.started.elapsed(),
    }));
}

/// Stall timer: a turn open with no stdout frame for `stall` → `Notice(Warning)`, abort; an
/// unanswered abort terminates with `KillCause::Stalled`.
async fn watchdog<P: Protocol>(shared: Weak<Shared<P>>) {
    let period = {
        let Some(s) = shared.upgrade() else { return };
        (s.core.timing.stall / 4).clamp(Duration::from_millis(50), Duration::from_secs(5))
    };
    loop {
        tokio::time::sleep(period).await;
        let Some(s) = shared.upgrade() else { return };
        let stalled = {
            let st = s.core.state.lock();
            match st.phase {
                Phase::Exited => return,
                Phase::Busy if st.turn_open => st.last_frame.elapsed() >= s.core.timing.stall,
                _ => false,
            }
        };
        if stalled {
            s.core.emit(HarnessEvent::Notice {
                level: NoticeLevel::Warning,
                text: format!("harness silent for {} s mid-turn; aborting the turn", s.core.timing.stall.as_secs()),
            });
            let _ = s.abort_turn(KillCause::Stalled).await;
        }
    }
}

/// Emits a deferred `Idle` when its deadline passes and nothing cancelled it.
async fn idle_timer<P: Protocol>(shared: Weak<Shared<P>>, mut rx: mpsc::UnboundedReceiver<Instant>) {
    while let Some(due) = rx.recv().await {
        tokio::time::sleep_until(due.into()).await;
        let Some(s) = shared.upgrade() else { return };
        let fire = {
            let mut st = s.core.state.lock();
            let fire = st.idle_due == Some(due);
            if fire {
                st.idle_due = None;
            }
            fire
        };
        if fire {
            s.core.idle();
        }
    }
}

impl SessionCore {
    /// Emits `Idle` after `delay` unless stdout produces another frame or a turn opens first.
    pub fn idle_after(&self, delay: Duration) {
        let due = Instant::now() + delay;
        self.state.lock().idle_due = Some(due);
        let _ = self.idle_tx.send(due);
    }

    /// Adapter-generated command id (`h-N`, `p-N`, `a-N`).
    pub fn next_id(&self, prefix: &str) -> String {
        format!("{prefix}-{}", self.ids.fetch_add(1, Ordering::Relaxed) + 1)
    }

    pub fn phase(&self) -> Phase {
        self.state.lock().phase
    }

    /// `(turn, open)`.
    pub fn turn(&self) -> (u32, bool) {
        let st = self.state.lock();
        (st.turn, st.turn_open)
    }

    pub fn emit(&self, event: HarnessEvent) {
        self.sink.emit(event);
    }

    /// Emits `Ready` and leaves `Starting`.
    pub fn ready(&self, info: SessionInfo) {
        let mut st = self.state.lock();
        if st.phase == Phase::Starting {
            st.phase = Phase::Idle;
        }
        self.sink.emit(HarnessEvent::Ready(info));
    }

    /// Opens a turn unless one is open; emits `TurnStarted`. Returns the open turn.
    pub fn open_turn(&self) -> u32 {
        let mut st = self.state.lock();
        if !st.turn_open && st.phase != Phase::Exited {
            st.turn += 1;
            st.turn_open = true;
            st.idle_sent = false;
            st.idle_due = None;
            if matches!(st.phase, Phase::Idle | Phase::Starting) {
                st.phase = Phase::Busy;
            }
            self.sink.emit(HarnessEvent::TurnStarted { turn: st.turn });
        }
        st.turn
    }

    /// Closes the open turn (no-op without one); emits `TurnEnded`.
    pub fn end_turn(&self, outcome: TurnOutcome) -> bool {
        let ended = {
            let mut st = self.state.lock();
            if !st.turn_open {
                false
            } else {
                st.turn_open = false;
                if matches!(st.phase, Phase::Busy | Phase::Aborting) {
                    st.phase = Phase::Idle;
                }
                self.sink.emit(HarnessEvent::TurnEnded { turn: st.turn, outcome });
                true
            }
        };
        if ended {
            self.bump();
        }
        ended
    }

    /// Emits `Idle` once per quiet stretch, only when no turn is open.
    pub fn idle(&self) {
        let mut st = self.state.lock();
        if !st.turn_open && !st.idle_sent && st.phase != Phase::Exited {
            st.idle_sent = true;
            self.sink.emit(HarnessEvent::Idle);
        }
    }

    pub fn add_usage(&self, delta: UsageDelta) {
        let mut st = self.state.lock();
        st.usage = st.usage.add(&delta);
        self.sink.emit(HarnessEvent::Usage(delta));
    }

    /// True while the adapter is aborting the open turn (Claude maps the turn's `result` to
    /// `Aborted`).
    pub fn aborting(&self) -> bool {
        self.state.lock().phase == Phase::Aborting
    }

    /// Writes one JSONL command (a `\n` is appended).
    pub async fn write_line(&self, line: Vec<u8>) -> Result<(), HarnessError> {
        write_stdin(&self.stdin, line).await
    }

    /// Writes a reply from the reader without blocking it.
    pub fn write_detached(&self, line: Vec<u8>) {
        let stdin = self.stdin.clone();
        tokio::spawn(async move {
            if let Err(e) = write_stdin(&stdin, line).await {
                tracing::debug!(error = %e, "harness reply not delivered");
            }
        });
    }

    async fn close_stdin(&self) {
        if let Some(mut w) = self.stdin.lock().await.take() {
            let _ = w.shutdown().await;
        }
    }

    /// Records why the adapter terminates the process (first reason wins) and terminates it.
    pub fn kill(&self, reason: ExitReason, grace: Duration) {
        {
            let mut st = self.state.lock();
            if st.kill.is_none() && st.phase != Phase::Exited {
                st.kill = Some(reason);
            }
        }
        let _ = self.proc_tx.send(ProcCmd::Terminate(grace));
    }

    fn interrupt(&self) {
        let _ = self.proc_tx.send(ProcCmd::Interrupt);
    }

    pub fn exited(&self) -> bool {
        self.proc_exit.borrow().is_some()
    }

    /// Resolves when the process has exited.
    pub async fn wait_process(&self) -> Result<Exit, String> {
        let mut rx = self.proc_exit.clone();
        match rx.wait_for(Option::is_some).await {
            Ok(v) => v.clone().unwrap_or_else(|| Err("no exit".into())),
            Err(_) => Err("supervisor gone".into()),
        }
    }

    /// Redacted stderr tail so far (for error messages).
    pub fn stderr_tail(&self) -> String {
        tail(&self.stderr.tail(), 2048).trim().to_owned()
    }

    fn bump(&self) {
        self.changed.send_modify(|n| *n += 1);
    }

    /// Waits until turn `turn` is no longer open (or the process exited), up to `max`.
    async fn wait_turn_closed(&self, turn: u32, max: Duration) -> bool {
        let mut rx = self.changed.subscribe();
        let closed = || {
            let st = self.state.lock();
            !st.turn_open || st.turn != turn || st.phase == Phase::Exited
        };
        tokio::time::timeout(max, async {
            loop {
                if closed() || self.exited() {
                    return;
                }
                tokio::select! {
                    r = rx.changed() => if r.is_err() { return },
                    _ = tokio::time::sleep(Duration::from_millis(200)) => {}
                }
            }
        })
        .await
        .is_ok()
    }

    async fn copy_transcript(&self, source: TranscriptSource) -> Option<PathBuf> {
        let bytes = match &source {
            TranscriptSource::Env(path) => match self.env.read(path, ReadRange::Whole { max_bytes: TRANSCRIPT_MAX }).await {
                Ok(r) => r.bytes,
                Err(e) => {
                    tracing::warn!(run_id = %self.run_id, path = %path, error = %e, "harness transcript not readable");
                    return None;
                }
            },
            TranscriptSource::Host(path) => match tokio::fs::read(path).await {
                Ok(mut b) => {
                    b.truncate(TRANSCRIPT_MAX as usize);
                    b
                }
                Err(e) => {
                    tracing::warn!(run_id = %self.run_id, path = %path.display(), error = %e, "harness transcript not readable");
                    return None;
                }
            },
        };
        let dest = self.log_dir.join(format!("{}.transcript.jsonl", self.run_id));
        match tokio::fs::write(&dest, self.redactor.redact_bytes(&bytes)).await {
            Ok(()) => Some(dest),
            Err(e) => {
                tracing::warn!(run_id = %self.run_id, error = %e, "cannot write transcript copy");
                None
            }
        }
    }
}

impl<P: Protocol> Shared<P> {
    /// Startup failure: terminate, wait for the process, return `err`.
    pub async fn fail_start(&self, reason: ExitReason, err: HarnessError) -> HarnessError {
        self.core.kill(reason, Duration::ZERO);
        let _ = self.core.wait_process().await;
        err
    }

    /// Startup failure caused by the process (timeout, exit, protocol): the error explains it.
    pub async fn fail_unavailable(&self, what: &str) -> HarnessError {
        let protocol = match &self.core.state.lock().kill {
            Some(ExitReason::ProtocolError(m)) => Some(m.clone()),
            _ => None,
        };
        let err = match protocol {
            Some(m) => HarnessError::Protocol(m),
            None => {
                let exited = self.core.exited();
                HarnessError::Unavailable(format!(
                    "{what}{}; stderr: {}",
                    if exited { " (harness exited)" } else { "" },
                    self.core.stderr_tail()
                ))
            }
        };
        self.fail_start(ExitReason::Killed(KillCause::CloseGraceExpired), err).await
    }

    async fn abort_turn(self: &Arc<Self>, cause: KillCause) -> Result<(), HarnessError> {
        let core = &self.core;
        let turn = {
            let mut st = core.state.lock();
            if st.phase == Phase::Exited {
                return Err(HarnessError::Closed);
            }
            if !st.turn_open {
                return Ok(());
            }
            if st.phase != Phase::Closing {
                st.phase = Phase::Aborting;
            }
            st.turn
        };
        let id = core.next_id("a");
        core.write_line(self.proto.abort_line(&id)).await?;
        if core.wait_turn_closed(turn, core.timing.abort_deadline).await {
            return Ok(());
        }
        core.emit(HarnessEvent::Notice {
            level: NoticeLevel::Error,
            text: format!("harness did not end the turn within {} s of abort; terminating", core.timing.abort_deadline.as_secs()),
        });
        if self.proto.interrupt_on_abort_timeout() {
            core.interrupt();
            let _ = tokio::time::timeout(INTERRUPT_WAIT, core.wait_process()).await;
        }
        core.kill(ExitReason::Killed(cause), TERMINATE_GRACE);
        let _ = core.wait_process().await;
        Err(HarnessError::Unavailable("harness did not honor abort; session terminated".into()))
    }
}

/// `HarnessSession` over the shared runtime.
pub struct Session<P: Protocol> {
    shared: Arc<Shared<P>>,
    info: SessionInfo,
}

impl<P: Protocol> Session<P> {
    pub fn new(shared: Arc<Shared<P>>, info: SessionInfo) -> Self {
        Self { shared, info }
    }
}

impl<P: Protocol> Drop for Session<P> {
    /// A session dropped without `close` must not leave the process running.
    fn drop(&mut self) {
        let core = &self.shared.core;
        if !core.exited() && !core.state.lock().close_requested {
            core.kill(ExitReason::Killed(KillCause::CloseGraceExpired), TERMINATE_GRACE);
        }
    }
}

#[async_trait]
impl<P: Protocol> HarnessSession for Session<P> {
    fn info(&self) -> &SessionInfo {
        &self.info
    }

    fn events(&self) -> Result<Box<dyn HarnessEventStream>, HarnessError> {
        match self.shared.core.stream.lock().take() {
            Some(stream) => Ok(Box::new(stream)),
            None => Err(HarnessError::Conflict("events() already taken".into())),
        }
    }

    async fn steer(&self, text: String) -> Result<SteerId, HarnessError> {
        let core = &self.shared.core;
        if text.len() > PROMPT_MAX_BYTES {
            return Err(HarnessError::Invalid(format!("steer text exceeds {PROMPT_MAX_BYTES} bytes")));
        }
        match core.phase() {
            Phase::Closing => return Err(HarnessError::Conflict("session is closing".into())),
            Phase::Exited => return Err(HarnessError::Closed),
            _ if core.exited() => return Err(HarnessError::Closed),
            _ => {}
        }
        let id = core.next_id("p");
        let text = format!("{STEER_PREFIX}{text}");
        core.open_turn();
        let line = self.shared.proto.steer_line(core, &id, &text);
        core.write_line(line).await?;
        Ok(SteerId::new(id))
    }

    async fn abort(&self) -> Result<(), HarnessError> {
        self.shared.abort_turn(KillCause::AbortUnanswered).await
    }

    async fn close(&self, grace: Duration) -> Result<(), HarnessError> {
        let core = &self.shared.core;
        let (first, turn_open) = {
            let mut st = core.state.lock();
            if st.phase == Phase::Exited {
                return Ok(());
            }
            let first = !st.close_requested;
            st.close_requested = true;
            st.phase = Phase::Closing;
            (first, st.turn_open)
        };
        if grace.is_zero() {
            core.kill(ExitReason::Killed(KillCause::CloseGraceExpired), Duration::ZERO);
            let _ = core.wait_process().await;
            return Ok(());
        }
        if first {
            if turn_open {
                let id = core.next_id("a");
                let _ = core.write_line(self.shared.proto.abort_line(&id)).await;
            }
            core.close_stdin().await;
        }
        if tokio::time::timeout(grace, core.wait_process()).await.is_err() {
            core.kill(ExitReason::Killed(KillCause::CloseGraceExpired), TERMINATE_GRACE);
            let _ = core.wait_process().await;
        }
        Ok(())
    }

    async fn wait(&self) -> HarnessExit {
        let mut rx = self.shared.core.exit.subscribe();
        // The sender lives in `self.shared`, so `wait_for` only returns once the finalizer has
        // published the exit.
        let exit = rx.wait_for(Option::is_some).await.ok().and_then(|v| v.clone());
        exit.unwrap_or_else(|| unreachable!("harness exit sender outlives the session"))
    }
}
