//! Sandbox terminals and chats end to end: a real 127.0.0.1 session host talking to an
//! in-process fake of the runtime agent's exec socket.
// Helpers outside `#[test]` fns are test code too.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use axum::extract::ws::{Message as AxMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use cloud_api::exec::{ExecRequest, Frame, MAX_FRAME, PtySize};
use cloud_api::internal::routes;
use cloud_api::{ApiError, SandboxChat, TerminalAccess};
use cloud_session_host::token::{self, Claims};
use cloud_session_host::{Config, Limits, SandboxConfig, SessionHost, TokenKey, TokioSpawner};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

const SERVICE_TOKEN: &str = "service-token-for-tests";
const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";
/// The session id the fake aho hands out when it is not resuming.
const NEW_SESSION: &str = "5e55100";
const CHAT_COMMAND: &str = "/opt/aho/bin/aho";
const STEP: Duration = Duration::from_secs(10);

type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

// ---- fake runtime agent --------------------------------------------------------------------

/// Records what the session host asked for and when it hung up.
#[derive(Default)]
struct Fake {
    requests: Mutex<Vec<(String, ExecRequest)>>,
    /// Exec sockets the session host closed (or dropped) before the process ended.
    hung_up: AtomicUsize,
    changed: Notify,
}

impl Fake {
    fn requests(&self) -> Vec<(String, ExecRequest)> {
        self.requests.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    async fn wait_hung_up(&self, n: usize) {
        tokio::time::timeout(STEP, async {
            loop {
                let notified = self.changed.notified();
                if self.hung_up.load(Ordering::SeqCst) >= n {
                    return;
                }
                notified.await;
            }
        })
        .await
        .expect("the session host never closed the exec socket");
    }
}

/// `GET SANDBOX_EXEC`: only with the service token; sandbox `missing` does not exist.
async fn exec_route(State(fake): State<Arc<Fake>>, Path(sandbox): Path<String>, headers: HeaderMap, upgrade: WebSocketUpgrade) -> Response {
    let bearer = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok());
    if bearer != Some(&format!("Bearer {SERVICE_TOKEN}")) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    if sandbox == "missing" {
        return StatusCode::NOT_FOUND.into_response();
    }
    upgrade.on_upgrade(move |ws| exec_session(fake, sandbox, ws))
}

async fn send(ws: &mut WebSocket, frame: Frame) {
    let _ = ws.send(AxMessage::Binary(frame.encode().into())).await;
}

/// Ends the session from the server side: the last frame, then a close.
async fn finish(ws: &mut WebSocket, frame: Frame) {
    send(ws, frame).await;
    let _ = ws.send(AxMessage::Close(None)).await;
}

async fn exec_session(fake: Arc<Fake>, sandbox: String, mut ws: WebSocket) {
    let Some(Ok(AxMessage::Text(first))) = ws.recv().await else { panic!("the first message must be the text ExecRequest") };
    let req: ExecRequest = serde_json::from_str(&first).expect("the first message is an ExecRequest");
    fake.requests.lock().unwrap_or_else(PoisonError::into_inner).push((sandbox, req.clone()));
    if req.pty.is_some() { fake_shell(&fake, ws).await } else { fake_aho(&fake, &req, ws).await }
}

fn hung_up(fake: &Fake) {
    fake.hung_up.fetch_add(1, Ordering::SeqCst);
    fake.changed.notify_waiters();
}

/// PTY session: echoes stdin as `echo:<bytes>`, reports resizes, `exit\r` exits 7, EOF exits 0.
async fn fake_shell(fake: &Fake, mut ws: WebSocket) {
    loop {
        match ws.recv().await {
            Some(Ok(AxMessage::Binary(b))) => match Frame::decode(&b).expect("a valid frame") {
                Frame::Stdin(data) if data == b"exit\r" => return finish(&mut ws, Frame::Exit(7)).await,
                Frame::Stdin(data) => send(&mut ws, Frame::Stdout([b"echo:".as_slice(), &data].concat())).await,
                Frame::Resize(PtySize { cols, rows }) => send(&mut ws, Frame::Stdout(format!("resize {cols}x{rows}").into_bytes())).await,
                Frame::CloseStdin => return finish(&mut ws, Frame::Exit(0)).await,
                other => panic!("the session host relayed {other:?}"),
            },
            Some(Ok(AxMessage::Close(_)) | Err(_)) | None => return hung_up(fake),
            Some(Ok(_)) => {}
        }
    }
}

