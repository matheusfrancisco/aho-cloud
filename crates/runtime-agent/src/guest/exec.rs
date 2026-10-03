//! The exec server: one command per session, its stdio (or a terminal) carried as
//! [`cloud_api::exec`] frames, length-prefixed, over any byte stream (a vsock connection in a
//! microVM, the stdin/stdout of `docker exec -i <c> cloud-guest exec` in a container,
//! `tokio::io::duplex` in tests).
//!
//! A session opens with a [`Frame::Request`]. The command is spawned (a failure answers
//! [`Frame::Error`] and ends the session) as the leader of its own process group, or of its own
//! session on a fresh PTY when the request asks for one. Stdin, resize and close-stdin frames
//! are applied as they arrive, output streams back, and [`Frame::Exit`] (`128 + signal` when
//! killed) ends the session. The host closing the stream ends it too: the process group gets
//! SIGHUP, then SIGKILL after [`ExecContext::hangup_grace`].
//!
//! [`read_frame`], [`write_frame`] and [`collect`] are the host side of the same framing.

use std::collections::BTreeMap;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::task::{Context, Poll, ready};
use std::time::Duration;

use cloud_api::exec::{ExecRequest, Frame, FrameError, PtySize};
use nix::errno::Errno;
use nix::fcntl::{FcntlArg, OFlag, fcntl};
use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use tokio::io::unix::AsyncFd;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;

use super::process::Reaper;

/// Most output bytes per Stdout/Stderr frame.
const CHUNK: usize = 32 * 1024;
/// How long output is still forwarded after the process exited: background children may keep
/// the terminal or the pipes open forever.
const DRAIN: Duration = Duration::from_millis(500);
/// How long a SIGKILLed group gets to be reaped before the session gives up on it.
const KILL_WAIT: Duration = Duration::from_secs(5);
/// What close-stdin writes to a terminal: its default EOF character (`^D`).
const VEOF: u8 = 0x04;

/// What every session inherits.
#[derive(Clone, Debug)]
pub struct ExecContext {
    /// The guest's environment; the request's `env` is added on top.
    pub env: BTreeMap<String, String>,
    /// Working directory when the request names none (`None`: the server's own).
    pub cwd: Option<PathBuf>,
    /// `(uid, gid)` sessions run as; `None` runs them as the server's user.
    pub user: Option<(u32, u32)>,
    /// Time between SIGHUP and SIGKILL when the host goes away.
    pub hangup_grace: Duration,
}

impl ExecContext {
    /// Sessions as this process: its environment, its working directory, its user.
    pub fn inherit(hangup_grace: Duration) -> Self {
        let env = std::env::vars_os().filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?))).collect();
        ExecContext { env, cwd: None, user: None, hangup_grace }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("exec stream: {0}")]
    Io(#[from] io::Error),
    #[error(transparent)]
    Frame(#[from] FrameError),
    /// The peer broke the protocol.
    #[error("{0}")]
    Protocol(String),
    /// The guest answered [`Frame::Error`] (e.g. the command could not be spawned).
    #[error("the guest refused: {0}")]
    Refused(String),
}

/// Reads one length-prefixed frame; `None` when the stream ends cleanly between frames. Not
/// cancel-safe.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<Frame>, SessionError> {
    let mut prefix = [0u8; 4];
    if r.read(&mut prefix[..1]).await? == 0 {
        return Ok(None);
    }
    r.read_exact(&mut prefix[1..]).await?;
    let len = Frame::prefixed_len(prefix)?;
    let mut body = vec![0; len];
    r.read_exact(&mut body).await?;
    Ok(Some(Frame::decode(&body)?))
}

/// Writes one length-prefixed frame and flushes.
pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, frame: &Frame) -> io::Result<()> {
    w.write_all(&frame.encode_prefixed()).await?;
    w.flush().await
}

/// The frame's kind, for messages.
pub fn frame_name(frame: &Frame) -> &'static str {
    match frame {
        Frame::Stdin(_) => "stdin",
        Frame::Stdout(_) => "stdout",
        Frame::Stderr(_) => "stderr",
        Frame::Resize(_) => "resize",
        Frame::CloseStdin => "close-stdin",
        Frame::Exit(_) => "exit",
        Frame::Error(_) => "error",
        Frame::Request(_) => "request",
    }
}

