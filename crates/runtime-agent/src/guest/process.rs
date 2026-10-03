//! Process supervision inside the guest: a SIGCHLD-driven reaper (as PID 1 it also collects
//! orphans), and a runner that keeps the app process alive with exponential restart backoff.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use nix::errno::Errno;
use nix::sys::signal::{Signal, killpg};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;
use parking_lot::Mutex;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{mpsc, oneshot, watch};

use super::control::{Control, ProcessState};

/// Reaping pass interval in case a SIGCHLD is coalesced away.
const BACKSTOP: Duration = Duration::from_millis(250);

/// Which children the reaper collects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReapMode {
    /// PID 1: every child, including orphans reparented to init.
    All,
    /// Only children spawned through the reaper (safe inside a larger process).
    Own,
}

/// Collects exited children and hands exit codes to whoever spawned them.
pub struct Reaper {
    mode: ReapMode,
    waiters: Mutex<HashMap<i32, oneshot::Sender<i32>>>,
}

/// Exit code of a finished child: its status, or `128 + signal`.
fn exit_of(status: WaitStatus) -> Option<(i32, i32)> {
    match status {
        WaitStatus::Exited(pid, code) => Some((pid.as_raw(), code)),
        WaitStatus::Signaled(pid, sig, _) => Some((pid.as_raw(), 128 + sig as i32)),
        _ => None,
    }
}

impl Reaper {
    /// Starts reaping on every SIGCHLD. Must be called inside a tokio runtime; the background
    /// task ends when the last `Arc` is dropped.
    pub fn start(mode: ReapMode) -> io::Result<Arc<Reaper>> {
        let mut sigchld = signal(SignalKind::child())?;
        let reaper = Arc::new(Reaper { mode, waiters: Mutex::new(HashMap::new()) });
        let weak = Arc::downgrade(&reaper);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = sigchld.recv() => {}
                    () = tokio::time::sleep(BACKSTOP) => {}
                }
                match weak.upgrade() {
                    Some(reaper) => reaper.reap(),
                    None => return,
                }
            }
        });
        Ok(reaper)
    }

    /// Spawns `cmd` as the leader of a new process group; the receiver yields its exit code.
    pub fn spawn(&self, cmd: &mut Command) -> io::Result<(u32, oneshot::Receiver<i32>)> {
        cmd.process_group(0);
        self.spawn_child(|| cmd.spawn()).map(|(child, exited)| (child.id(), exited))
    }

    /// Runs `spawn` (which starts exactly one child) so the child is registered before it can
    /// be reaped; the receiver yields its exit code. The returned handle is for its stdio and
    /// pid only: this reaper collects the child, nobody may wait on it.
    pub fn spawn_child(&self, spawn: impl FnOnce() -> io::Result<Child>) -> io::Result<(Child, oneshot::Receiver<i32>)> {
        // Holding the lock across spawn means the child cannot be reaped before it is registered.
        let mut waiters = self.waiters.lock();
        let child = spawn()?;
        let raw = i32::try_from(child.id()).map_err(io::Error::other)?;
        let (tx, rx) = oneshot::channel();
        waiters.insert(raw, tx);
        Ok((child, rx))
    }

    /// One non-blocking pass: collects every exited child this reaper is responsible for.
    pub fn reap(&self) {
        let mut waiters = self.waiters.lock();
        let mut exited: Vec<(i32, i32)> = Vec::new();
        match self.mode {
            ReapMode::All => loop {
                match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
                    Ok(WaitStatus::StillAlive) => break,
                    Ok(status) => exited.extend(exit_of(status)),
                    Err(Errno::EINTR) => {}
                    Err(_) => break,
                }
            },
            ReapMode::Own => {
                for &pid in waiters.keys() {
                    match waitpid(Pid::from_raw(pid), Some(WaitPidFlag::WNOHANG)) {
                        Ok(status) => exited.extend(exit_of(status)),
                        // Collected by someone else: the status is gone, but never hang the waiter.
                        Err(Errno::ECHILD) => exited.push((pid, -1)),
                        Err(_) => {}
                    }
                }
            }
        }
        for (pid, code) in exited {
            if let Some(tx) = waiters.remove(&pid) {
                let _ = tx.send(code);
            }
        }
    }
}

