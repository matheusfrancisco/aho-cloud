//! The WebSocket ↔ child stdio pump for one rpc connection.
//!
//! * Client → child: every text message must be one JSON object on one line, at most
//!   `max_frame_bytes`, and the first must be method `hello`. It is written to the child's stdin
//!   followed by `\n`. The child is spawned only once a valid `hello` arrived.
//! * Child → client: every non-blank stdout line becomes one text message. The first `hello_ok`
//!   teaches the workspace its session id, persisted before the frame is forwarded.
//! * Teardown: client close, frame violation or idle timeout closes the child's stdin; the child
//!   gets `kill_grace` to exit and is then killed. When the child exits first, its remaining output
//!   is forwarded and the socket closed: 1000 on exit code 0, 1011 otherwise. The connection's
//!   limit slot is released only once the child is gone.

use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message, Utf8Bytes, WebSocket, close_code};
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use serde::de::IgnoredAny;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::{Instant, sleep_until, timeout};

use crate::limits::{Denied, Limits, Permit};
use crate::spawn::{CommandTemplate, Process, Spawned, Spawner};
use crate::workspace::{Workspace, is_session_id};

/// How long to wait for the client's close reply after sending ours.
const CLOSE_WAIT: Duration = Duration::from_secs(2);
/// Longest reason a close frame can carry, in bytes.
const MAX_REASON: usize = 123;

type WsSink = SplitSink<WebSocket, Message>;
type WsStream = SplitStream<WebSocket>;

/// Why a client message was refused; the connection is then closed with [`FrameError::close_code`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FrameError {
    #[error("frame is {len} bytes; the limit is {max}")]
    TooLarge { len: usize, max: usize },
    #[error("frame contains a line break; send exactly one JSON object per message")]
    MultiLine,
    #[error("frame is not a JSON object")]
    NotObject,
    #[error("the first frame must be method `hello`")]
    FirstNotHello,
    #[error("binary frames are not accepted; send JSON text")]
    Binary,
}

impl FrameError {
    /// 1009 for size, 1003 for binary, 1008 (policy) for the rest.
    pub fn close_code(&self) -> u16 {
        match self {
            Self::TooLarge { .. } => close_code::SIZE,
            Self::Binary => close_code::UNSUPPORTED,
            Self::MultiLine | Self::NotObject | Self::FirstNotHello => close_code::POLICY,
        }
    }

    fn farewell(&self) -> Farewell {
        Farewell::Close { error: Some(self.to_string()), code: self.close_code(), reason: self.to_string() }
    }
}

/// The rules for one client text message; `first` additionally requires method `hello`.
pub fn check_client_frame(text: &str, first: bool, max_bytes: usize) -> Result<(), FrameError> {
    if text.len() > max_bytes {
        return Err(FrameError::TooLarge { len: text.len(), max: max_bytes });
    }
    if text.bytes().any(|b| b == b'\n' || b == b'\r') {
        return Err(FrameError::MultiLine);
    }
    if !text.trim_start().starts_with('{') || serde_json::from_str::<IgnoredAny>(text).is_err() {
        return Err(FrameError::NotObject);
    }
    if first {
        #[derive(Deserialize)]
        struct Head {
            #[serde(default)]
            method: Option<String>,
        }
        match serde_json::from_str::<Head>(text) {
            Ok(Head { method: Some(m) }) if m == "hello" => {}
            _ => return Err(FrameError::FirstNotHello),
        }
    }
    Ok(())
}

/// Why child output could not be forwarded.
#[derive(Debug, thiserror::Error)]
pub(crate) enum LineError {
    #[error("session process wrote a line over {max} bytes")]
    TooLong { max: usize },
    #[error("session process wrote non-UTF-8 output")]
    NotUtf8,
    #[error("reading session process output: {0}")]
    Io(#[from] io::Error),
}

/// Bounded NDJSON line reader: strips `\n`/`\r\n`, skips blank lines, refuses lines over `max`.
pub(crate) struct LineReader<R> {
    inner: BufReader<R>,
    max: usize,
}

impl<R: AsyncRead + Unpin> LineReader<R> {
    pub(crate) fn new(inner: R, max: usize) -> Self {
        Self { inner: BufReader::new(inner), max }
    }