/// Serves one session on `conn` (see the module docs). Children are spawned and collected
/// through `reaper`. What went wrong inside the session was reported to the host as a frame
/// before the error is returned.
pub async fn serve<S>(conn: S, reaper: &Reaper, ctx: &ExecContext) -> Result<(), SessionError>
where
    S: AsyncRead + AsyncWrite + Send + 'static,
{
    let (mut host_rx, mut host_tx) = tokio::io::split(conn);
    let request = match read_frame(&mut host_rx).await {
        Ok(Some(Frame::Request(r))) => r,
        Ok(None) => return Ok(()),
        Ok(Some(other)) => {
            let message = format!("a session opens with a request frame, not {}", frame_name(&other));
            return refuse(&mut host_tx, message).await;
        }
        Err(SessionError::Io(e)) => return Err(e.into()),
        Err(e) => return refuse(&mut host_tx, e.to_string()).await,
    };
    let session = match spawn(&request, reaper, ctx) {
        Ok(s) => s,
        Err(message) => {
            write_frame(&mut host_tx, &Frame::Error(message)).await?;
            return Ok(());
        }
    };
    session.run(host_rx, host_tx, ctx.hangup_grace).await
}

async fn refuse<W: AsyncWrite + Unpin>(w: &mut W, message: String) -> Result<(), SessionError> {
    write_frame(w, &Frame::Error(message.clone())).await?;
    Err(SessionError::Protocol(message))
}

/// A spawned session.
struct Session {
    pid: u32,
    exited: oneshot::Receiver<i32>,
    input: Input,
    /// `(is_stderr, reader)`.
    outputs: Vec<(bool, Box<dyn AsyncRead + Send + Unpin>)>,
}

enum Input {
    Pty { master: PtyMaster, pty: pty_process::blocking::Pty },
    Pipe(Option<tokio::process::ChildStdin>),
}

impl Input {
    async fn write(&mut self, bytes: &[u8]) {
        // A process that stopped reading its input is not an error of the session.
        match self {
            Input::Pty { master, .. } => {
                let _ = master.write_all(bytes).await;
            }
            Input::Pipe(stdin) => {
                if let Some(pipe) = stdin
                    && pipe.write_all(bytes).await.is_err()
                {
                    *stdin = None;
                }
            }
        }
    }

    fn resize(&self, size: PtySize) {
        if let Input::Pty { pty, .. } = self {
            let _ = pty.resize(pty_process::Size::new(size.rows, size.cols));
        }
    }

    async fn close(&mut self) {
        match self {
            Input::Pty { master, .. } => {
                let _ = master.write_all(&[VEOF]).await;
            }
            Input::Pipe(stdin) => {
                if let Some(mut pipe) = stdin.take() {
                    let _ = pipe.shutdown().await;
                }
            }
        }
    }
}

/// Spawns the request's command; the error is the message for the host.
fn spawn(request: &ExecRequest, reaper: &Reaper, ctx: &ExecContext) -> Result<Session, String> {
    let Some(program) = request.argv.first() else {
        return Err("empty argv".into());
    };
    let mut env = ctx.env.clone();
    env.extend(request.env.iter().map(|(k, v)| (k.clone(), v.clone())));
    let cwd = request.cwd.as_ref().map(PathBuf::from).or_else(|| ctx.cwd.clone());
    let failed = |e: io::Error| format!("{program}: {e}");
    match request.pty {
        Some(size) => spawn_pty(&request.argv, env, cwd, size, reaper, ctx).map_err(failed),
        None => spawn_pipes(&request.argv, env, cwd, reaper, ctx).map_err(failed),
    }
}

fn pty_err(e: pty_process::Error) -> io::Error {
    match e {
        pty_process::Error::Io(e) => e,
        other => io::Error::other(other.to_string()),
    }
}

fn spawn_pty(
    argv: &[String],
    env: BTreeMap<String, String>,
    cwd: Option<PathBuf>,
    size: PtySize,
    reaper: &Reaper,
    ctx: &ExecContext,
) -> io::Result<Session> {
    let (pty, pts) = pty_process::blocking::open().map_err(pty_err)?;
    pty.resize(pty_process::Size::new(size.rows, size.cols)).map_err(pty_err)?;
    let mut cmd = pty_process::blocking::Command::new(&argv[0]).args(&argv[1..]).env_clear().envs(env);
    if let Some(cwd) = cwd {
        cmd = cmd.current_dir(cwd);
    }
    if let Some((uid, gid)) = ctx.user {
        // The terminal belongs to whoever uses it (programs that reopen it, `mesg`, …).
        if let Err(e) = std::os::unix::fs::fchown(&pts, Some(uid), Some(gid)) {
            tracing::debug!(error = %e, "cannot hand the terminal to the session user");
        }
        cmd = cmd.uid(uid).gid(gid);
    }
    let master = PtyMaster::new(std::os::fd::AsFd::as_fd(&pty).try_clone_to_owned()?)?;
    // The child becomes the leader of a new session (and so of a new process group) on `pts`.
    let (child, exited) = reaper.spawn_child(|| cmd.spawn(pts).map_err(pty_err))?;
    Ok(Session { pid: child.id(), exited, outputs: vec![(false, Box::new(master.clone()))], input: Input::Pty { master, pty } })
}

