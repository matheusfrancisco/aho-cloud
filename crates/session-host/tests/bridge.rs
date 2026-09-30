//! End to end over a real 127.0.0.1 server: the internal workspace route, rpc token checks, and
//! the WebSocket ↔ stdio bridge driving a fake `ahoc` shell script.
#![cfg(unix)]
// Helpers outside `#[test]` fns are test code too.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cloud_api::ApiError;
use cloud_api::internal::WorkspaceStarted;
use cloud_session_host::token::{self, Claims};
use cloud_session_host::workspace::workspace_id;
use cloud_session_host::{CommandTemplate, Config, Limits, SessionHost, TokenKey, TokioSpawner};
use futures::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

const SERVICE_TOKEN: &str = "service-token-for-tests";
const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";
/// The id the fake hands out when it is not resuming.
const NEW_SESSION: &str = "5e55100";
const STEP: Duration = Duration::from_secs(10);

/// Fake `ahoc`: `hello` → `hello_ok` whose session is the `--session` value (or [`NEW_SESSION`])
/// and whose payload also reports argv and the physical cwd; `exit` → exit 3; `quit` → exit 0;
/// `hang` → stop reading stdin forever; anything else is echoed back as a `result` payload.
const FAKE_AHOC: &str = r#"#!/bin/sh
session=""
prev=""
for a in "$@"; do
  if [ "$prev" = "--session" ]; then session="$a"; fi
  prev="$a"
done
[ -n "$session" ] || session="5e55100"
argv="$*"
while IFS= read -r line; do
  case "$line" in
    *'"method":"hello"'*)
      printf '{"v":1,"reply_to":0,"event":"hello_ok","payload":{"version":1,"session":"%s","argv":"%s","cwd":"%s"}}\n' "$session" "$argv" "$(pwd -P)" ;;
    *'"method":"exit"'*) exit 3 ;;
    *'"method":"quit"'*) exit 0 ;;
    *'"method":"hang"'*) while :; do sleep 1; done ;;
    *) printf '{"v":1,"reply_to":null,"event":"result","payload":%s}\n' "$line" ;;
  esac
done
"#;

type Ws = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

struct Host {
    http: reqwest::Client,
    base: String,
    ws_base: String,
    data: PathBuf,
    _dir: tempfile::TempDir,
}

fn fast() -> Limits {
    Limits { kill_grace: Duration::from_millis(300), ..Limits::DEFAULT }
}

async fn host(limits: Limits) -> Host {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("fake-ahoc");
    std::fs::write(&script, FAKE_AHOC).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let data = dir.path().join("data");
    let config = Config {
        data_root: data.clone(),
        public_url: format!("ws://{addr}/"),
        preview_url: "https://{project}.preview.test".into(),
        service_token: SERVICE_TOKEN.into(),
        token_key: TokenKey::new(SECRET).unwrap(),
        token_ttl: Duration::from_secs(60),
        command: CommandTemplate { program: script, sets: vec!["seams.world=vm".into()], ..CommandTemplate::default() },
        limits,
    };
    let host = SessionHost::new(config, Arc::new(TokioSpawner)).unwrap();
    tokio::spawn(host.serve(listener, std::future::pending()));
    Host { http: reqwest::Client::new(), base: format!("http://{addr}"), ws_base: format!("ws://{addr}"), data, _dir: dir }
}

impl Host {
    async fn post(&self, bearer: Option<&str>, body: String) -> reqwest::Response {
        let mut req = self
            .http
            .post(format!("{}{}", self.base, cloud_api::internal::routes::WORKSPACES))
            .header("content-type", "application/json")
            .body(body);
        if let Some(b) = bearer {
            req = req.bearer_auth(b);
        }
        req.send().await.unwrap()
    }

    async fn start_with(&self, project: &str, user: &str, session: Option<&str>) -> WorkspaceStarted {
        let body = json!({ "project": project, "user_id": user, "session_id": session }).to_string();
        let resp = self.post(Some(SERVICE_TOKEN), body).await;
        assert_eq!(resp.status(), 200);
        resp.json().await.unwrap()
    }

    async fn start(&self, project: &str, user: &str) -> WorkspaceStarted {
        self.start_with(project, user, None).await
    }

    fn project_dir(&self, started: &WorkspaceStarted) -> PathBuf {
        self.data.join("workspaces").join(id_of(started)).join("project")
    }
}

fn id_of(started: &WorkspaceStarted) -> &str {
    started.rpc_url.rsplit('/').next().unwrap()
}