/// Restart delays: `initial`, doubling per consecutive quick failure, capped at `max`; a run
/// that lasted `reset_after` or longer starts the sequence over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Backoff {
    pub initial: Duration,
    pub max: Duration,
    pub reset_after: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Backoff { initial: Duration::from_millis(250), max: Duration::from_secs(30), reset_after: Duration::from_secs(30) }
    }
}

impl Backoff {
    /// Delay before the restart that follows the `failures`-th consecutive quick exit (1-based).
    pub fn delay(&self, failures: u32) -> Duration {
        let exp = failures.saturating_sub(1).min(31);
        self.initial.saturating_mul(1u32 << exp).min(self.max)
    }
}

/// How to exec the app process (and health commands, which share its environment).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunSpec {
    pub argv: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub cwd: Option<PathBuf>,
    /// `(uid, gid)` to drop to.
    pub user: Option<(u32, u32)>,
}

impl RunSpec {
    fn command(&self, argv: &[String], output: fn() -> Stdio) -> Command {
        let mut cmd = Command::new(&argv[0]);
        cmd.args(&argv[1..]).env_clear().envs(&self.env).stdin(Stdio::null()).stdout(output()).stderr(output());
        if let Some(cwd) = &self.cwd {
            cmd.current_dir(cwd);
        }
        if let Some((uid, gid)) = self.user {
            cmd.uid(uid).gid(gid);
        }
        cmd
    }
}

#[derive(Clone, Debug)]
pub struct RunnerConfig {
    pub spec: RunSpec,
    pub backoff: Backoff,
    /// Time between SIGTERM and SIGKILL when stopping the process.
    pub stop_grace: Duration,
}

enum Cmd {
    Restart(oneshot::Sender<()>),
    Stop(oneshot::Sender<()>),
}

/// Keeps one process running. Clones share it; dropping the last clone stops the process.
#[derive(Clone)]
pub struct Runner {
    reaper: Arc<Reaper>,
    spec: Arc<RunSpec>,
    state: watch::Receiver<ProcessState>,
    cmds: mpsc::Sender<Cmd>,
}

/// SIGTERM to the process group, SIGKILL after `grace`; the exit code.
async fn terminate(pid: u32, mut exited: oneshot::Receiver<i32>, grace: Duration) -> i32 {
    let group = Pid::from_raw(i32::try_from(pid).unwrap_or(i32::MAX));
    let _ = killpg(group, Signal::SIGTERM);
    match tokio::time::timeout(grace, &mut exited).await {
        Ok(code) => code.unwrap_or(-1),
        Err(_) => {
            let _ = killpg(group, Signal::SIGKILL);
            exited.await.unwrap_or(-1)
        }
    }
}

async fn supervise(
    reaper: Arc<Reaper>,
    spec: Arc<RunSpec>,
    cfg: RunnerConfig,
    state: watch::Sender<ProcessState>,
    mut cmds: mpsc::Receiver<Cmd>,
) {
    let mut failures: u32 = 0;
    loop {
        let started = Instant::now();
        let spawned = reaper.spawn(&mut spec.command(&spec.argv, Stdio::inherit));
        state.send_modify(|s| {
            s.starts += 1;
            s.pid = spawned.as_ref().ok().map(|(pid, _)| *pid);
        });
        let code = match spawned {
            Err(e) => {
                tracing::warn!(argv = ?spec.argv, error = %e, "cannot start process");
                127
            }
            Ok((pid, mut exited)) => {
                tokio::select! {
                    code = &mut exited => code.unwrap_or(-1),
                    cmd = cmds.recv() => {
                        let code = terminate(pid, exited, cfg.stop_grace).await;
                        state.send_modify(|s| {
                            s.pid = None;
                            s.last_exit = Some(code);
                        });
                        match cmd {
                            Some(Cmd::Restart(ack)) => {
                                failures = 0;
                                let _ = ack.send(());
                                continue;
                            }
                            Some(Cmd::Stop(ack)) => {
                                let _ = ack.send(());
                                return;
                            }
                            None => return,
                        }
                    }
                }
            }
        };
        tracing::info!(code, "process exited");
        state.send_modify(|s| {
            s.pid = None;
            s.last_exit = Some(code);
        });
        if started.elapsed() >= cfg.backoff.reset_after {
            failures = 0;
        }
        failures = failures.saturating_add(1);
        tokio::select! {
            () = tokio::time::sleep(cfg.backoff.delay(failures)) => {}
            cmd = cmds.recv() => match cmd {
                Some(Cmd::Restart(ack)) => {
                    failures = 0;
                    let _ = ack.send(());
                }
                Some(Cmd::Stop(ack)) => {
                    let _ = ack.send(());
                    return;
                }
                None => return,
            },
        }
    }
}