fn spawn_pipes(
    argv: &[String],
    env: BTreeMap<String, String>,
    cwd: Option<PathBuf>,
    reaper: &Reaper,
    ctx: &ExecContext,
) -> io::Result<Session> {
    let mut cmd = Command::new(&argv[0]);
    cmd.args(&argv[1..]).env_clear().envs(env).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0);
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    if let Some((uid, gid)) = ctx.user {
        cmd.uid(uid).gid(gid);
    }
    let (mut child, exited) = reaper.spawn_child(|| cmd.spawn())?;
    let stdin = child.stdin.take().map(tokio::process::ChildStdin::from_std).transpose()?;
    let mut outputs: Vec<(bool, Box<dyn AsyncRead + Send + Unpin>)> = Vec::new();
    if let Some(out) = child.stdout.take() {
        outputs.push((false, Box::new(tokio::process::ChildStdout::from_std(out)?)));
    }
    if let Some(err) = child.stderr.take() {
        outputs.push((true, Box::new(tokio::process::ChildStderr::from_std(err)?)));
    }
    Ok(Session { pid: child.id(), exited, input: Input::Pipe(stdin), outputs })
}

/// How a running session ended.
enum Outcome {
    Exited(i32),
    /// The host closed the stream (`None`) or broke the protocol (the message).
    HostGone(Option<String>),
}

impl Session {
    async fn run<R, W>(self, host_rx: R, mut host_tx: W, grace: Duration) -> Result<(), SessionError>
    where
        R: AsyncRead + Send + Unpin + 'static,
        W: AsyncWrite + Unpin,
    {
        let Session { pid, mut exited, input, outputs } = self;
        let (out_tx, mut out_rx) = mpsc::channel::<Frame>(8);
        let mut pumps = JoinSet::new();
        for (stderr, reader) in outputs {
            pumps.spawn(pump(reader, stderr, out_tx.clone()));
        }
        drop(out_tx);
        // Input runs on its own task: a process that does not read its stdin must never stop
        // its output from flowing (or its exit from being noticed).
        let (gone_tx, mut gone_rx) = oneshot::channel();
        let feeder = tokio::spawn(async move {
            let _ = gone_tx.send(feed(host_rx, input).await);
        });

        let mut output_open = true;
        let outcome = loop {
            tokio::select! {
                code = &mut exited => break Outcome::Exited(code.unwrap_or(-1)),
                why = &mut gone_rx => break Outcome::HostGone(why.unwrap_or(None)),
                frame = out_rx.recv(), if output_open => match frame {
                    Some(frame) => {
                        if write_frame(&mut host_tx, &frame).await.is_err() {
                            break Outcome::HostGone(None);
                        }
                    }
                    None => output_open = false,
                },
            }
        };
        let group = Pid::from_raw(i32::try_from(pid).unwrap_or(i32::MAX));
        match outcome {
            Outcome::Exited(code) => {
                let drained = tokio::time::timeout(DRAIN, async {
                    while let Some(frame) = out_rx.recv().await {
                        write_frame(&mut host_tx, &frame).await?;
                    }
                    Ok::<_, io::Error>(())
                })
                .await;
                feeder.abort();
                let _ = feeder.await;
                pumps.shutdown().await;
                if let Ok(Err(e)) = drained {
                    return Err(e.into());
                }
                write_frame(&mut host_tx, &Frame::Exit(code)).await?;
                let _ = host_tx.shutdown().await;
                Ok(())
            }
            Outcome::HostGone(why) => {
                if let Some(message) = &why {
                    let _ = write_frame(&mut host_tx, &Frame::Error(message.clone())).await;
                }
                feeder.abort();
                let _ = feeder.await;
                pumps.shutdown().await;
                hang_up(group, exited, grace).await;
                match why {
                    Some(message) => Err(SessionError::Protocol(message)),
                    None => Ok(()),
                }
            }
        }
    }
}

/// Forwards one output stream as frames until it ends.
async fn pump(mut reader: Box<dyn AsyncRead + Send + Unpin>, stderr: bool, out: mpsc::Sender<Frame>) {
    let mut buf = vec![0; CHUNK];
    loop {
        let n = match reader.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        let bytes = buf[..n].to_vec();
        let frame = if stderr { Frame::Stderr(bytes) } else { Frame::Stdout(bytes) };
        if out.send(frame).await.is_err() {
            return;
        }
    }
}

