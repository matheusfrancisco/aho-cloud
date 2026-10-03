//! The runtime agent's exec socket, as this service's client.
//!
//! [`RuntimeClient::exec`] opens `GET` [`cloud_api::internal::routes::SANDBOX_EXEC`] as a
//! WebSocket with the service token and sends the [`ExecRequest`]; terminals relay the frames
//! that follow as they are. Sandbox chats wrap the socket as a bridge child ([`SandboxLaunch`]):
//! stdin bytes become `Stdin` frames (EOF = `CloseStdin`), `Stdout` frames become stdout bytes,
//! `Stderr` is logged, `Exit` is the exit status and kill closes the socket.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use cloud_api::exec::{ExecRequest, Frame, MAX_FRAME};
use futures::future::BoxFuture;
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::net::TcpStream;
use tokio::sync::watch;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderValue, header};
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

use crate::spawn::{Launch, Process, STDERR_TAIL_MAX, Spawned, StderrTail};

/// An open exec session on the runtime agent, request already sent.
pub(crate) type ExecSocket = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// How long opening an exec session may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// After a kill closes the socket, how long to wait for the runtime's `Exit`.
const KILL_WAIT: Duration = Duration::from_secs(2);
/// Largest `Stdin` payload one frame carries.
const STDIN_CHUNK: usize = 64 << 10;
/// Buffer of the in-memory stdio pipes between the bridge and the socket.
const PIPE_BUFFER: usize = 64 << 10;

/// Why an exec session could not be opened.
#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error("runtime agent refused the exec session with HTTP {0}")]
    Refused(u16),
    #[error("runtime agent unreachable: {0}")]
    Unreachable(String),
    #[error("runtime agent did not answer within {CONNECT_TIMEOUT:?}")]
    Timeout,
}

/// Reaches the runtime agent's exec socket. `Debug` redacts the token.
pub struct RuntimeClient {
    /// `ws://` or `wss://` base, no trailing `/`.
    base: String,
    service_token: String,
}

impl std::fmt::Debug for RuntimeClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeClient").field("base", &self.base).field("service_token", &"..").finish()
    }
}

impl RuntimeClient {
    /// `runtime_url` is the agent's `http://` or `https://` base URL; `None` if it is neither.
    pub fn new(runtime_url: &str, service_token: String) -> Option<Self> {
        let url = runtime_url.trim_end_matches('/');
        let (scheme, host) = match url.strip_prefix("http://") {
            Some(rest) => ("ws", rest),
            None => ("wss", url.strip_prefix("https://")?),
        };
        (!host.is_empty()).then(|| Self { base: format!("{scheme}://{host}"), service_token })
    }

    /// Opens an exec session in `sandbox` and sends `request` as its first message.
    pub(crate) async fn exec(&self, sandbox: &str, request: &ExecRequest) -> Result<ExecSocket, ExecError> {
        let path = cloud_api::routes::fill(cloud_api::internal::routes::SANDBOX_EXEC, &[("s", sandbox)]);
        let mut req = format!("{}{path}", self.base).into_client_request().map_err(|e| ExecError::Unreachable(e.to_string()))?;
        let bearer = HeaderValue::from_str(&format!("Bearer {}", self.service_token))
            .map_err(|_| ExecError::Unreachable("the service token is not a valid header value".into()))?;
        req.headers_mut().insert(header::AUTHORIZATION, bearer);
        let config = WebSocketConfig::default().max_message_size(Some(MAX_FRAME)).max_frame_size(Some(MAX_FRAME));
        let open = async {
            let (mut ws, _) = tokio_tungstenite::connect_async_with_config(req, Some(config), true).await.map_err(|e| match e {
                tungstenite::Error::Http(resp) => ExecError::Refused(resp.status().as_u16()),
                other => ExecError::Unreachable(other.to_string()),
            })?;
            let json = serde_json::to_string(request).map_err(|e| ExecError::Unreachable(e.to_string()))?;
            ws.send(Message::Text(json.into())).await.map_err(|e| ExecError::Unreachable(e.to_string()))?;
            Ok(ws)
        };
        tokio::time::timeout(CONNECT_TIMEOUT, open).await.map_err(|_| ExecError::Timeout)?
    }
}

