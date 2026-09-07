//! The host ↔ guest control protocol: one JSON object per line in each direction, one response
//! per request, over any byte stream (vsock in production, `tokio::io::duplex` in tests).
//!
//! ```text
//! → {"op":"health","argv":["pg_isready"],"timeout_ms":2000}   ← {"ok":true,"exit_code":0}
//! → {"op":"restart"}                                            ← {"ok":true}
//! → {"op":"status"}                                             ← {"ok":true,"status":{…}}
//! ```

use std::io;
use std::time::Duration;

use futures::future::BoxFuture;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};

/// Longest request or response line accepted.
pub const MAX_LINE: usize = 64 * 1024;
/// Health-command timeout when the request names none.
pub const DEFAULT_HEALTH_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    /// Run `argv` in the process's environment; answer its exit code.
    Health {
        argv: Vec<String>,
        #[serde(default)]
        timeout_ms: Option<u64>,
    },
    /// Stop the process and start it again now.
    Restart,
    Status,
}

/// The supervised process as the guest sees it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessState {
    pub pid: Option<u32>,
    /// Times the process has been spawned.
    pub starts: u32,
    /// Exit code of the last run (`128 + signal` if killed).
    pub last_exit: Option<i32>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ProcessState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    fn error(message: impl Into<String>) -> Self {
        Response { ok: false, error: Some(message.into()), ..Response::default() }
    }
}

/// What the guest side of the protocol drives.
pub trait Control: Send + Sync {
    fn health(&self, argv: Vec<String>, timeout: Duration) -> BoxFuture<'_, Result<i32, String>>;
    fn restart(&self) -> BoxFuture<'_, Result<(), String>>;
    fn status(&self) -> ProcessState;
}

#[derive(Debug, thiserror::Error)]
pub enum ControlError {
    #[error("control connection: {0}")]
    Io(#[from] io::Error),
    #[error("control connection closed")]
    Closed,
    #[error("malformed control message: {0}")]
    Malformed(String),
    #[error("guest refused: {0}")]
    Refused(String),
}

/// Reads one `\n`-terminated line of at most `max` bytes; `None` at a clean EOF.
async fn read_line<R: AsyncBufRead + Unpin>(r: &mut R, max: usize) -> io::Result<Option<Vec<u8>>> {
    let mut out = Vec::new();
    loop {
        let buf = r.fill_buf().await?;
        if buf.is_empty() {
            return if out.is_empty() { Ok(None) } else { Err(io::Error::new(io::ErrorKind::UnexpectedEof, "unterminated line")) };
        }
        if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            out.extend_from_slice(&buf[..pos]);
            r.consume(pos + 1);
            if out.len() > max {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
            }
            return Ok(Some(out));
        }
        let n = buf.len();
        out.extend_from_slice(buf);
        r.consume(n);
        if out.len() > max {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "line too long"));
        }
    }
}

async fn write_json<W: AsyncWrite + Unpin, T: Serialize>(w: &mut W, value: &T) -> io::Result<()> {
    let mut line = serde_json::to_vec(value).map_err(io::Error::other)?;
    line.push(b'\n');
    w.write_all(&line).await?;
    w.flush().await
}

async fn handle(control: &dyn Control, line: &[u8]) -> Response {
    let request: Request = match serde_json::from_slice(line) {
        Ok(r) => r,
        Err(e) => return Response::error(format!("bad request: {e}")),
    };
    match request {
        Request::Health { argv, timeout_ms } => {
            if argv.is_empty() {
                return Response::error("health argv is empty");
            }
            let timeout = timeout_ms.map_or(DEFAULT_HEALTH_TIMEOUT, Duration::from_millis);
            match control.health(argv, timeout).await {
                Ok(code) => Response { ok: true, exit_code: Some(code), ..Response::default() },
                Err(e) => Response::error(e),
            }
        }
        Request::Restart => match control.restart().await {
            Ok(()) => Response { ok: true, ..Response::default() },
            Err(e) => Response::error(e),
        },
        Request::Status => Response { ok: true, status: Some(control.status()), ..Response::default() },
    }
}

/// Serves requests from `conn` until it closes. Malformed requests get an error response;
/// an oversized line ends the connection.
pub async fn serve<S: AsyncRead + AsyncWrite + Unpin>(conn: S, control: &dyn Control) -> io::Result<()> {
    let (read, mut write) = tokio::io::split(conn);
    let mut read = BufReader::new(read);
    while let Some(line) = read_line(&mut read, MAX_LINE).await? {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        let response = handle(control, &line).await;
        write_json(&mut write, &response).await?;
    }
    Ok(())
}

/// The host side of the protocol.
pub struct Client<S> {
    conn: BufReader<S>,
}

impl<S: AsyncRead + AsyncWrite + Unpin> Client<S> {
    pub fn new(conn: S) -> Self {
        Client { conn: BufReader::new(conn) }
    }