    /// The next non-blank line; `None` at end of input. A final line without `\n` still counts.
    pub(crate) async fn next_line(&mut self) -> Result<Option<String>, LineError> {
        let limit = u64::try_from(self.max).unwrap_or(u64::MAX).saturating_add(1);
        loop {
            let mut buf = Vec::new();
            if (&mut self.inner).take(limit).read_until(b'\n', &mut buf).await? == 0 {
                return Ok(None);
            }
            if buf.last() == Some(&b'\n') {
                buf.pop();
                if buf.last() == Some(&b'\r') {
                    buf.pop();
                }
            }
            if buf.len() > self.max {
                return Err(LineError::TooLong { max: self.max });
            }
            if buf.iter().all(u8::is_ascii_whitespace) {
                continue;
            }
            return String::from_utf8(buf).map(Some).map_err(|_| LineError::NotUtf8);
        }
    }
}

/// Last client activity, shared by both pump directions.
#[derive(Clone)]
struct Activity {
    start: Instant,
    last_ms: Arc<AtomicU64>,
}

impl Activity {
    fn new() -> Self {
        Self { start: Instant::now(), last_ms: Arc::new(AtomicU64::new(0)) }
    }

    fn touch(&self) {
        let ms = u64::try_from(self.start.elapsed().as_millis()).unwrap_or(u64::MAX);
        self.last_ms.store(ms, Ordering::Relaxed);
    }