fn url(started: &WorkspaceStarted) -> String {
    format!("{}?token={}", started.rpc_url, started.token)
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

fn hello_frame() -> Value {
    json!({"v": 1, "id": 0, "method": "hello", "params": {"min": 1, "max": 1, "client_instance": "test", "has_local_history": false, "allow_takeover": false}})
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

async fn send_text(ws: &mut Ws, text: String) {
    ws.send(Message::Text(text.into())).await.unwrap();
}

async fn next(ws: &mut Ws) -> Message {
    tokio::time::timeout(STEP, ws.next()).await.expect("timed out").expect("stream ended").expect("websocket error")
}

async fn recv_json(ws: &mut Ws) -> Value {
    match next(ws).await {
        Message::Text(t) => serde_json::from_str(&t).unwrap(),
        other => panic!("expected a text frame, got {other:?}"),
    }
}

/// Text frames received before the close frame, and the close frame.
async fn recv_close(ws: &mut Ws) -> (Vec<Value>, CloseFrame) {
    let mut texts = Vec::new();
    loop {
        match next(ws).await {
            Message::Text(t) => texts.push(serde_json::from_str(&t).unwrap()),
            Message::Close(Some(frame)) => return (texts, frame),
            Message::Ping(_) | Message::Pong(_) => {}
            other => panic!("expected a close frame, got {other:?}"),
        }
    }
}

async fn hello(ws: &mut Ws) -> Value {
    send_text(ws, hello_frame().to_string()).await;
    let frame = recv_json(ws).await;
    assert_eq!(frame["event"], "hello_ok", "{frame}");
    frame["payload"].clone()
}

/// Connects and says hello, retrying while the host is at its connection limit (refused with
/// 1013) because an earlier connection is still being torn down.
async fn hello_when_free(url: &str) -> (Ws, Value) {
    let deadline = Instant::now() + STEP;
    loop {
        let mut ws = connect(url).await;
        let _ = ws.send(Message::Text(hello_frame().to_string().into())).await;
        match next(&mut ws).await {
            Message::Text(t) => {
                let frame: Value = serde_json::from_str(&t).unwrap();
                if frame["event"] == "hello_ok" {
                    return (ws, frame["payload"].clone());
                }
                assert_eq!(frame["event"], "error", "{frame}");
                let (_, close) = recv_close(&mut ws).await;
                assert_eq!(close.code, CloseCode::Again, "{close:?}");
            }
            Message::Close(Some(close)) => assert_eq!(close.code, CloseCode::Again, "{close:?}"),
            other => panic!("unexpected {other:?}"),
        }
        assert!(Instant::now() < deadline, "the workspace never became free");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn close_and_drain(mut ws: Ws) {
    let _ = ws.close(None).await;
    let _ = tokio::time::timeout(STEP, async { while let Some(Ok(_)) = ws.next().await {} }).await;
}

// ---- workspace route -----------------------------------------------------------------------

#[tokio::test]
async fn workspace_route_requires_the_service_token_and_valid_input() {
    let h = host(fast()).await;
    let body = || json!({ "project": "shop", "user_id": "alice" }).to_string();

    for bearer in [None, Some("service-token-for-test"), Some("SERVICE-TOKEN-FOR-TESTS")] {
        let resp = h.post(bearer, body()).await;
        assert_eq!(resp.status(), 401, "{bearer:?}");
        assert_eq!(resp.json::<ApiError>().await.unwrap().code, "unauthorized");
    }
    for bad in [
        "{nope".to_owned(),
        json!({ "project": "Shop", "user_id": "alice" }).to_string(),
        json!({ "project": "../etc", "user_id": "alice" }).to_string(),
        json!({ "project": "shop", "user_id": "" }).to_string(),
        json!({ "project": "shop", "user_id": "alice", "session_id": "--help" }).to_string(),
    ] {
        let resp = h.post(Some(SERVICE_TOKEN), bad.clone()).await;
        assert_eq!(resp.status(), 400, "{bad}");
        assert_eq!(resp.json::<ApiError>().await.unwrap().code, "invalid");
    }
    assert!(!h.data.join("workspaces").exists(), "refused requests create nothing");

    let started = h.start("shop", "alice").await;
    let id = workspace_id("shop", "alice");
    assert_eq!(started.rpc_url, format!("{}/rpc/{id}", h.ws_base));
    assert_eq!(started.preview_url, "https://shop.preview.test");
    assert_eq!(started.session_id, None, "no session until the first hello_ok");
    assert!(h.project_dir(&started).is_dir());
    let key = TokenKey::new(SECRET).unwrap();
    let claims = token::verify(&key, &started.token, &id, now()).unwrap();
    assert_eq!(claims.user, "alice");
    assert!(claims.exp > now() && claims.exp <= now() + 60);
    // Same pair, same workspace.
    assert_eq!(id_of(&h.start("shop", "alice").await), id);
    assert_ne!(id_of(&h.start("shop", "bob").await), id);
}

#[tokio::test]
async fn rpc_upgrade_checks_the_token_first() {
    let h = host(fast()).await;
    let alice = h.start("shop", "alice").await;
    let bob = h.start("shop", "bob").await;
    let key = TokenKey::new(SECRET).unwrap();

    assert_eq!(handshake_status(&alice.rpc_url).await, 401, "no token");
    assert_eq!(handshake_status(&format!("{}?token={}", alice.rpc_url, bob.token)).await, 401, "another workspace's token");
    let mut tampered = alice.token.clone().into_bytes();
    tampered[3] = if tampered[3] == b'x' { b'y' } else { b'x' };
    let tampered = String::from_utf8(tampered).unwrap();
    assert_eq!(handshake_status(&format!("{}?token={tampered}", alice.rpc_url)).await, 401, "tampered");
    let expired = token::mint(&key, &Claims { workspace: id_of(&alice).into(), user: "alice".into(), exp: now() - 1 });
    assert_eq!(handshake_status(&format!("{}?token={expired}", alice.rpc_url)).await, 401, "expired");
    let foreign = TokenKey::new(b"ffffffffffffffffffffffffffffffff").unwrap();
    let forged = token::mint(&foreign, &Claims { workspace: id_of(&alice).into(), user: "alice".into(), exp: now() + 60 });
    assert_eq!(handshake_status(&format!("{}?token={forged}", alice.rpc_url)).await, 401, "wrong key");

    let ghost = workspace_id("ghost", "alice");
    let valid = token::mint(&key, &Claims { workspace: ghost.clone(), user: "alice".into(), exp: now() + 60 });
    assert_eq!(handshake_status(&format!("{}/rpc/{ghost}?token={valid}", h.ws_base)).await, 404, "never started");

    // The genuine token still works.
    let mut ws = connect(&url(&alice)).await;
    assert_eq!(hello(&mut ws).await["session"], NEW_SESSION);
}

// ---- bridge --------------------------------------------------------------------------------

#[tokio::test]
async fn frames_flow_both_ways_and_the_learned_session_is_resumed() {
    let h = host(fast()).await;
    let started = h.start("shop", "alice").await;
    let dir = h.project_dir(&started);

    let mut ws = connect(&url(&started)).await;
    let ok = hello(&mut ws).await;
    assert_eq!(ok["session"], NEW_SESSION);
    assert_eq!(ok["argv"], format!("--set seams.world=vm --project {} rpc", dir.display()));
    assert_eq!(PathBuf::from(ok["cwd"].as_str().unwrap()), std::fs::canonicalize(&dir).unwrap());

    // Pipelined requests come back one message per line, in order, byte-for-byte as payloads.
    for id in 1..=3 {
        send_text(&mut ws, json!({"v": 1, "id": id, "method": "session.fold", "params": {"note": "a\\nb \u{e9}"}}).to_string()).await;
    }
    for id in 1..=3 {
        let frame = recv_json(&mut ws).await;
        assert_eq!(frame["event"], "result");
        assert_eq!(frame["payload"], json!({"v": 1, "id": id, "method": "session.fold", "params": {"note": "a\\nb \u{e9}"}}));
    }

    // Persisted before hello_ok was forwarded: the control plane sees it right away.
    assert_eq!(h.start("shop", "alice").await.session_id.as_deref(), Some(NEW_SESSION));

    close_and_drain(ws).await;
    let again = h.start("shop", "alice").await;
    let (_ws, ok) = hello_when_free(&url(&again)).await;
    assert_eq!(ok["argv"], format!("--set seams.world=vm --project {} rpc --session {NEW_SESSION}", dir.display()));
    assert_eq!(ok["session"], NEW_SESSION);
}

#[tokio::test]
async fn a_session_named_at_start_is_resumed() {
    let h = host(fast()).await;
    let started = h.start_with("shop", "carol", Some("abc123")).await;
    assert_eq!(started.session_id.as_deref(), Some("abc123"));
    let mut ws = connect(&url(&started)).await;
    let ok = hello(&mut ws).await;
    assert!(ok["argv"].as_str().unwrap().ends_with("rpc --session abc123"), "{ok}");
    assert_eq!(ok["session"], "abc123");
}

#[tokio::test]
async fn the_first_frame_must_be_hello() {
    let h = host(fast()).await;
    let started = h.start("shop", "alice").await;
    let mut ws = connect(&url(&started)).await;
    send_text(&mut ws, json!({"v": 1, "id": 1, "method": "session.prompt", "params": {"text": "hi"}}).to_string()).await;
    let (texts, close) = recv_close(&mut ws).await;
    assert_eq!(close.code, CloseCode::Policy);
    assert_eq!(texts.len(), 1);
    assert_eq!(texts[0]["event"], "error");
    assert!(texts[0]["payload"]["message"].as_str().unwrap().contains("hello"), "{}", texts[0]);
    // The close handshake completes (browsers report `wasClean`): the stream ends without a reset.
    let after = tokio::time::timeout(STEP, ws.next()).await.expect("timed out");
    assert!(after.is_none(), "{after:?}");
    // Nothing was spawned, so nothing was learned.
    assert_eq!(h.start("shop", "alice").await.session_id, None);
}

#[tokio::test]
async fn malformed_frames_after_hello_close_the_socket() {
    let h = host(fast()).await;
    let cases: [(&str, Message, CloseCode); 4] = [
        ("multi-line", Message::Text("{\"v\":1,\n\"id\":2,\"method\":\"session.fold\"}".into()), CloseCode::Policy),
        ("two objects on two lines", Message::Text("{\"id\":2}\n{\"id\":3}".into()), CloseCode::Policy),
        ("not an object", Message::Text("[1,2,3]".into()), CloseCode::Policy),
        ("binary", Message::Binary(b"{\"id\":2}".to_vec().into()), CloseCode::Unsupported),
    ];
    for (i, (what, msg, code)) in cases.into_iter().enumerate() {
        let started = h.start("shop", &format!("user{i}")).await;
        let mut ws = connect(&url(&started)).await;
        hello(&mut ws).await;
        ws.send(msg).await.unwrap();
        let (texts, close) = recv_close(&mut ws).await;
        assert_eq!(close.code, code, "{what}");
        assert!(texts.iter().all(|t| t["event"] == "error"), "{what}: nothing reached the child: {texts:?}");
    }
}

#[tokio::test]
async fn oversized_frames_are_refused() {
    let h = host(fast()).await;
    let max = Limits::DEFAULT.max_frame_bytes;
    let frame_of = |len: usize| {
        let head = r#"{"v":1,"id":1,"method":"hello","params":""#;
        format!("{head}{}\"}}", "a".repeat(len - head.len() - 2))
    };
    assert_eq!(frame_of(max + 1).len(), max + 1);

    // As the first frame: refused before anything is spawned.
    let first = h.start("shop", "u1").await;
    let mut ws = connect(&url(&first)).await;
    send_text(&mut ws, frame_of(max + 1)).await;
    let (_, close) = recv_close(&mut ws).await;
    assert_eq!(close.code, CloseCode::Size);

    // After hello.
    let later = h.start("shop", "u2").await;
    let mut ws = connect(&url(&later)).await;
    hello(&mut ws).await;
    send_text(&mut ws, frame_of(max + 1)).await;
    let (texts, close) = recv_close(&mut ws).await;
    assert_eq!(close.code, CloseCode::Size);
    assert!(texts.iter().all(|t| t["event"] == "error"), "{texts:?}");

    // Far beyond the limit the transport drops the connection; nothing is echoed.
    let huge = h.start("shop", "u3").await;
    let mut ws = connect(&url(&huge)).await;
    hello(&mut ws).await;
    let _ = ws.send(Message::Text(frame_of(3 * max).into())).await;
    let got = tokio::time::timeout(STEP, ws.next()).await.expect("timed out");
    assert!(!matches!(got, Some(Ok(Message::Text(_)))), "{got:?}");
}

#[tokio::test]
async fn child_exit_closes_the_socket() {
    let h = host(fast()).await;

    let failing = h.start("shop", "u1").await;
    let mut ws = connect(&url(&failing)).await;
    hello(&mut ws).await;
    send_text(&mut ws, json!({"v": 1, "id": 1, "method": "exit"}).to_string()).await;
    let (texts, close) = recv_close(&mut ws).await;
    assert!(texts.is_empty(), "{texts:?}");
    assert_eq!(close.code, CloseCode::Error);
    assert!(close.reason.contains("code 3"), "{close:?}");

    let clean = h.start("shop", "u2").await;
    let mut ws = connect(&url(&clean)).await;
    hello(&mut ws).await;
    send_text(&mut ws, json!({"v": 1, "id": 1, "method": "session.fold"}).to_string()).await;
    send_text(&mut ws, json!({"v": 1, "id": 2, "method": "quit"}).to_string()).await;
    let (texts, close) = recv_close(&mut ws).await;
    assert_eq!(texts.len(), 1, "output written before exit is still delivered: {texts:?}");
    assert_eq!(texts[0]["payload"]["id"], 1);
    assert_eq!(close.code, CloseCode::Normal);
}

#[tokio::test]
async fn a_newer_connection_takes_the_workspace_over_and_the_host_limit_refuses() {
    let h = host(Limits { max_connections: 2, max_per_workspace: 1, ..fast() }).await;
    let one = h.start("shop", "u1").await;
    let two = h.start("shop", "u2").await;
    let three = h.start("shop", "u3").await;

    let mut a = connect(&url(&one)).await;
    hello(&mut a).await;

    // A reload or second tab: the newer connection wins, the old one is told why and closed,
    // and the new one is admitted once the old child is gone (one writer per session).
    let newer = tokio::spawn({
        let url = url(&one);
        async move {
            let mut ws = connect(&url).await;
            let ok = hello(&mut ws).await;
            (ws, ok)
        }
    });
    let (texts, close) = recv_close(&mut a).await;
    assert_eq!(u16::from(close.code), cloud_session_host::bridge::CLOSE_REPLACED, "{close:?}");
    assert_eq!(texts[0]["event"], "error");
    assert!(texts[0]["payload"]["message"].as_str().unwrap().contains("another window"), "{texts:?}");
    let (mut same_workspace, _) = tokio::time::timeout(STEP, newer).await.unwrap().unwrap();
    send_text(&mut same_workspace, json!({"v": 1, "id": 7, "method": "session.fold"}).to_string()).await;
    assert_eq!(recv_json(&mut same_workspace).await["payload"]["id"], 7);

    let mut b = connect(&url(&two)).await;
    hello(&mut b).await;

    // The host limit never replaces anyone: refused right away with 1013.
    let mut over_host = connect(&url(&three)).await;
    let (_, close) = recv_close(&mut over_host).await;
    assert_eq!(close.code, CloseCode::Again);
    assert!(close.reason.contains("connection limit"), "{close:?}");

    // Closing a connection frees its slot for workspace three.
    close_and_drain(same_workspace).await;
    let (_c, ok) = hello_when_free(&url(&three)).await;
    assert_eq!(ok["session"], NEW_SESSION);
    // `b` is unaffected throughout.
    send_text(&mut b, json!({"v": 1, "id": 9, "method": "session.fold"}).to_string()).await;
    assert_eq!(recv_json(&mut b).await["payload"]["id"], 9);
}

#[tokio::test]
async fn idle_connections_are_closed_and_traffic_keeps_them_open() {
    // Wide margins: each round trip gets most of the idle period even on a loaded machine.
    let idle = Duration::from_millis(1500);
    let h = host(Limits { idle_timeout: idle, ..fast() }).await;
    let started = h.start("shop", "alice").await;
    let mut ws = connect(&url(&started)).await;
    hello(&mut ws).await;

    let opened = Instant::now();
    for id in 1..=6 {
        tokio::time::sleep(idle / 5).await;
        send_text(&mut ws, json!({"v": 1, "id": id, "method": "session.fold"}).to_string()).await;
        assert_eq!(recv_json(&mut ws).await["payload"]["id"], id);
    }
    assert!(opened.elapsed() > idle, "traffic kept the connection past one idle period");

    let (_, close) = recv_close(&mut ws).await;
    assert_eq!(close.code, CloseCode::Away);
    assert_eq!(close.reason.as_str(), "idle timeout");
}

#[tokio::test]
async fn a_child_that_ignores_eof_is_killed_after_the_grace_period() {
    let h = host(Limits { kill_grace: Duration::from_millis(200), ..fast() }).await;
    let started = h.start("shop", "alice").await;
    let mut ws = connect(&url(&started)).await;
    hello(&mut ws).await;
    send_text(&mut ws, json!({"v": 1, "id": 1, "method": "hang"}).to_string()).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let closed = Instant::now();
    close_and_drain(ws).await;
    // The slot is only released once the child is gone; it never exits on its own.
    let (_ws, ok) = hello_when_free(&url(&started)).await;
    assert_eq!(ok["session"], NEW_SESSION);
    assert!(closed.elapsed() >= Duration::from_millis(200), "freed before the grace period ended");
}
