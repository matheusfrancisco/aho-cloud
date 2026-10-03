//! `ahocloud ssh`: a sandbox terminal over the session host's WebSocket, speaking
//! [`cloud_api::exec`] frames. With a PTY at a terminal, the local tty is put in raw mode
//! (restored on every way out) and resized with the window. Without a PTY (the default when stdin
//! is not a terminal, or `-T`), stdin is streamed until EOF, then closed, and stdout/stderr come
//! back byte for byte.

use std::io::{IsTerminal, Read, Write};
use std::process::ExitCode;
use std::sync::{Arc, Once};

use cloud_api::TerminalRequest;
use cloud_api::client::Client;
use cloud_api::exec::{Frame, PtySize};
use futures::{SinkExt, StreamExt};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{Connector, connect_async_tls_with_config};

use crate::error::{Error, Result};

/// Size asked for when stdin is not a terminal.
const DEFAULT_SIZE: PtySize = PtySize { cols: 80, rows: 24 };

/// Largest stdin chunk per frame; well under [`cloud_api::exec::MAX_FRAME`].
const CHUNK: usize = 64 * 1024;

/// `pty`: `Some(true)` forces one (`-t`), `Some(false)` refuses one (`-T`), `None` follows
/// whether stdin is a terminal.
pub async fn ssh(client: &Client, slug: &str, command: Vec<String>, pty: Option<bool>) -> Result<ExitCode> {
    let stdin_tty = std::io::stdin().is_terminal();
    let pty = pty.unwrap_or(stdin_tty);
    // Raw mode and resizes only make sense for a PTY driven from a local terminal.
    let interactive = pty && stdin_tty;
    let size = if interactive { local_size() } else { DEFAULT_SIZE };
    let request = TerminalRequest { cols: size.cols, rows: size.rows, command: (!command.is_empty()).then_some(command), no_pty: !pty };
    let access = client.sandbox_terminal(slug, &request).await?;
    let sep = if access.url.contains('?') { '&' } else { '?' };
    let url = format!("{}{sep}token={}", access.url, query_value(&access.token));
    let (ws, _) = connect_async_tls_with_config(url.as_str(), None, false, Some(Connector::Rustls(Arc::new(tls_config()?))))
        .await
        .map_err(|e| Error::Terminal(format!("cannot connect to {}: {e}", access.url)))?;

    let (mut resizes, mut endings) = (Resizes::new(interactive)?, Endings::new()?);
    let _raw = if interactive { Some(RawMode::enable()?) } else { None };
    let (mut tx, mut rx) = ws.split();
    let mut stdin = spawn_stdin_reader();
    let mut stdin_open = true;
    let send_err = |e: tokio_tungstenite::tungstenite::Error| Error::Terminal(format!("the connection broke: {e}"));
    loop {
        tokio::select! {
            msg = rx.next() => match msg {
                Some(Ok(Message::Binary(bytes))) => match Frame::decode(&bytes) {
                    Ok(Frame::Stdout(b)) => write_all(&mut std::io::stdout(), &b, "<stdout>")?,
                    Ok(Frame::Stderr(b)) => write_all(&mut std::io::stderr(), &b, "<stderr>")?,
                    Ok(Frame::Exit(code)) => {
                        let _ = tx.close().await;
                        return Ok(ExitCode::from(u8::try_from(code).unwrap_or(u8::MAX)));
                    }
                    Ok(Frame::Error(text)) => return Err(Error::Terminal(text)),
                    Ok(other) => return Err(Error::Terminal(format!("the sandbox sent an unexpected {other:?} frame"))),
                    Err(e) => return Err(Error::Terminal(format!("the sandbox sent a bad frame: {e}"))),
                },
                Some(Ok(Message::Close(_))) | None => {
                    return Err(Error::Terminal("the sandbox closed the terminal without an exit status".into()));
                }
                Some(Ok(_)) => {}
                Some(Err(e)) => return Err(send_err(e)),
            },
            chunk = stdin.recv(), if stdin_open => {
                let frame = match chunk {
                    Some(bytes) => Frame::Stdin(bytes),
                    None => {
                        stdin_open = false;
                        Frame::CloseStdin
                    }
                };
                tx.send(Message::Binary(frame.encode().into())).await.map_err(send_err)?;
            }
            () = resizes.next() => {
                tx.send(Message::Binary(Frame::Resize(local_size()).encode().into())).await.map_err(send_err)?;
            }
            signal = endings.next() => {
                let _ = tx.close().await;
                return Ok(ExitCode::from(128 + signal));
            }
        }
    }
}