/// Pipe session behaving like `aho rpc`: `hello` → `hello_ok` (session = `--session` value or
/// [`NEW_SESSION`], plus argv); `exit` exits 3, `quit` exits 0, `fail` writes why to stderr and
/// exits 1, `crash` ends with an `Error`
/// frame, `hang` makes it ignore stdin EOF; anything else is echoed as a `result`. Writes a line
/// to stderr first.
async fn fake_aho(fake: &Fake, req: &ExecRequest, mut ws: WebSocket) {
    let session = req.argv.windows(2).find(|w| w[0] == "--session").map_or(NEW_SESSION.to_owned(), |w| w[1].clone());
    let argv = req.argv.join(" ");
    send(&mut ws, Frame::Stderr(b"aho: starting\n".to_vec())).await;
    let mut pending = Vec::new();
    let mut ignore_eof = false;
    loop {
        match ws.recv().await {
            Some(Ok(AxMessage::Binary(b))) => match Frame::decode(&b).expect("a valid frame") {
                Frame::Stdin(data) => {
                    pending.extend_from_slice(&data);
                    while let Some(nl) = pending.iter().position(|&c| c == b'\n') {
                        let line = String::from_utf8(pending.drain(..=nl).collect()).unwrap();
                        let line = line.trim_end();
                        let reply = if line.contains(r#""method":"hello""#) {
                            json!({"v": 1, "reply_to": 0, "event": "hello_ok", "payload": {"version": 1, "session": session, "argv": argv}})
                        } else if line.contains(r#""method":"exit""#) {
                            return finish(&mut ws, Frame::Exit(3)).await;
                        } else if line.contains(r#""method":"quit""#) {
                            return finish(&mut ws, Frame::Exit(0)).await;
                        } else if line.contains(r#""method":"fail""#) {
                            // Split across frames, the way a pipe may deliver it.
                            send(&mut ws, Frame::Stderr(b"aho: session could not start: no model ".to_vec())).await;
                            send(&mut ws, Frame::Stderr(b"route `llm:anthropic` is active\n\n".to_vec())).await;
                            return finish(&mut ws, Frame::Exit(1)).await;
                        } else if line.contains(r#""method":"crash""#) {
                            return finish(&mut ws, Frame::Error("boom".into())).await;
                        } else if line.contains(r#""method":"hang""#) {
                            ignore_eof = true;
                            continue;
                        } else {
                            json!({"v": 1, "reply_to": null, "event": "result", "payload": serde_json::from_str::<Value>(line).unwrap()})
                        };
                        send(&mut ws, Frame::Stdout(format!("{reply}\n").into_bytes())).await;
                    }
                }
                Frame::CloseStdin if ignore_eof => {}
                Frame::CloseStdin => return finish(&mut ws, Frame::Exit(0)).await,
                other => panic!("the session host sent {other:?}"),
            },
            Some(Ok(AxMessage::Close(_)) | Err(_)) | None => return hung_up(fake),
            Some(Ok(_)) => {}
        }
    }
}

async fn fake_runtime() -> (String, Arc<Fake>) {
    let fake = Arc::new(Fake::default());
    let app = axum::Router::new().route(routes::SANDBOX_EXEC, axum::routing::get(exec_route)).with_state(Arc::clone(&fake));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await });
    (format!("http://{addr}"), fake)
}

// ---- session host --------------------------------------------------------------------------

struct Host {
    http: reqwest::Client,
    base: String,
    ws_base: String,
    fake: Arc<Fake>,
    _dir: tempfile::TempDir,
}