/// The aho inside a sandbox, run as a pipe-mode exec session: `[command, --project,
/// /workspace, rpc]`, plus `--session <id>` when resuming.
pub(crate) struct SandboxLaunch {
    pub runtime: Arc<RuntimeClient>,
    pub sandbox: String,
    pub command: String,
}

impl SandboxLaunch {
    /// The exec request for one chat connection.
    pub(crate) fn request(&self, session: Option<&str>) -> ExecRequest {
        let workspace = cloud_api::internal::SANDBOX_WORKSPACE;
        let mut argv: Vec<String> = [self.command.as_str(), "--project", workspace, "rpc"].map(String::from).to_vec();
        if let Some(session) = session {
            argv.extend(["--session".to_owned(), session.to_owned()]);
        }
        ExecRequest { argv, pty: None, env: Default::default(), cwd: Some(workspace.to_owned()) }
    }
}

impl Launch for SandboxLaunch {
    fn launch<'a>(&'a self, session: Option<&'a str>) -> BoxFuture<'a, io::Result<Spawned>> {
        Box::pin(async move {
            let socket = self.runtime.exec(&self.sandbox, &self.request(session)).await.map_err(io::Error::other)?;
            Ok(exec_child(socket, self.sandbox.clone()))
        })
    }
}

/// How an exec session ended: `Ok(code)`, `Ok(None)` if it was killed without reporting one, or
/// why it was lost.
type Status = Result<Option<i32>, String>;

/// An exec session as a bridge child.
fn exec_child(socket: ExecSocket, sandbox: String) -> Spawned {
    let (stdin_ours, stdin_theirs) = tokio::io::duplex(PIPE_BUFFER);
    let (stdout_ours, stdout_theirs) = tokio::io::duplex(PIPE_BUFFER);
    let (status_tx, status) = watch::channel(None);
    let (kill, kill_rx) = watch::channel(false);
    let stderr = StderrLines { sandbox, tail: StderrTail::default(), partial: Vec::new() };
    let tail = stderr.tail.clone();
    tokio::spawn(drive(socket, stdin_ours, stdout_ours, status_tx, kill_rx, stderr));
    Spawned { stdin: Box::pin(stdin_theirs), stdout: Box::pin(stdout_theirs), process: Box::new(ExecProcess { status, kill, tail }) }
}

/// `Stderr` frames split into lines (a line may span frames): each is logged and the last
/// non-blank one kept.
struct StderrLines {
    sandbox: String,
    tail: StderrTail,
    /// Bytes after the last `\n`, at most [`STDERR_TAIL_MAX`] (a longer line is taken as is).
    partial: Vec<u8>,
}

impl StderrLines {
    fn push(&mut self, data: &[u8]) {
        self.partial.extend_from_slice(data);
        while let Some(nl) = self.partial.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.partial.drain(..=nl).collect();
            self.line(&line);
        }
        if self.partial.len() > STDERR_TAIL_MAX {
            self.flush();
        }
    }

    /// The unterminated rest, once the session ended (or it grew too long).
    fn flush(&mut self) {
        let rest = std::mem::take(&mut self.partial);
        self.line(&rest);
    }

    fn line(&self, bytes: &[u8]) {
        let line = String::from_utf8_lossy(bytes);
        let line = line.trim();
        if !line.is_empty() {
            tracing::info!(target: "sandbox", sandbox = %self.sandbox, "{line}");
            self.tail.record(line);
        }
    }
}

/// Dropping it kills the session, like `kill_on_drop` for local children.
struct ExecProcess {
    status: watch::Receiver<Option<Status>>,
    kill: watch::Sender<bool>,
    tail: StderrTail,
}

impl Process for ExecProcess {
    fn wait(&mut self) -> BoxFuture<'_, io::Result<Option<i32>>> {
        Box::pin(async move {
            match self.status.wait_for(Option::is_some).await {
                Ok(done) => match done.as_ref() {
                    Some(Ok(code)) => Ok(*code),
                    Some(Err(why)) => Err(io::Error::other(why.clone())),
                    None => Err(io::Error::other("exec session lost")),
                },
                Err(_) => Err(io::Error::other("exec session driver stopped")),
            }
        })
    }

    fn kill(&mut self) -> io::Result<()> {
        self.kill.send_replace(true);
        Ok(())
    }

    fn stderr_tail(&self) -> Option<String> {
        self.tail.last()
    }
}