    fn deadline(&self, idle: Duration) -> Instant {
        self.start + Duration::from_millis(self.last_ms.load(Ordering::Relaxed)) + idle
    }
}

/// How the server ends the socket.
#[derive(Debug)]
enum Farewell {
    /// The transport is already gone.
    Silent,
    /// The client sent a close; complete the handshake.
    Echo,
    /// Optionally an aho `error` event, then a close frame.
    Close { error: Option<String>, code: u16, reason: String },
}

fn idle_farewell() -> Farewell {
    Farewell::Close { error: None, code: close_code::AWAY, reason: "idle timeout".into() }
}

fn child_farewell(status: &io::Result<Option<i32>>) -> Farewell {
    let (code, reason) = match status {
        Ok(Some(0)) => (close_code::NORMAL, "session process exited".to_owned()),
        Ok(Some(n)) => (close_code::ERROR, format!("session process exited with code {n}")),
        Ok(None) => (close_code::ERROR, "session process was terminated by a signal".to_owned()),
        Err(_) => (close_code::ERROR, "session process was lost".to_owned()),
    };
    Farewell::Close { error: None, code, reason }
}

/// An aho `error` event, so clients that only read frames learn why the socket closes.
fn error_event(message: &str) -> String {
    serde_json::json!({ "v": 1, "reply_to": null, "event": "error", "payload": { "message": message } }).to_string()
}

fn truncate(s: &str, max: usize) -> &str {
    let mut end = s.len().min(max);
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

async fn say_farewell(mut sink: WsSink, stream: Option<WsStream>, farewell: Farewell) {
    match farewell {
        Farewell::Silent => {}
        Farewell::Echo => {
            let _ = sink.close().await;
        }
        Farewell::Close { error, code, reason } => {
            if let Some(message) = error {
                let _ = sink.send(Message::Text(error_event(&message).into())).await;
            }
            let frame = CloseFrame { code, reason: truncate(&reason, MAX_REASON).into() };
            if sink.send(Message::Close(Some(frame))).await.is_err() {
                return;
            }
            if let Some(mut stream) = stream {
                let _ = timeout(CLOSE_WAIT, async {
                    while let Some(Ok(msg)) = stream.next().await {
                        if matches!(msg, Message::Close(_)) {
                            break;
                        }
                    }
                })
                .await;
            }
        }
    }
}

/// Closes a connection the limits refused (1013, try again later).
pub(crate) async fn refuse(socket: WebSocket, denied: Denied) {
    let (sink, stream) = socket.split();
    let message = denied.to_string();
    say_farewell(sink, Some(stream), Farewell::Close { error: Some(message.clone()), code: close_code::AGAIN, reason: message }).await;
}

/// How the client side of a connection ended.
#[derive(Debug)]
enum ClientEnd {
    Closed,
    Gone,
    Violation(FrameError),
    /// Writing to the child failed: it stopped reading, so its exit is what ends the connection.
    StdinClosed,
    /// The bridge stopped the reader.
    Stopped,
}

impl ClientEnd {
    fn farewell(&self) -> Farewell {
        match self {
            Self::Closed => Farewell::Echo,
            Self::Violation(e) => e.farewell(),
            Self::Gone | Self::StdinClosed | Self::Stopped => Farewell::Silent,
        }
    }
}

async fn first_frame(stream: &mut WsStream, max: usize) -> Result<Utf8Bytes, ClientEnd> {
    loop {
        match stream.next().await {
            None | Some(Err(_)) => return Err(ClientEnd::Gone),
            Some(Ok(Message::Text(text))) => {
                return check_client_frame(&text, true, max).map(|()| text).map_err(ClientEnd::Violation);
            }
            Some(Ok(Message::Binary(_))) => return Err(ClientEnd::Violation(FrameError::Binary)),
            Some(Ok(Message::Close(_))) => return Err(ClientEnd::Closed),
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
        }
    }
}

async fn write_line<W: AsyncWrite + Unpin + ?Sized>(w: &mut W, text: &str) -> io::Result<()> {
    w.write_all(text.as_bytes()).await?;
    w.write_all(b"\n").await?;
    w.flush().await
}

/// Client → child. Returns the stream (for the close handshake) and why it stopped; dropping
/// `stdin` on return is the child's EOF.
async fn client_to_child(
    mut stream: WsStream,
    mut stdin: Pin<Box<dyn AsyncWrite + Send>>,
    hello: Utf8Bytes,
    mut stop: oneshot::Receiver<()>,
    activity: Activity,
    max: usize,
) -> (WsStream, ClientEnd) {
    let mut pending = Some(hello);
    let end = loop {
        let text = match pending.take() {
            Some(text) => text,
            None => {
                let msg = tokio::select! {
                    _ = &mut stop => break ClientEnd::Stopped,
                    msg = stream.next() => msg,
                };
                activity.touch();
                match msg {
                    None | Some(Err(_)) => break ClientEnd::Gone,
                    Some(Ok(Message::Text(text))) => match check_client_frame(&text, false, max) {
                        Ok(()) => text,
                        Err(e) => break ClientEnd::Violation(e),
                    },
                    Some(Ok(Message::Binary(_))) => break ClientEnd::Violation(FrameError::Binary),
                    Some(Ok(Message::Close(_))) => break ClientEnd::Closed,
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => continue,
                }
            }
        };
        tokio::select! {
            _ = &mut stop => break ClientEnd::Stopped,
            written = write_line(&mut stdin, &text) => if written.is_err() {
                break ClientEnd::StdinClosed;
            },
        }
    };
    drop(stdin);
    (stream, end)
}

/// Child stdout → channel, in its own task so a slow socket never blocks the child's writes to
/// stdin and vice versa.
fn stdout_pump(stdout: Pin<Box<dyn AsyncRead + Send>>, max: usize) -> (mpsc::Receiver<Result<String, LineError>>, JoinHandle<()>) {
    let (tx, rx) = mpsc::channel(32);
    let task = tokio::spawn(async move {
        let mut reader = LineReader::new(stdout, max);
        while let Some(item) = reader.next_line().await.transpose() {
            let failed = item.is_err();
            if tx.send(item).await.is_err() || failed {
                return;
            }
        }
    });
    (rx, task)
}

/// Child → client, plus the `hello_ok` probe.
struct Downstream<'a> {
    sink: WsSink,
    workspace: &'a Workspace,
    activity: &'a Activity,
    hello_seen: bool,
}

impl Downstream<'_> {
    async fn forward(&mut self, line: String) -> Result<(), axum::Error> {
        self.activity.touch();
        if !self.hello_seen {
            self.hello_seen = self.learn_session(&line).await;
        }
        self.sink.send(Message::Text(line.into())).await
    }

    /// `true` once `line` is the `hello_ok`; its session id is persisted before it is forwarded.
    async fn learn_session(&self, line: &str) -> bool {
        let Ok(frame) = serde_json::from_str::<Value>(line) else { return false };
        if frame.get("event").and_then(Value::as_str) != Some("hello_ok") {
            return false;
        }
        let id = &self.workspace.id;
        match frame.pointer("/payload/session").and_then(Value::as_str) {
            Some(session) if is_session_id(session) => match self.workspace.set_session_id(session).await {
                Ok(()) => tracing::info!(workspace = %id, session, "session id recorded"),
                Err(e) => tracing::error!(workspace = %id, session, "could not persist the session id: {e}"),
            },
            other => tracing::warn!(workspace = %id, session = ?other, "hello_ok without a usable session id"),
        }
        true
    }
}

/// Why the pump loop stopped.
#[derive(Debug)]
enum Outcome {
    Client,
    Exited,
    StdoutClosed,
    BadOutput(LineError),
    SendFailed,
    Idle,
    ReaderPanicked,
}