fn write_all(out: &mut impl Write, bytes: &[u8], name: &str) -> Result<()> {
    out.write_all(bytes).and_then(|()| out.flush()).map_err(Error::io(name))
}

fn local_size() -> PtySize {
    match crossterm::terminal::size() {
        Ok((cols, rows)) if cols > 0 && rows > 0 => PtySize { cols, rows },
        _ => DEFAULT_SIZE,
    }
}

/// Percent-encodes a query value.
fn query_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `wss://` with the webpki roots, on ring (the provider `reqwest` already links).
fn tls_config() -> Result<rustls::ClientConfig> {
    let roots = rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::Terminal(format!("tls: {e}")))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(config)
}

/// Stdin on its own thread (a blocking read must not hold up the runtime at exit). The channel
/// closes at end of input.
fn spawn_stdin_reader() -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel(8);
    std::thread::spawn(move || {
        let mut stdin = std::io::stdin().lock();
        let mut buf = vec![0u8; CHUNK];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) => return,
                Ok(n) => {
                    if tx.blocking_send(buf[..n].to_vec()).is_err() {
                        return;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(_) => return,
            }
        }
    });
    rx
}

/// The local terminal in raw mode until dropped, or until a panic.
struct RawMode;

impl RawMode {
    fn enable() -> Result<Self> {
        static HOOK: Once = Once::new();
        HOOK.call_once(|| {
            let previous = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                let _ = crossterm::terminal::disable_raw_mode();
                previous(info);
            }));
        });
        crossterm::terminal::enable_raw_mode().map_err(Error::io("<terminal>"))?;
        Ok(RawMode)
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

/// Window resizes, at a terminal.
#[cfg(unix)]
struct Resizes(Option<tokio::signal::unix::Signal>);

#[cfg(unix)]
impl Resizes {
    fn new(interactive: bool) -> Result<Self> {
        let winch = || tokio::signal::unix::signal(tokio::signal::unix::SignalKind::window_change()).map_err(Error::io("<signals>"));
        Ok(Self(if interactive { Some(winch()?) } else { None }))
    }

    async fn next(&mut self) {
        match &mut self.0 {
            Some(s) => {
                s.recv().await;
            }
            None => std::future::pending().await,
        }
    }
}

/// The signals that end the session (in raw mode the tty sends none; Ctrl-C goes to the sandbox).
#[cfg(unix)]
struct Endings([(u8, tokio::signal::unix::Signal); 4]);

#[cfg(unix)]
impl Endings {
    fn new() -> Result<Self> {
        use tokio::signal::unix::{SignalKind, signal};
        let on = |n: u8, kind: SignalKind| signal(kind).map(|s| (n, s)).map_err(Error::io("<signals>"));
        Ok(Self([
            on(1, SignalKind::hangup())?,
            on(2, SignalKind::interrupt())?,
            on(3, SignalKind::quit())?,
            on(15, SignalKind::terminate())?,
        ]))
    }

    /// The signal number.
    async fn next(&mut self) -> u8 {
        let [(n1, s1), (n2, s2), (n3, s3), (n4, s4)] = &mut self.0;
        tokio::select! {
            _ = s1.recv() => *n1,
            _ = s2.recv() => *n2,
            _ = s3.recv() => *n3,
            _ = s4.recv() => *n4,
        }
    }
}

#[cfg(not(unix))]
struct Resizes;

#[cfg(not(unix))]
impl Resizes {
    fn new(_interactive: bool) -> Result<Self> {
        Ok(Self)
    }

    async fn next(&mut self) {
        std::future::pending().await
    }
}

#[cfg(not(unix))]
struct Endings;

#[cfg(not(unix))]
impl Endings {
    fn new() -> Result<Self> {
        Ok(Self)
    }

    async fn next(&mut self) -> u8 {
        let _ = tokio::signal::ctrl_c().await;
        2
    }
}