/// Resolves once a kill was asked for or the process handle is gone.
async fn killed(kill: &mut watch::Receiver<bool>) {
    let _ = kill.wait_for(|k| *k).await;
}

type Sink = SplitSink<ExecSocket, Message>;
type Stream = SplitStream<ExecSocket>;

/// Owns the socket for the session's lifetime. The status is published before stdout closes
/// (and after the stderr tail is complete), so the bridge never sees EOF without an exit status
/// and its reason to go with it.
async fn drive(
    socket: ExecSocket,
    mut stdin: DuplexStream,
    stdout: DuplexStream,
    status: watch::Sender<Option<Status>>,
    mut kill: watch::Receiver<bool>,
    mut stderr: StderrLines,
) {
    let (mut sink, mut stream) = socket.split();
    let mut stdout = Some(stdout);
    let mut stdin_open = true;
    let mut buf = vec![0; STDIN_CHUNK];
    let mut kill_requested = false;
    let outcome: Status = loop {
        tokio::select! {
            () = killed(&mut kill) => {
                kill_requested = true;
                break Ok(None);
            }
            read = stdin.read(&mut buf), if stdin_open => {
                let frame = match read {
                    Ok(0) | Err(_) => {
                        stdin_open = false;
                        Frame::CloseStdin
                    }
                    Ok(n) => Frame::Stdin(buf[..n].to_vec()),
                };
                if sink.send(Message::Binary(frame.encode().into())).await.is_err() {
                    break Err("the exec session's socket failed".into());
                }
            }
            msg = stream.next() => match msg {
                Some(Ok(Message::Binary(bytes))) => match Frame::decode(&bytes) {
                    Ok(Frame::Stdout(data)) => {
                        let Some(out) = stdout.as_mut() else { continue };
                        tokio::select! {
                            written = out.write_all(&data) => if written.is_err() {
                                // The bridge stopped reading; keep running until it kills us.
                                stdout = None;
                            },
                            () = killed(&mut kill) => {
                                kill_requested = true;
                                break Ok(None);
                            }
                        }
                    }
                    Ok(Frame::Stderr(data)) => stderr.push(&data),
                    Ok(Frame::Exit(code)) => break Ok(Some(code)),
                    Ok(Frame::Error(text)) => break Err(format!("exec session failed: {text}")),
                    Ok(other) => break Err(format!("runtime agent sent a client frame: {other:?}")),
                    Err(e) => break Err(format!("runtime agent sent a bad frame: {e}")),
                },
                Some(Ok(Message::Close(_))) | None => break Err("the exec session ended without an exit status".into()),
                Some(Err(e)) => break Err(format!("the exec session's socket failed: {e}")),
                Some(Ok(_)) => {}
            },
        }
    };
    let outcome = if kill_requested { close_after_kill(&mut sink, &mut stream).await } else { outcome };
    stderr.flush();
    status.send_replace(Some(outcome));
    drop(stdout);
    let _ = sink.close().await;
}

/// Closes the socket (the runtime then ends the process group) and waits briefly for an `Exit`.
async fn close_after_kill(sink: &mut Sink, stream: &mut Stream) -> Status {
    let _ = sink.send(Message::Close(None)).await;
    let code = tokio::time::timeout(KILL_WAIT, async {
        while let Some(Ok(msg)) = stream.next().await {
            if let Message::Binary(bytes) = msg
                && let Ok(Frame::Exit(code)) = Frame::decode(&bytes)
            {
                return Some(code);
            }
        }
        None
    })
    .await
    .ok()
    .flatten();
    Ok(code)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_urls_map_to_websocket_schemes() {
        let ws = |url: &str| RuntimeClient::new(url, "t".into()).map(|c| c.base);
        assert_eq!(ws("http://127.0.0.1:8791/").as_deref(), Some("ws://127.0.0.1:8791"));
        assert_eq!(ws("https://runtime.internal").as_deref(), Some("wss://runtime.internal"));
        for bad in ["ws://x", "ftp://x", "127.0.0.1:8791", "http://", ""] {
            assert!(ws(bad).is_none(), "{bad:?}");
        }
    }
}