impl Runner {
    /// Starts supervising `cfg.spec` now.
    pub fn start(reaper: Arc<Reaper>, cfg: RunnerConfig) -> Runner {
        let spec = Arc::new(cfg.spec.clone());
        let (state_tx, state) = watch::channel(ProcessState::default());
        let (cmds, cmd_rx) = mpsc::channel(4);
        tokio::spawn(supervise(reaper.clone(), spec.clone(), cfg, state_tx, cmd_rx));
        Runner { reaper, spec, state, cmds }
    }

    pub fn state(&self) -> ProcessState {
        self.state.borrow().clone()
    }

    /// Changes to [`Runner::state`], for observers.
    pub fn watch(&self) -> watch::Receiver<ProcessState> {
        self.state.clone()
    }

    /// Runs `argv` once with the process's environment, cwd and user; its exit code.
    pub async fn run_once(&self, argv: &[String], timeout: Duration) -> Result<i32, String> {
        let Some(program) = argv.first() else {
            return Err("empty argv".into());
        };
        let (pid, exited) = self.reaper.spawn(&mut self.spec.command(argv, Stdio::null)).map_err(|e| format!("{program}: {e}"))?;
        match tokio::time::timeout(timeout, exited).await {
            Ok(code) => Ok(code.unwrap_or(-1)),
            Err(_) => {
                let _ = killpg(Pid::from_raw(i32::try_from(pid).unwrap_or(i32::MAX)), Signal::SIGKILL);
                Err(format!("{program}: timed out after {timeout:?}"))
            }
        }
    }

    /// Stops the process and starts it again immediately, resetting the backoff.
    pub async fn restart(&self) -> Result<(), String> {
        let (ack, done) = oneshot::channel();
        self.cmds.send(Cmd::Restart(ack)).await.map_err(|_| "runner stopped".to_owned())?;
        done.await.map_err(|_| "runner stopped".to_owned())
    }

    /// Stops the process for good (SIGTERM, then SIGKILL after the grace period).
    pub async fn stop(&self) {
        let (ack, done) = oneshot::channel();
        if self.cmds.send(Cmd::Stop(ack)).await.is_ok() {
            let _ = done.await;
        }
    }
}