struct Setup {
    runtime: bool,
    limits: Limits,
    terminal_token_ttl: Duration,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            runtime: true,
            limits: Limits { kill_grace: Duration::from_millis(300), ..Limits::DEFAULT },
            terminal_token_ttl: Duration::from_secs(60),
        }
    }
}

async fn host(setup: Setup) -> Host {
    let dir = tempfile::tempdir().unwrap();
    let (runtime_url, fake) = fake_runtime().await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = Config {
        data_root: dir.path().join("data"),
        public_url: format!("ws://{addr}"),
        preview_url: "https://{project}.preview.test".into(),
        service_token: SERVICE_TOKEN.into(),
        token_key: TokenKey::new(SECRET).unwrap(),
        token_ttl: Duration::from_secs(60),
        command: Default::default(),
        limits: setup.limits,
        sandbox: SandboxConfig {
            runtime_url: setup.runtime.then_some(runtime_url),
            chat_command: CHAT_COMMAND.into(),
            terminal_token_ttl: setup.terminal_token_ttl,
        },
    };
    let host = SessionHost::new(config, Arc::new(TokioSpawner)).unwrap();
    tokio::spawn(host.serve(listener, std::future::pending()));
    Host { http: reqwest::Client::new(), base: format!("http://{addr}"), ws_base: format!("ws://{addr}"), fake, _dir: dir }
}

fn shell(cols: u16, rows: u16) -> ExecRequest {
    ExecRequest {
        argv: vec!["bash".into(), "-l".into()],
        pty: Some(PtySize { cols, rows }),
        env: BTreeMap::from([("TERM".into(), "xterm-256color".into())]),
        cwd: Some("/workspace".into()),
    }
}

impl Host {
    async fn post(&self, route: &str, bearer: Option<&str>, body: Value) -> reqwest::Response {
        let mut req = self.http.post(format!("{}{route}", self.base)).json(&body);
        if let Some(b) = bearer {
            req = req.bearer_auth(b);
        }
        req.send().await.unwrap()
    }

    async fn terminal(&self, sandbox: &str, user: &str, exec: &ExecRequest) -> TerminalAccess {
        let resp =
            self.post(routes::SANDBOX_TERMINALS, Some(SERVICE_TOKEN), json!({"sandbox": sandbox, "user_id": user, "exec": exec})).await;
        assert_eq!(resp.status(), 200);
        resp.json().await.unwrap()
    }

    async fn chat(&self, sandbox: &str, user: &str) -> SandboxChat {
        let resp = self.post(routes::SANDBOX_CHATS, Some(SERVICE_TOKEN), json!({"sandbox": sandbox, "user_id": user})).await;
        assert_eq!(resp.status(), 200);
        resp.json().await.unwrap()
    }
}

fn open_url(access: &TerminalAccess) -> String {
    format!("{}?token={}", access.url, access.token)
}