/// Applies host frames until the host is gone; `Some` when it broke the protocol.
async fn feed<R: AsyncRead + Unpin>(mut host: R, mut input: Input) -> Option<String> {
    loop {
        let frame = match read_frame(&mut host).await {
            Ok(Some(frame)) => frame,
            Ok(None) | Err(SessionError::Io(_)) => return None,
            Err(e) => return Some(e.to_string()),
        };
        match frame {
            Frame::Stdin(bytes) => input.write(&bytes).await,
            Frame::Resize(size) => input.resize(size),
            Frame::CloseStdin => input.close().await,
            other => return Some(format!("unexpected {} frame from the host", frame_name(&other))),
        }
    }
}

/// SIGHUP to the group, SIGKILL after `grace`.
async fn hang_up(group: Pid, mut exited: oneshot::Receiver<i32>, grace: Duration) {
    let _ = killpg(group, Signal::SIGHUP);
    if tokio::time::timeout(grace, &mut exited).await.is_err() {
        let _ = killpg(group, Signal::SIGKILL);
        if tokio::time::timeout(KILL_WAIT, exited).await.is_err() {
            tracing::warn!(pid = group.as_raw(), "session process survived SIGKILL");
        }
    }
}

/// The controlling side of a PTY, non-blocking on the reactor. Reads end (`Ok(0)`) once every
/// terminal end is closed.
#[derive(Clone)]
struct PtyMaster(Arc<AsyncFd<OwnedFd>>);

impl PtyMaster {
    fn new(fd: OwnedFd) -> io::Result<Self> {
        let flags = OFlag::from_bits_truncate(fcntl(&fd, FcntlArg::F_GETFL)?);
        fcntl(&fd, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))?;
        Ok(PtyMaster(Arc::new(AsyncFd::new(fd)?)))
    }
}

impl AsyncRead for PtyMaster {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        loop {
            let mut guard = ready!(self.0.poll_read_ready(cx))?;
            let unfilled = buf.initialize_unfilled();
            let read = guard.try_io(|fd| match nix::unistd::read(fd.get_ref(), unfilled) {
                Ok(n) => Ok(n),
                // Linux reports a terminal whose every other end is closed as EIO.
                Err(Errno::EIO) => Ok(0),
                Err(e) => Err(io::Error::from(e)),
            });
            match read {
                Ok(Ok(n)) => {
                    buf.advance(n);
                    return Poll::Ready(Ok(()));
                }
                Ok(Err(e)) => return Poll::Ready(Err(e)),
                Err(_would_block) => {}
            }
        }
    }
}

impl AsyncWrite for PtyMaster {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &[u8]) -> Poll<io::Result<usize>> {
        loop {
            let mut guard = ready!(self.0.poll_write_ready(cx))?;
            match guard.try_io(|fd| nix::unistd::write(fd.get_ref(), buf).map_err(io::Error::from)) {
                Ok(result) => return Poll::Ready(result),
                Err(_would_block) => {}
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// What a command printed and how it ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Collected {
    pub code: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// The host side of one non-interactive session: runs `request` over `conn` with its stdin
/// closed and collects its output. More than `max_output` bytes on either stream is an error
/// (dropping the stream then ends the process).
pub async fn collect<S: AsyncRead + AsyncWrite + Unpin>(
    conn: S,
    request: ExecRequest,
    max_output: usize,
) -> Result<Collected, SessionError> {
    let (mut rx, mut tx) = tokio::io::split(conn);
    write_frame(&mut tx, &Frame::Request(request)).await?;
    write_frame(&mut tx, &Frame::CloseStdin).await?;
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    loop {
        let (sink, bytes, name) = match read_frame(&mut rx).await? {
            Some(Frame::Stdout(bytes)) => (&mut stdout, bytes, "stdout"),
            Some(Frame::Stderr(bytes)) => (&mut stderr, bytes, "stderr"),
            Some(Frame::Exit(code)) => return Ok(Collected { code, stdout, stderr }),
            Some(Frame::Error(message)) => return Err(SessionError::Refused(message)),
            Some(other) => return Err(SessionError::Protocol(format!("unexpected {} frame from the guest", frame_name(&other)))),
            None => return Err(SessionError::Protocol("the guest ended the session without an exit status".into())),
        };
        if sink.len() + bytes.len() > max_output {
            return Err(SessionError::Protocol(format!("more than {max_output} bytes on {name}")));
        }
        sink.extend_from_slice(&bytes);
    }
}