    /// Sends one request and reads its response.
    pub async fn call(&mut self, request: &Request) -> Result<Response, ControlError> {
        write_json(self.conn.get_mut(), request).await?;
        let line = read_line(&mut self.conn, MAX_LINE).await?.ok_or(ControlError::Closed)?;
        serde_json::from_slice(&line).map_err(|e| ControlError::Malformed(e.to_string()))
    }

    async fn call_ok(&mut self, request: &Request) -> Result<Response, ControlError> {
        let response = self.call(request).await?;
        if response.ok { Ok(response) } else { Err(ControlError::Refused(response.error.unwrap_or_default())) }
    }

    /// Runs a health command in the guest; its exit code.
    pub async fn health(&mut self, argv: &[String], timeout: Duration) -> Result<i32, ControlError> {
        let timeout_ms = u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX);
        let request = Request::Health { argv: argv.to_vec(), timeout_ms: Some(timeout_ms) };
        let response = self.call_ok(&request).await?;
        response.exit_code.ok_or_else(|| ControlError::Malformed("health response without exit_code".into()))
    }

    pub async fn restart(&mut self) -> Result<(), ControlError> {
        self.call_ok(&Request::Restart).await.map(drop)
    }

    pub async fn status(&mut self) -> Result<ProcessState, ControlError> {
        let response = self.call_ok(&Request::Status).await?;
        response.status.ok_or_else(|| ControlError::Malformed("status response without status".into()))
    }
}

#[cfg(test)]
mod tests {
    use parking_lot::Mutex;

    use super::*;

    /// Healthy iff argv[0] == "ok"; records calls.
    #[derive(Default)]
    struct Fake {
        restarts: Mutex<u32>,
        timeouts: Mutex<Vec<Duration>>,
    }

    impl Control for Fake {
        fn health(&self, argv: Vec<String>, timeout: Duration) -> BoxFuture<'_, Result<i32, String>> {
            self.timeouts.lock().push(timeout);
            Box::pin(async move {
                match argv[0].as_str() {
                    "ok" => Ok(0),
                    "boom" => Err("spawn failed".into()),
                    _ => Ok(3),
                }
            })
        }
        fn restart(&self) -> BoxFuture<'_, Result<(), String>> {
            *self.restarts.lock() += 1;
            Box::pin(async { Ok(()) })
        }
        fn status(&self) -> ProcessState {
            ProcessState { pid: Some(7), starts: 1 + *self.restarts.lock(), last_exit: None }
        }
    }

    #[tokio::test]
    async fn requests_and_responses_over_a_duplex() {
        let fake = std::sync::Arc::new(Fake::default());
        let (host, guest) = tokio::io::duplex(1024);
        let server = {
            let fake = fake.clone();
            tokio::spawn(async move { serve(guest, fake.as_ref()).await })
        };
        let mut client = Client::new(host);
        assert_eq!(client.health(&["ok".into()], Duration::from_millis(1500)).await.unwrap(), 0);
        assert_eq!(client.health(&["fail".into()], Duration::from_secs(1)).await.unwrap(), 3);
        assert!(matches!(
            client.health(&["boom".into()], Duration::from_secs(1)).await,
            Err(ControlError::Refused(m)) if m == "spawn failed"
        ));
        assert!(matches!(client.health(&[], Duration::from_secs(1)).await, Err(ControlError::Refused(_))));
        client.restart().await.unwrap();
        assert_eq!(client.status().await.unwrap(), ProcessState { pid: Some(7), starts: 2, last_exit: None });
        assert_eq!(fake.timeouts.lock()[0], Duration::from_millis(1500));
        drop(client);
        server.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn wire_format_and_malformed_requests() {
        let fake = std::sync::Arc::new(Fake::default());
        let (host, guest) = tokio::io::duplex(1024);
        let f = fake.clone();
        let server = tokio::spawn(async move { serve(guest, f.as_ref()).await });
        let (read, mut write) = tokio::io::split(host);
        let mut read = BufReader::new(read);
        let mut line = String::new();
        write.write_all(b"{\"op\":\"health\",\"argv\":[\"ok\"]}\n\n{\"op\":\"nope\"}\nnot json\n{\"op\":\"status\"}\n").await.unwrap();
        read.read_line(&mut line).await.unwrap();
        assert_eq!(line, "{\"ok\":true,\"exit_code\":0}\n");
        assert_eq!(fake.timeouts.lock()[0], DEFAULT_HEALTH_TIMEOUT);
        for _ in 0..2 {
            line.clear();
            read.read_line(&mut line).await.unwrap();
            let r: Response = serde_json::from_str(&line).unwrap();
            assert!(!r.ok && r.error.unwrap().starts_with("bad request"));
        }
        line.clear();
        read.read_line(&mut line).await.unwrap();
        assert!(line.starts_with("{\"ok\":true,\"status\":{\"pid\":7"));
        drop(write);
        drop(read);
        server.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn oversized_lines_end_the_connection() {
        let fake = Fake::default();
        let (mut host, guest) = tokio::io::duplex(MAX_LINE * 2);
        let long = vec![b'x'; MAX_LINE + 10];
        host.write_all(&long).await.unwrap();
        let err = serve(guest, &fake).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