impl Control for Runner {
    fn health(&self, argv: Vec<String>, timeout: Duration) -> BoxFuture<'_, Result<i32, String>> {
        Box::pin(async move { self.run_once(&argv, timeout).await })
    }

    fn restart(&self) -> BoxFuture<'_, Result<(), String>> {
        Box::pin(Runner::restart(self))
    }

    fn status(&self) -> ProcessState {
        self.state()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str) -> RunSpec {
        RunSpec {
            argv: vec!["sh".into(), "-c".into(), script.into()],
            env: BTreeMap::from([("PATH".into(), "/usr/bin:/bin".into()), ("FOO".into(), "bar".into())]),
            cwd: None,
            user: None,
        }
    }

    fn runner(script: &str, backoff: Backoff) -> Runner {
        let reaper = Reaper::start(ReapMode::Own).unwrap();
        Runner::start(reaper, RunnerConfig { spec: sh(script), backoff, stop_grace: Duration::from_secs(2) })
    }

    /// Waits for `n` starts; the instant each was observed.
    async fn starts(watch: &mut watch::Receiver<ProcessState>, n: u32) -> Vec<Instant> {
        let mut seen = Vec::new();
        let mut last = 0;
        tokio::time::timeout(Duration::from_secs(10), async {
            while (seen.len() as u32) < n {
                let starts = watch.borrow_and_update().starts;
                if starts > last {
                    last = starts;
                    seen.push(Instant::now());
                    continue;
                }
                watch.changed().await.unwrap();
            }
        })
        .await
        .unwrap();
        seen
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let b = Backoff { initial: Duration::from_millis(100), max: Duration::from_millis(700), reset_after: Duration::ZERO };
        let delays: Vec<u128> = (1..=6).map(|n| b.delay(n).as_millis()).collect();
        assert_eq!(delays, [100, 200, 400, 700, 700, 700]);
        assert_eq!(b.delay(u32::MAX), Duration::from_millis(700));
    }

    #[tokio::test]
    async fn crashing_child_restarts_with_growing_delays() {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("runs");
        let backoff =
            Backoff { initial: Duration::from_millis(100), max: Duration::from_millis(400), reset_after: Duration::from_secs(60) };
        let r = runner(&format!("echo run >> '{}'; exit 1", log.display()), backoff);
        let mut watch = r.watch();
        let at = starts(&mut watch, 5).await;
        let gaps: Vec<Duration> = at.windows(2).map(|w| w[1] - w[0]).collect();
        let slack = Duration::from_millis(20);
        // Lower bounds hold under any load: each restart waited at least its delay.
        for (gap, delay) in gaps.iter().zip([100, 200, 400, 400]) {
            assert!(*gap + slack >= Duration::from_millis(delay), "gaps {gaps:?}");
        }
        // Uncapped, the fourth delay would be 800 ms.
        assert!(gaps[3] < Duration::from_millis(800), "cap exceeded: {gaps:?}");
        assert_eq!(r.state().last_exit, Some(1));
        let runs = std::fs::read_to_string(&log).unwrap().lines().count();
        assert!(runs >= 4, "children did not run: {runs}");
    }

    #[tokio::test]
    async fn a_long_run_resets_the_backoff() {
        let backoff = Backoff { initial: Duration::from_millis(300), max: Duration::from_secs(5), reset_after: Duration::from_millis(150) };
        let r = runner("sleep 0.2; exit 2", backoff);
        let mut watch = r.watch();
        let at = starts(&mut watch, 4).await;
        for w in at.windows(2) {
            // 200 ms run + 300 ms delay every time. Without the reset the second and third gaps
            // would be 200 + 600 and 200 + 1200 ms.
            assert!(w[1] - w[0] < Duration::from_millis(800), "{:?}", w[1] - w[0]);
        }
        assert_eq!(r.state().last_exit, Some(2));
    }

    #[tokio::test]
    async fn restart_terminates_the_group_and_starts_again() {
        let r = runner("exec sleep 30", Backoff::default());
        let mut watch = r.watch();
        starts(&mut watch, 1).await;
        let first = r.state().pid.unwrap();
        r.restart().await.unwrap();
        tokio::time::timeout(Duration::from_secs(5), watch.wait_for(|s| s.starts == 2 && s.pid.is_some())).await.unwrap().unwrap();
        let state = r.state();
        assert_eq!(state.starts, 2);
        assert_eq!(state.last_exit, Some(128 + 15));
        assert_ne!(state.pid.unwrap(), first);
        let gone = nix::sys::signal::kill(Pid::from_raw(first as i32), None);
        assert_eq!(gone, Err(Errno::ESRCH));
    }

    #[tokio::test]
    async fn run_once_shares_env_and_reports_exit_codes_and_timeouts() {
        let r = runner("exec sleep 30", Backoff::default());
        let sh = |s: &str| vec!["sh".to_owned(), "-c".to_owned(), s.to_owned()];
        assert_eq!(r.run_once(&sh("exit 3"), Duration::from_secs(5)).await, Ok(3));
        assert_eq!(r.run_once(&sh("test \"$FOO\" = bar"), Duration::from_secs(5)).await, Ok(0));
        assert_eq!(r.run_once(&sh("kill -9 $$"), Duration::from_secs(5)).await, Ok(128 + 9));
        let slow = r.run_once(&sh("sleep 5"), Duration::from_millis(100)).await.unwrap_err();
        assert!(slow.contains("timed out"), "{slow}");
        let missing = r.run_once(&["/nonexistent/probe".to_owned()], Duration::from_secs(1)).await.unwrap_err();
        assert!(missing.starts_with("/nonexistent/probe"), "{missing}");
    }

    #[tokio::test]
    async fn dropping_the_runner_stops_the_process() {
        let r = runner("exec sleep 30", Backoff::default());
        let mut watch = r.watch();
        starts(&mut watch, 1).await;
        let pid = r.state().pid.unwrap();
        drop(r);
        let dead = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if watch.borrow().last_exit.is_some() {
                    return;
                }
                if watch.changed().await.is_err() {
                    return;
                }
            }
        })
        .await;
        assert!(dead.is_ok());
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(nix::sys::signal::kill(Pid::from_raw(pid as i32), None), Err(Errno::ESRCH));
    }
}