async fn stop_child(process: &mut dyn Process, grace: Duration) -> io::Result<Option<i32>> {
    if let Ok(status) = timeout(grace, process.wait()).await {
        return status;
    }
    if let Err(e) = process.kill() {
        tracing::debug!("kill after grace: {e}");
    }
    process.wait().await
}

/// Everything one admitted connection needs.
pub(crate) struct Conn {
    pub workspace: Arc<Workspace>,
    pub user: String,
    pub spawner: Arc<dyn Spawner>,
    pub template: Arc<CommandTemplate>,
    pub limits: Limits,
    pub permit: Permit,
}

/// Runs one connection to completion.
pub(crate) async fn run(socket: WebSocket, conn: Conn) {
    let Conn { workspace, user, spawner, template, limits, permit } = conn;
    let (sink, mut stream) = socket.split();

    let hello = match timeout(limits.idle_timeout, first_frame(&mut stream, limits.max_frame_bytes)).await {
        Ok(Ok(text)) => text,
        Ok(Err(end)) => {
            drop(permit);
            say_farewell(sink, Some(stream), end.farewell()).await;
            return;
        }
        Err(_) => {
            drop(permit);
            say_farewell(sink, Some(stream), idle_farewell()).await;
            return;
        }
    };

    let session = workspace.session_id().await;
    let spec = template.render(&workspace.project_dir.to_string_lossy(), session.as_deref(), &workspace.id);
    let Spawned { stdin, stdout, mut process } = match spawner.spawn(&spec) {
        Ok(spawned) => spawned,
        Err(e) => {
            tracing::error!(workspace = %workspace.id, program = %spec.program.display(), "spawn failed: {e}");
            drop(permit);
            let message = "could not start the session process".to_owned();
            say_farewell(sink, Some(stream), Farewell::Close { error: Some(message.clone()), code: close_code::ERROR, reason: message })
                .await;
            return;
        }
    };
    tracing::info!(workspace = %workspace.id, %user, resume = session.as_deref().unwrap_or("new"), "session process started");

    let activity = Activity::new();
    let (stop, stop_rx) = oneshot::channel();
    let mut reader = tokio::spawn(client_to_child(stream, stdin, hello, stop_rx, activity.clone(), limits.max_frame_bytes));
    let (mut lines, pump) = stdout_pump(stdout, limits.max_line_bytes);
    let mut down = Downstream { sink, workspace: &workspace, activity: &activity, hello_seen: false };
    let mut client: Option<(WsStream, ClientEnd)> = None;
    let mut exit_status = None;

    let outcome = {
        let mut exit = process.wait();
        let idle = sleep_until(activity.deadline(limits.idle_timeout));
        tokio::pin!(idle);
        loop {
            tokio::select! {
                biased;
                joined = &mut reader, if client.is_none() => match joined {
                    Ok((stream, ClientEnd::StdinClosed)) => client = Some((stream, ClientEnd::StdinClosed)),
                    Ok(ended) => {
                        client = Some(ended);
                        break Outcome::Client;
                    }
                    Err(e) => {
                        tracing::error!(workspace = %workspace.id, "client reader failed: {e}");
                        break Outcome::ReaderPanicked;
                    }
                },
                line = lines.recv() => match line {
                    Some(Ok(line)) => if down.forward(line).await.is_err() {
                        break Outcome::SendFailed;
                    },
                    Some(Err(e)) => break Outcome::BadOutput(e),
                    None => break Outcome::StdoutClosed,
                },
                status = &mut exit => {
                    exit_status = Some(status);
                    break Outcome::Exited;
                }
                () = &mut idle => {
                    let deadline = activity.deadline(limits.idle_timeout);
                    if deadline <= Instant::now() {
                        break Outcome::Idle;
                    }
                    idle.as_mut().reset(deadline);
                }
            }
        }
    };

    if matches!(outcome, Outcome::Exited) {
        // Output written just before exit is still in the pipe.
        let _ = timeout(limits.kill_grace, async {
            while let Some(Ok(line)) = lines.recv().await {
                if down.forward(line).await.is_err() {
                    break;
                }
            }
        })
        .await;
    }

    // Stop reading the client; its task drops stdin, which is the child's EOF.
    let _ = stop.send(());
    if client.is_none() && !matches!(outcome, Outcome::ReaderPanicked) {
        match reader.await {
            Ok(ended) => client = Some(ended),
            Err(e) => tracing::error!(workspace = %workspace.id, "client reader failed: {e}"),
        }
    }
    let status = match exit_status {
        Some(status) => status,
        None => stop_child(process.as_mut(), limits.kill_grace).await,
    };
    pump.abort();
    drop(permit);
    tracing::info!(workspace = %workspace.id, ?outcome, ?status, "rpc connection closed");

    let farewell = match (&outcome, &client) {
        (Outcome::Client, Some((_, end))) => end.farewell(),
        (Outcome::Exited | Outcome::StdoutClosed, _) => child_farewell(&status),
        (Outcome::BadOutput(e), _) => Farewell::Close { error: Some(e.to_string()), code: close_code::ERROR, reason: e.to_string() },
        (Outcome::Idle, _) => idle_farewell(),
        (Outcome::ReaderPanicked, _) => Farewell::Close { error: None, code: close_code::ERROR, reason: "internal error".into() },
        (Outcome::Client, None) | (Outcome::SendFailed, _) => Farewell::Silent,
    };
    say_farewell(down.sink, client.map(|(stream, _)| stream), farewell).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_size_limit_is_inclusive() {
        let pad = |n: usize| {
            let head = r#"{"v":1,"method":"hello","params":""#;
            format!("{head}{}\"}}", "a".repeat(n - head.len() - 2))
        };
        assert_eq!(pad(100).len(), 100);
        assert_eq!(check_client_frame(&pad(100), true, 100), Ok(()));
        assert_eq!(check_client_frame(&pad(101), true, 100), Err(FrameError::TooLarge { len: 101, max: 100 }));
    }

    #[test]
    fn line_breaks_are_refused_even_inside_valid_json() {
        for text in ["{\"method\":\n\"hello\"}", "{\"method\":\"hello\"}\n", "{\"method\":\"hello\"}\r"] {
            assert_eq!(check_client_frame(text, true, 1024), Err(FrameError::MultiLine), "{text:?}");
        }
    }

    #[test]
    fn only_json_objects_pass() {
        for text in ["[1,2]", "\"hello\"", "42", "null", "{\"a\":1", "{\"a\":1} {}", "", "  "] {
            assert_eq!(check_client_frame(text, false, 1024), Err(FrameError::NotObject), "{text:?}");
        }
        assert_eq!(check_client_frame(" {\"v\":1,\"id\":3,\"method\":\"session.abort\"} ", false, 1024), Ok(()));
    }

    #[test]
    fn the_first_frame_must_be_hello() {
        for text in [r#"{"v":1,"method":"session.prompt"}"#, r#"{"v":1}"#, r#"{"method":7}"#, r#"{"method":"HELLO"}"#] {
            assert_eq!(check_client_frame(text, true, 1024), Err(FrameError::FirstNotHello), "{text:?}");
        }
        assert_eq!(check_client_frame(r#"{"v":1,"id":0,"method":"hello","params":{"min":1,"max":1}}"#, true, 1024), Ok(()));
        assert_eq!(check_client_frame(r#"{"v":1,"method":"session.prompt"}"#, false, 1024), Ok(()));
    }

    #[tokio::test]
    async fn line_reader_splits_skips_blanks_and_keeps_a_final_partial_line() {
        let mut r = LineReader::new(&b"a\n\n  \nb\r\nc"[..], 64);
        assert_eq!(r.next_line().await.unwrap().as_deref(), Some("a"));
        assert_eq!(r.next_line().await.unwrap().as_deref(), Some("b"));
        assert_eq!(r.next_line().await.unwrap().as_deref(), Some("c"));
        assert_eq!(r.next_line().await.unwrap(), None);
    }

    #[tokio::test]
    async fn line_reader_bounds_line_length_exactly() {
        let mut ok = LineReader::new(&b"abcd\nabcd"[..], 4);
        assert_eq!(ok.next_line().await.unwrap().as_deref(), Some("abcd"));
        assert_eq!(ok.next_line().await.unwrap().as_deref(), Some("abcd"));
        let mut long = LineReader::new(&b"abcde\n"[..], 4);
        assert!(matches!(long.next_line().await, Err(LineError::TooLong { max: 4 })));
        let mut bad = LineReader::new(&b"\xff\xfe\n"[..], 4);
        assert!(matches!(bad.next_line().await, Err(LineError::NotUtf8)));
    }

    #[test]
    fn close_reasons_are_cut_on_a_char_boundary() {
        let reason = "é".repeat(100);
        let cut = truncate(&reason, MAX_REASON);
        assert!(cut.len() <= MAX_REASON && cut.len() == 122);
        assert_eq!(truncate("short", MAX_REASON), "short");
    }
}