fn chat_url(chat: &SandboxChat) -> String {
    format!("{}?token={}", chat.rpc_url, chat.token)
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

async fn connect(url: &str) -> Ws {
    connect_async(url).await.unwrap().0
}

async fn handshake_status(url: &str) -> u16 {
    match connect_async(url).await.expect_err("handshake should be refused") {
        tungstenite::Error::Http(resp) => resp.status().as_u16(),
        other => panic!("expected an HTTP refusal, got {other}"),
    }
}

async fn next(ws: &mut Ws) -> Message {
    loop {
        match tokio::time::timeout(STEP, ws.next()).await.expect("timed out").expect("stream ended").expect("websocket error") {
            Message::Ping(_) | Message::Pong(_) => {}
            msg => return msg,
        }
    }
}

async fn send_frame(ws: &mut Ws, frame: Frame) {
    ws.send(Message::Binary(frame.encode().into())).await.unwrap();
}

async fn recv_frame(ws: &mut Ws) -> Frame {
    match next(ws).await {
        Message::Binary(b) => Frame::decode(&b).unwrap(),
        other => panic!("expected a binary frame, got {other:?}"),
    }
}

/// Binary frames before the close frame, and the close frame.
async fn recv_close(ws: &mut Ws) -> (Vec<Frame>, CloseFrame) {
    let mut frames = Vec::new();
    loop {
        match next(ws).await {
            Message::Binary(b) => frames.push(Frame::decode(&b).unwrap()),
            Message::Close(Some(frame)) => return (frames, frame),
            other => panic!("expected a close frame, got {other:?}"),
        }
    }
}

/// Text frames before the close frame, and the close frame.
async fn recv_text_close(ws: &mut Ws) -> (Vec<Value>, CloseFrame) {
    let mut texts = Vec::new();
    loop {
        match next(ws).await {
            Message::Text(t) => texts.push(serde_json::from_str(&t).unwrap()),
            Message::Close(Some(frame)) => return (texts, frame),
            other => panic!("expected a close frame, got {other:?}"),
        }
    }
}

async fn send_text(ws: &mut Ws, value: Value) {
    ws.send(Message::Text(value.to_string().into())).await.unwrap();
}

async fn recv_json(ws: &mut Ws) -> Value {
    match next(ws).await {
        Message::Text(t) => serde_json::from_str(&t).unwrap(),
        other => panic!("expected a text frame, got {other:?}"),
    }
}

async fn hello(ws: &mut Ws) -> Value {
    send_text(ws, json!({"v": 1, "id": 0, "method": "hello", "params": {"min": 1, "max": 1}})).await;
    let frame = recv_json(ws).await;
    assert_eq!(frame["event"], "hello_ok", "{frame}");
    frame["payload"].clone()
}

/// Connects and says hello, retrying while the previous connection is still being torn down.
async fn hello_when_free(url: &str) -> (Ws, Value) {
    let deadline = Instant::now() + STEP;
    loop {
        let mut ws = connect(url).await;
        let _ = ws.send(Message::Text(json!({"v": 1, "id": 0, "method": "hello"}).to_string().into())).await;
        match next(&mut ws).await {
            Message::Text(t) => {
                let frame: Value = serde_json::from_str(&t).unwrap();
                if frame["event"] == "hello_ok" {
                    return (ws, frame["payload"].clone());
                }
                let (_, close) = recv_text_close(&mut ws).await;
                assert_eq!(close.code, CloseCode::Again, "{close:?}");
            }
            Message::Close(Some(close)) => assert_eq!(close.code, CloseCode::Again, "{close:?}"),
            other => panic!("unexpected {other:?}"),
        }
        assert!(Instant::now() < deadline, "the chat never became free");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

// ---- configuration and internal routes -----------------------------------------------------

#[tokio::test]
async fn sandbox_routes_answer_503_without_a_runtime_url() {
    let h = host(Setup { runtime: false, ..Setup::default() }).await;
    for (route, body) in [
        (routes::SANDBOX_TERMINALS, json!({"sandbox": "box", "user_id": "alice", "exec": shell(80, 24)})),
        (routes::SANDBOX_CHATS, json!({"sandbox": "box", "user_id": "alice"})),
    ] {
        // Unauthenticated callers learn nothing about the configuration.
        assert_eq!(h.post(route, None, body.clone()).await.status(), 401, "{route}");
        let resp = h.post(route, Some(SERVICE_TOKEN), body).await;
        assert_eq!(resp.status(), 503, "{route}");
        assert_eq!(resp.json::<ApiError>().await.unwrap().code, "sandboxes_disabled");
    }
    assert_eq!(handshake_status(&format!("{}/terminal/box?token=x", h.ws_base)).await, 503);
    assert_eq!(handshake_status(&format!("{}/sandbox-rpc/box-0011223344556677?token=x", h.ws_base)).await, 503);
    assert!(h.fake.requests().is_empty());
}

#[tokio::test]
async fn internal_sandbox_routes_require_the_service_token_and_valid_input() {
    let h = host(Setup::default()).await;
    let terminal = json!({"sandbox": "box", "user_id": "alice", "exec": shell(80, 24)});
    let chat = json!({"sandbox": "box", "user_id": "alice"});
    for (route, body) in [(routes::SANDBOX_TERMINALS, &terminal), (routes::SANDBOX_CHATS, &chat)] {
        for bearer in [None, Some("service-token-for-test"), Some("SERVICE-TOKEN-FOR-TESTS")] {
            let resp = h.post(route, bearer, body.clone()).await;
            assert_eq!(resp.status(), 401, "{route} {bearer:?}");
            assert_eq!(resp.json::<ApiError>().await.unwrap().code, "unauthorized");
        }
    }
    let empty_argv = ExecRequest { argv: vec![], ..shell(80, 24) };
    for (route, bad) in [
        (routes::SANDBOX_TERMINALS, json!({"sandbox": "Box", "user_id": "alice", "exec": shell(80, 24)})),
        (routes::SANDBOX_TERMINALS, json!({"sandbox": "box", "user_id": "", "exec": shell(80, 24)})),
        (routes::SANDBOX_TERMINALS, json!({"sandbox": "box", "user_id": "alice", "exec": empty_argv})),
        (routes::SANDBOX_TERMINALS, json!({"sandbox": "box", "user_id": "alice"})),
        (routes::SANDBOX_CHATS, json!({"sandbox": "../etc", "user_id": "alice"})),
        (routes::SANDBOX_CHATS, json!({"sandbox": "box", "user_id": "alice", "session_id": "--help"})),
    ] {
        let resp = h.post(route, Some(SERVICE_TOKEN), bad.clone()).await;
        assert_eq!(resp.status(), 400, "{bad}");
        assert_eq!(resp.json::<ApiError>().await.unwrap().code, "invalid");
    }
}

// ---- terminals -----------------------------------------------------------------------------

#[tokio::test]
async fn a_terminal_relays_input_resize_and_exit_with_the_granted_request() {
    let h = host(Setup::default()).await;
    let exec = shell(120, 40);
    let access = h.terminal("box", "alice", &exec).await;
    assert_eq!(access.url, format!("{}/terminal/box", h.ws_base));
    // At most a minute.
    let key = TokenKey::new(SECRET).unwrap();
    assert!(token::verify_terminal(&key, &access.token, "box", now() + 60).is_err());
    assert_eq!(token::verify_terminal(&key, &access.token, "box", now()).unwrap().user, "alice");

    let mut ws = connect(&open_url(&access)).await;
    send_frame(&mut ws, Frame::Stdin(b"ls\r".to_vec())).await;
    assert_eq!(recv_frame(&mut ws).await, Frame::Stdout(b"echo:ls\r".to_vec()));
    send_frame(&mut ws, Frame::Resize(PtySize { cols: 100, rows: 30 })).await;
    assert_eq!(recv_frame(&mut ws).await, Frame::Stdout(b"resize 100x30".to_vec()));
    send_frame(&mut ws, Frame::Stdin(b"exit\r".to_vec())).await;
    let (frames, close) = recv_close(&mut ws).await;
    assert_eq!(frames, [Frame::Exit(7)]);
    assert_eq!(close.code, CloseCode::Normal);

    // The runtime agent got exactly the request the control plane asked for, for that sandbox.
    assert_eq!(h.fake.requests(), [("box".to_owned(), exec)]);
}

#[tokio::test]
async fn terminal_tokens_are_single_use_scoped_and_unforgeable() {
    let h = host(Setup::default()).await;
    let access = h.terminal("box", "alice", &shell(80, 24)).await;

    assert_eq!(handshake_status(&access.url).await, 401, "no token");
    // Another sandbox's URL refuses it (and does not use it up).
    assert_eq!(handshake_status(&format!("{}/terminal/other?token={}", h.ws_base, access.token)).await, 401, "wrong sandbox");
    // Claims rewritten to another user keep the old MAC: refused.
    let (payload, tag) = access.token.split_once('.').unwrap();
    let mut claims: Value = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).unwrap()).unwrap();
    claims["u"] = json!("mallory");
    let forged = format!("{}.{tag}", URL_SAFE_NO_PAD.encode(claims.to_string()));
    assert_eq!(handshake_status(&format!("{}?token={forged}", access.url)).await, 401, "forged user");
    // An rpc token for the same names is not a terminal token.
    let rpc = token::mint_chat(&TokenKey::new(SECRET).unwrap(), &Claims { workspace: "box".into(), user: "alice".into(), exp: now() + 60 });
    assert_eq!(handshake_status(&format!("{}?token={rpc}", access.url)).await, 401, "chat token");

    let mut ws = connect(&open_url(&access)).await;
    send_frame(&mut ws, Frame::Stdin(b"pwd\r".to_vec())).await;
    assert_eq!(recv_frame(&mut ws).await, Frame::Stdout(b"echo:pwd\r".to_vec()));
    assert_eq!(handshake_status(&open_url(&access)).await, 401, "second use while the first is open");
    let _ = ws.close(None).await;
    h.fake.wait_hung_up(1).await;
    assert_eq!(handshake_status(&open_url(&access)).await, 401, "second use after it closed");
    assert_eq!(h.fake.requests().len(), 1);
}

#[tokio::test]
async fn terminal_tokens_expire() {
    let h = host(Setup { terminal_token_ttl: Duration::from_secs(1), ..Setup::default() }).await;
    let access = h.terminal("box", "alice", &shell(80, 24)).await;
    tokio::time::sleep(Duration::from_millis(2100)).await;
    assert_eq!(handshake_status(&open_url(&access)).await, 401);
    assert!(h.fake.requests().is_empty());
}

#[tokio::test]
async fn browser_frames_other_than_input_close_the_terminal() {
    let h = host(Setup::default()).await;
    let cases: [(&str, Message, CloseCode); 5] = [
        (
            "request",
            Message::Binary(Frame::Request(ExecRequest { argv: vec!["sh".into()], ..shell(80, 24) }).encode().into()),
            CloseCode::Policy,
        ),
        ("server-kind exit", Message::Binary(Frame::Exit(0).encode().into()), CloseCode::Policy),
        ("unknown kind", Message::Binary(vec![42].into()), CloseCode::Policy),
        ("text", Message::Text("ls".into()), CloseCode::Unsupported),
        ("oversized", Message::Binary([&[0u8][..], &vec![b'a'; MAX_FRAME]].concat().into()), CloseCode::Size),
    ];
    for (i, (what, msg, code)) in cases.into_iter().enumerate() {
        let mut ws = connect(&open_url(&h.terminal("box", "alice", &shell(80, 24)).await)).await;
        ws.send(msg).await.unwrap();
        let (frames, close) = recv_close(&mut ws).await;
        assert_eq!(close.code, code, "{what}");
        assert!(frames.is_empty(), "{what}: {frames:?}");
        // The runtime side is closed too; nothing but the granted request ever reached it.
        h.fake.wait_hung_up(i + 1).await;
    }
    assert!(h.fake.requests().iter().all(|(_, r)| r.argv == ["bash", "-l"]));
}

#[tokio::test]
async fn closing_the_browser_side_ends_the_runtime_session_and_frees_the_slot() {
    let h = host(Setup { limits: Limits { max_connections: 1, ..Setup::default().limits }, ..Setup::default() }).await;
    let mut ws = connect(&open_url(&h.terminal("box", "alice", &shell(80, 24)).await)).await;
    send_frame(&mut ws, Frame::Stdin(b"x".to_vec())).await;
    recv_frame(&mut ws).await;

    // The host limit applies to terminals: a second one is refused with an Error frame and 1013.
    let mut over = connect(&open_url(&h.terminal("box", "bob", &shell(80, 24)).await)).await;
    let (frames, close) = recv_close(&mut over).await;
    assert_eq!(close.code, CloseCode::Again);
    assert!(matches!(&frames[..], [Frame::Error(m)] if m.contains("connection limit")), "{frames:?}");

    let _ = ws.close(None).await;
    h.fake.wait_hung_up(1).await;
    let deadline = Instant::now() + STEP;
    loop {
        let mut again = connect(&open_url(&h.terminal("box", "bob", &shell(80, 24)).await)).await;
        let _ = again.send(Message::Binary(Frame::Stdin(b"y".to_vec()).encode().into())).await;
        match recv_frame(&mut again).await {
            Frame::Stdout(out) => break assert_eq!(out, b"echo:y"),
            Frame::Error(_) => assert!(Instant::now() < deadline, "the slot was never freed"),
            other => panic!("{other:?}"),
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test]
async fn a_sandbox_the_runtime_refuses_ends_the_terminal_with_an_error() {
    let h = host(Setup::default()).await;
    let mut ws = connect(&open_url(&h.terminal("missing", "alice", &shell(80, 24)).await)).await;
    let (frames, close) = recv_close(&mut ws).await;
    assert_eq!(close.code, CloseCode::Error);
    assert!(matches!(&frames[..], [Frame::Error(_)]), "{frames:?}");
}

// ---- chats ---------------------------------------------------------------------------------

#[tokio::test]
async fn a_chat_runs_aho_rpc_in_the_sandbox_and_resumes_the_learned_session() {
    let h = host(Setup::default()).await;
    let chat = h.chat("box", "alice").await;
    assert!(chat.rpc_url.starts_with(&format!("{}/sandbox-rpc/box-", h.ws_base)), "{}", chat.rpc_url);
    assert_eq!(chat.session_id, None);

    let mut ws = connect(&chat_url(&chat)).await;
    let ok = hello(&mut ws).await;
    assert_eq!(ok["session"], NEW_SESSION);
    assert_eq!(ok["argv"], format!("{CHAT_COMMAND} --project /workspace rpc"));
    for id in 1..=3 {
        send_text(&mut ws, json!({"v": 1, "id": id, "method": "session.fold"})).await;
    }
    for id in 1..=3 {
        assert_eq!(recv_json(&mut ws).await["payload"]["id"], id);
    }
    let (sandbox, req) = h.fake.requests().remove(0);
    assert_eq!(sandbox, "box");
    assert_eq!((req.pty, req.cwd.as_deref()), (None, Some("/workspace")));

    // Persisted before hello_ok was forwarded.
    let again = h.chat("box", "alice").await;
    assert_eq!(again.session_id.as_deref(), Some(NEW_SESSION));
    assert_eq!(again.rpc_url, chat.rpc_url);
    assert_eq!(h.chat("box", "bob").await.session_id, None, "per user");

    let _ = ws.close(None).await;
    let (_ws, ok) = hello_when_free(&chat_url(&again)).await;
    assert_eq!(ok["argv"], format!("{CHAT_COMMAND} --project /workspace rpc --session {NEW_SESSION}"));
}

#[tokio::test]
async fn chat_tokens_open_only_their_own_chat() {
    let h = host(Setup::default()).await;
    let alice = h.chat("box", "alice").await;
    let bob = h.chat("box", "bob").await;
    assert_eq!(handshake_status(&alice.rpc_url).await, 401, "no token");
    assert_eq!(handshake_status(&format!("{}?token={}", alice.rpc_url, bob.token)).await, 401, "another user's token");
    let id = alice.rpc_url.rsplit('/').next().unwrap();
    let workspace_token =
        token::mint(&TokenKey::new(SECRET).unwrap(), &Claims { workspace: id.into(), user: "alice".into(), exp: now() + 60 });
    assert_eq!(handshake_status(&format!("{}?token={workspace_token}", alice.rpc_url)).await, 401, "workspace token");
    let mut ws = connect(&chat_url(&alice)).await;
    hello(&mut ws).await;
}

#[tokio::test]
async fn a_newer_chat_connection_takes_over() {
    let h = host(Setup::default()).await;
    let chat = h.chat("box", "alice").await;
    let mut first = connect(&chat_url(&chat)).await;
    hello(&mut first).await;

    let newer = tokio::spawn({
        let url = chat_url(&chat);
        async move {
            let mut ws = connect(&url).await;
            hello(&mut ws).await;
            ws
        }
    });
    let (texts, close) = recv_text_close(&mut first).await;
    assert_eq!(u16::from(close.code), cloud_session_host::bridge::CLOSE_REPLACED, "{close:?}");
    assert_eq!(texts[0]["event"], "error");
    let mut second = tokio::time::timeout(STEP, newer).await.unwrap().unwrap();
    send_text(&mut second, json!({"v": 1, "id": 5, "method": "session.fold"})).await;
    assert_eq!(recv_json(&mut second).await["payload"]["id"], 5);
    assert_eq!(h.fake.requests().len(), 2);
}

#[tokio::test]
async fn the_exec_sessions_exit_maps_to_the_close_code() {
    let h = host(Setup::default()).await;
    // The fake's only stderr line, written at start, is the failure's explanation.
    for (user, method, code, reason, error) in [
        ("u1", "exit", CloseCode::Error, "exited with code 3: aho: starting", true),
        ("u2", "quit", CloseCode::Normal, "exited", false),
        ("u3", "crash", CloseCode::Error, "lost: aho: starting", true),
    ] {
        let mut ws = connect(&chat_url(&h.chat("box", user).await)).await;
        hello(&mut ws).await;
        send_text(&mut ws, json!({"v": 1, "id": 1, "method": "session.fold"})).await;
        send_text(&mut ws, json!({"v": 1, "id": 2, "method": method})).await;
        let (texts, close) = recv_text_close(&mut ws).await;
        assert_eq!(close.code, code, "{method}");
        assert!(close.reason.contains(reason), "{method}: {close:?}");
        assert_eq!(texts[0]["payload"]["id"], 1, "{method}: output before the exit is delivered: {texts:?}");
        assert_eq!(texts.len(), 1 + usize::from(error), "{method}: {texts:?}");
        if error {
            assert_eq!(texts[1]["event"], "error");
            assert_eq!(texts[1]["payload"]["message"], close.reason.as_str(), "{method}");
        }
    }
}

#[tokio::test]
async fn a_failing_aho_tells_the_client_why_from_its_last_stderr_line() {
    let h = host(Setup::default()).await;
    let mut ws = connect(&chat_url(&h.chat("box", "alice").await)).await;
    hello(&mut ws).await;
    send_text(&mut ws, json!({"v": 1, "id": 1, "method": "fail"})).await;
    let (texts, close) = recv_text_close(&mut ws).await;
    let why = "session process exited with code 1: aho: session could not start: no model route `llm:anthropic` is active";
    assert_eq!(texts.len(), 1, "{texts:?}");
    assert_eq!(texts[0]["event"], "error");
    assert_eq!(texts[0]["payload"]["message"], why);
    assert_eq!(close.code, CloseCode::Error);
    // Close reasons are capped at 123 bytes.
    assert!(why.starts_with(close.reason.as_str()) && close.reason.len() > 60, "{close:?}");
}

#[tokio::test]
async fn closing_the_chat_ends_the_exec_session() {
    let h = host(Setup::default()).await;
    let chat = h.chat("box", "alice").await;
    let mut ws = connect(&chat_url(&chat)).await;
    hello(&mut ws).await;
    let _ = ws.close(None).await;
    // Stdin EOF reaches the fake as CloseStdin; it exits 0, so the slot frees for the next one.
    let (_ws, ok) = hello_when_free(&chat_url(&chat)).await;
    assert!(ok["argv"].as_str().unwrap().ends_with(&format!("--session {NEW_SESSION}")), "{ok}");
}

#[tokio::test]
async fn a_chat_child_that_ignores_eof_is_killed_by_closing_its_exec_socket() {
    let h = host(Setup { limits: Limits { kill_grace: Duration::from_millis(200), ..Setup::default().limits }, ..Setup::default() }).await;
    let chat = h.chat("box", "alice").await;
    let mut ws = connect(&chat_url(&chat)).await;
    hello(&mut ws).await;
    send_text(&mut ws, json!({"v": 1, "id": 1, "method": "hang"})).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let closed = Instant::now();
    let _ = ws.close(None).await;
    h.fake.wait_hung_up(1).await;
    assert!(closed.elapsed() >= Duration::from_millis(200), "killed before the grace period ended");
    let (_ws, ok) = hello_when_free(&chat_url(&chat)).await;
    assert_eq!(ok["session"], NEW_SESSION);
}
