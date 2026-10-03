//! The internal HTTP API end to end: service-token auth, validation, and status transitions;
//! the sandbox routes, file reads through the real `cloud-guest`, and the exec WebSocket.
// Helpers outside `#[test]` fns are test code too.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use cloud_api::exec::{ExecRequest, Frame, MAX_FRAME, PtySize};
use cloud_api::internal::{DeploymentStatus, RunState, SandboxStatus, routes};
use cloud_api::{ApiError, FileContent, FileTree, SandboxState};
use cloud_runtime::egress::EgressPolicies;
use cloud_runtime::net::Allocator;
use cloud_runtime::{Supervisor, SupervisorConfig, api};
use common::{DIGEST_1, TestVmm, deployment, sandbox};
use futures::{SinkExt, StreamExt};
use reqwest::{Client, Method, StatusCode};
use serde_json::json;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::{self, Message};

const TOKEN: &str = "svc-token-3f9a";
const SB: &str = "dev-box";

struct Server {
    base: String,
    http: Client,
    sup: Supervisor<TestVmm>,
    vmm: TestVmm,
    /// Holds the sandbox workspace and the `cloud-guest` wrapper.
    tmp: tempfile::TempDir,
}

impl Server {
    async fn start() -> Server {
        // `cloud-guest` as the fake guest runs it: this build, with `/workspace` mapped to a
        // temp dir (and refusing any other root).
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("workspace");
        std::fs::create_dir_all(ws.join("src")).unwrap();
        std::fs::create_dir_all(ws.join(".git")).unwrap();
        std::fs::write(ws.join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(ws.join(".git/HEAD"), "ref: x").unwrap();
        let wrapper = tmp.path().join("cloud-guest");
        let script = format!(
            "#!/bin/sh\nsub=\"$1\"\n[ \"$2\" = --root ] && [ \"$3\" = /workspace ] || {{ echo \"root $3\" >&2; exit 99; }}\nshift 3\nexec '{}' \"$sub\" --root '{}' \"$@\"\n",
            env!("CARGO_BIN_EXE_cloud-guest"),
            ws.display()
        );
        std::fs::write(&wrapper, script).unwrap();
        std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755)).unwrap();

        let allocator = Allocator::new("10.77.0.0/24".parse().unwrap()).unwrap();
        let cfg = SupervisorConfig { health_timeout: Duration::from_millis(500), startup_grace: Duration::ZERO, ..Default::default() };
        let vmm = TestVmm { guest_program: wrapper.display().to_string(), ..TestVmm::default() };
        let sup = Supervisor::new(vmm.clone(), allocator, EgressPolicies::default(), cfg);
        let app = api::router(sup.clone(), TOKEN);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        Server { base, http: Client::new(), sup, vmm, tmp }
    }

    fn url(&self, route: &str, project: &str, stage: &str) -> String {
        format!("{}{}", self.base, cloud_api::routes::fill(route, &[("p", project), ("stage", stage)]))
    }

    fn sandbox_url(&self, route: &str, slug: &str) -> String {
        format!("{}{}", self.base, cloud_api::routes::fill(route, &[("s", slug)]))
    }

    async fn call(&self, method: Method, route: &str, stage: &str, body: Option<serde_json::Value>) -> reqwest::Response {
        let mut req = self.http.request(method, self.url(route, "acme-shop", stage)).bearer_auth(TOKEN);
        if let Some(body) = body {
            req = req.json(&body);
        }
        req.send().await.unwrap()
    }

    async fn status(&self, method: Method, route: &str, body: Option<serde_json::Value>) -> DeploymentStatus {
        let res = self.call(method, route, "live", body).await;
        assert_eq!(res.status(), StatusCode::OK);
        res.json().await.unwrap()
    }

    async fn sandbox_call(&self, method: Method, route: &str, slug: &str, body: Option<serde_json::Value>) -> reqwest::Response {
        let mut req = self.http.request(method, self.sandbox_url(route, slug)).bearer_auth(TOKEN);
        if let Some(body) = body {
            req = req.json(&body);
        }
        req.send().await.unwrap()
    }

    async fn sandbox_status(&self, method: Method, route: &str, body: Option<serde_json::Value>) -> SandboxStatus {
        let res = self.sandbox_call(method, route, SB, body).await;
        assert_eq!(res.status(), StatusCode::OK);
        res.json().await.unwrap()
    }

    /// A running sandbox `SB`.
    async fn running_sandbox(&self) {
        self.sandbox_status(Method::PUT, routes::SANDBOX, Some(serde_json::to_value(sandbox(SB, DIGEST_1)).unwrap())).await;
        self.sup.reconcile().await;
        assert_eq!(self.sup.sandbox_status(SB).unwrap().state, SandboxState::Running);
    }

    async fn exec_socket(&self, slug: &str, token: Option<&str>) -> Result<Socket, tungstenite::Error> {
        let url = self.sandbox_url(routes::SANDBOX_EXEC, slug).replacen("http://", "ws://", 1);
        let mut request = url.into_client_request().unwrap();
        if let Some(token) = token {
            request.headers_mut().insert("authorization", format!("Bearer {token}").parse().unwrap());
        }
        tokio_tungstenite::connect_async(request).await.map(|(socket, _)| socket)
    }
}

type Socket = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn error(res: reqwest::Response, status: StatusCode) -> ApiError {
    assert_eq!(res.status(), status);
    res.json().await.unwrap()
}

#[tokio::test]
async fn every_route_requires_the_service_token() {
    let s = Server::start().await;
    let body = serde_json::to_value(deployment("r1", DIGEST_1)).unwrap();
    let sandbox_body = serde_json::to_value(sandbox(SB, DIGEST_1)).unwrap();
    let deployment_routes = [
        (Method::PUT, routes::DEPLOYMENT),
        (Method::GET, routes::DEPLOYMENT),
        (Method::DELETE, routes::DEPLOYMENT),
        (Method::POST, routes::DEPLOYMENT_RESTART),
        (Method::POST, routes::DEPLOYMENT_SCALE),
        (Method::POST, routes::DEPLOYMENT_MAINTENANCE),
        (Method::POST, routes::DEPLOYMENT_SLEEP),
    ]
    .map(|(m, r)| (m, s.url(r, "acme-shop", "live"), body.clone()));
    let sandbox_routes = [
        (Method::PUT, routes::SANDBOX),
        (Method::GET, routes::SANDBOX),
        (Method::DELETE, routes::SANDBOX),
        (Method::POST, routes::SANDBOX_STOP),
        (Method::GET, routes::SANDBOX_FILES),
        (Method::GET, routes::SANDBOX_FILE),
        (Method::GET, routes::SANDBOX_EXEC),
    ]
    .map(|(m, r)| (m, s.sandbox_url(r, SB), sandbox_body.clone()));
    for (method, url, body) in deployment_routes.into_iter().chain(sandbox_routes) {
        let anonymous = s.http.request(method.clone(), &url).json(&body).send().await.unwrap();
        assert_eq!(error(anonymous, StatusCode::UNAUTHORIZED).await.code, "unauthorized", "{method} {url}");
        let wrong = s.http.request(method.clone(), &url).bearer_auth("svc-token-3f9b").json(&body).send().await.unwrap();
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
        let empty = s.http.request(method.clone(), &url).header("authorization", "Bearer ").send().await.unwrap();
        assert_eq!(empty.status(), StatusCode::UNAUTHORIZED);
        let basic = s.http.request(method.clone(), &url).header("authorization", format!("Basic {TOKEN}")).send().await.unwrap();
        assert_eq!(basic.status(), StatusCode::UNAUTHORIZED);
    }
    assert!(s.sup.status("acme-shop", cloud_contract::ledger::Stage::Live).is_err(), "nothing was applied");
    assert!(s.sup.sandbox_status(SB).is_err(), "no sandbox was created");

    // The exec WebSocket refuses the upgrade itself, even for a running sandbox.
    s.running_sandbox().await;
    for token in [None, Some("svc-token-3f9b")] {
        match s.exec_socket(SB, token).await {
            Err(tungstenite::Error::Http(res)) => assert_eq!(res.status(), StatusCode::UNAUTHORIZED),
            other => panic!("{token:?}: {:?}", other.map(|_| ())),
        }
    }
    assert!(s.vmm.log().iter().all(|l| !l.starts_with("exec")));
}

#[tokio::test]
async fn an_empty_configured_token_admits_nobody() {
    let allocator = Allocator::new("10.77.0.0/24".parse().unwrap()).unwrap();
    let sup = Supervisor::new(TestVmm::default(), allocator, EgressPolicies::default(), SupervisorConfig::default());
    let app = api::router(sup, "");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://{}{}",
        listener.local_addr().unwrap(),
        cloud_api::routes::fill(routes::DEPLOYMENT, &[("p", "x"), ("stage", "live")])
    );
    tokio::spawn(async move { axum::serve(listener, app).await });
    let res = Client::new().get(&url).header("authorization", "Bearer ").send().await.unwrap();
    assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn put_validates_the_deployment() {
    let s = Server::start().await;
    let good = serde_json::to_value(deployment("r1", DIGEST_1)).unwrap();

    let res = s.call(Method::PUT, routes::DEPLOYMENT, "staging", Some(good.clone())).await;
    assert_eq!(error(res, StatusCode::BAD_REQUEST).await.code, "invalid", "stage mismatch");
    let res = s.http.put(s.url(routes::DEPLOYMENT, "other", "live")).bearer_auth(TOKEN).json(&good).send().await.unwrap();
    assert_eq!(error(res, StatusCode::BAD_REQUEST).await.code, "invalid", "project mismatch");

    let mut bad_digest = good.clone();
    bad_digest["image_digest"] = json!("latest");
    let res = s.call(Method::PUT, routes::DEPLOYMENT, "live", Some(bad_digest)).await;
    assert_eq!(error(res, StatusCode::BAD_REQUEST).await.code, "invalid");

    let mut refused = good.clone();
    refused["manifest"]["egress"] = json!([]);
    let res = s.call(Method::PUT, routes::DEPLOYMENT, "live", Some(refused)).await;
    let e = error(res, StatusCode::BAD_REQUEST).await;
    assert_eq!(e.code, "refused");
    assert!(!e.refusals.is_empty(), "a secret's reach must be covered by egress");

    let res = s.call(Method::PUT, routes::DEPLOYMENT, "live", Some(json!({"release": "r1"}))).await;
    assert_eq!(error(res, StatusCode::BAD_REQUEST).await.code, "invalid");

    let res = s.call(Method::GET, routes::DEPLOYMENT, "nightly", None).await;
    assert_eq!(error(res, StatusCode::NOT_FOUND).await.code, "not_found");
}

#[tokio::test]
async fn status_moves_through_the_lifecycle() {
    let s = Server::start().await;
    let res = s.call(Method::GET, routes::DEPLOYMENT, "live", None).await;
    assert_eq!(error(res, StatusCode::NOT_FOUND).await.code, "not_found");

    let body = serde_json::to_value(deployment("r1", DIGEST_1)).unwrap();
    let st = s.status(Method::PUT, routes::DEPLOYMENT, Some(body.clone())).await;
    assert_eq!((st.state, st.release.as_str()), (RunState::Starting, "r1"));
    assert_eq!(s.status(Method::GET, routes::DEPLOYMENT, None).await.state, RunState::Starting);

    s.sup.reconcile().await;
    let st = s.status(Method::GET, routes::DEPLOYMENT, None).await;
    assert_eq!(st.state, RunState::Running);
    let web = st.processes.iter().find(|p| p.name == "web").unwrap();
    assert_eq!((web.replicas, web.healthy, web.addresses.len()), (1, 1, 1));

    let on = s.status(Method::POST, routes::DEPLOYMENT_MAINTENANCE, Some(json!({"on": true}))).await;
    assert_eq!(on.state, RunState::Maintenance);
    let off = s.status(Method::POST, routes::DEPLOYMENT_MAINTENANCE, Some(json!({"on": false}))).await;
    assert_eq!(off.state, RunState::Running);

    let scaled = s.status(Method::POST, routes::DEPLOYMENT_SCALE, Some(json!({"process": "web", "replicas": 2}))).await;
    assert_eq!(scaled.state, RunState::Starting);
    s.sup.reconcile().await;
    let st = s.status(Method::GET, routes::DEPLOYMENT, None).await;
    assert_eq!(st.processes.iter().find(|p| p.name == "web").unwrap().replicas, 2);
    let res = s.call(Method::POST, routes::DEPLOYMENT_SCALE, "live", Some(json!({"process": "ghost", "replicas": 1}))).await;
    assert_eq!(error(res, StatusCode::NOT_FOUND).await.code, "not_found");
    let res = s.call(Method::POST, routes::DEPLOYMENT_SCALE, "live", Some(json!({"process": "web", "replicas": 999}))).await;
    assert_eq!(error(res, StatusCode::BAD_REQUEST).await.code, "invalid");

    assert_eq!(s.status(Method::POST, routes::DEPLOYMENT_RESTART, None).await.state, RunState::Starting);
    s.sup.reconcile().await;
    s.sup.reconcile().await;
    assert_eq!(s.status(Method::GET, routes::DEPLOYMENT, None).await.state, RunState::Running);

    assert_eq!(s.status(Method::POST, routes::DEPLOYMENT_SLEEP, None).await.state, RunState::Sleeping);
    s.sup.reconcile().await;
    let st = s.status(Method::GET, routes::DEPLOYMENT, None).await;
    assert_eq!(st.state, RunState::Sleeping);
    assert!(st.processes.iter().all(|p| p.replicas == 0));

    // A new PUT wakes it.
    assert_eq!(s.status(Method::PUT, routes::DEPLOYMENT, Some(body)).await.state, RunState::Starting);

    let res = s.call(Method::DELETE, routes::DEPLOYMENT, "live", None).await;
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    let res = s.call(Method::GET, routes::DEPLOYMENT, "live", None).await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let res = s.call(Method::POST, routes::DEPLOYMENT_SLEEP, "live", None).await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn put_validates_the_sandbox() {
    let s = Server::start().await;
    let good = serde_json::to_value(sandbox(SB, DIGEST_1)).unwrap();
    let res = s.sandbox_call(Method::PUT, routes::SANDBOX, "other-box", Some(good.clone())).await;
    assert_eq!(error(res, StatusCode::BAD_REQUEST).await.code, "invalid", "slug mismatch");
    for (field, value) in [
        ("image_digest", json!("latest")),
        ("image_repo", json!("../escape")),
        ("image_repo", json!("")),
        ("egress", json!(["not a host"])),
        ("env", json!({"BAD-NAME": "x"})),
        ("cpus", json!(0)),
        ("port", json!(0)),
    ] {
        let mut bad = good.clone();
        bad[field] = value.clone();
        let res = s.sandbox_call(Method::PUT, routes::SANDBOX, SB, Some(bad)).await;
        assert_eq!(error(res, StatusCode::BAD_REQUEST).await.code, "invalid", "{field} = {value}");
    }
    let mut bad_slug = good.clone();
    bad_slug["slug"] = json!("Dev.Box");
    let res = s.sandbox_call(Method::PUT, routes::SANDBOX, "Dev.Box", Some(bad_slug)).await;
    assert_eq!(error(res, StatusCode::BAD_REQUEST).await.code, "invalid");
    assert!(s.sup.sandbox_status(SB).is_err(), "nothing was recorded");
}

#[tokio::test]
async fn sandbox_status_moves_through_its_lifecycle() {
    let s = Server::start().await;
    let res = s.sandbox_call(Method::GET, routes::SANDBOX, SB, None).await;
    assert_eq!(error(res, StatusCode::NOT_FOUND).await.code, "not_found");
    let res = s.sandbox_call(Method::POST, routes::SANDBOX_STOP, SB, None).await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);

    let body = serde_json::to_value(sandbox(SB, DIGEST_1)).unwrap();
    let st = s.sandbox_status(Method::PUT, routes::SANDBOX, Some(body.clone())).await;
    assert_eq!((st.slug.as_str(), st.state, st.address), (SB, SandboxState::Starting, None));
    let res = s.sandbox_call(Method::GET, routes::SANDBOX_FILES, SB, None).await;
    assert_eq!(error(res, StatusCode::CONFLICT).await.code, "not_running");

    s.sup.reconcile().await;
    let st = s.sandbox_status(Method::GET, routes::SANDBOX, None).await;
    assert_eq!(st.state, SandboxState::Running);
    let id = s.vmm.sandbox_ids(SB).pop().unwrap();
    assert_eq!(st.address, Some(s.vmm.endpoint_of(&id).to_string()));

    let st = s.sandbox_status(Method::POST, routes::SANDBOX_STOP, None).await;
    assert_eq!((st.state, st.address), (SandboxState::Stopped, None));
    s.sup.reconcile().await;
    assert!(s.vmm.sandbox_ids(SB).is_empty());
    let file_url = format!("{}?path=src/main.rs", s.sandbox_url(routes::SANDBOX_FILE, SB));
    let res = s.http.get(file_url).bearer_auth(TOKEN).send().await.unwrap();
    assert_eq!(error(res, StatusCode::CONFLICT).await.code, "not_running");
    match s.exec_socket(SB, Some(TOKEN)).await {
        Err(tungstenite::Error::Http(res)) => assert_eq!(res.status(), StatusCode::CONFLICT),
        other => panic!("{:?}", other.map(|_| ())),
    }

    assert_eq!(s.sandbox_status(Method::PUT, routes::SANDBOX, Some(body)).await.state, SandboxState::Starting);
    s.sup.reconcile().await;
    assert_eq!(s.sandbox_status(Method::GET, routes::SANDBOX, None).await.state, SandboxState::Running);

    let res = s.sandbox_call(Method::DELETE, routes::SANDBOX, SB, None).await;
    assert_eq!(res.status(), StatusCode::NO_CONTENT);
    let res = s.sandbox_call(Method::GET, routes::SANDBOX, SB, None).await;
    assert_eq!(res.status(), StatusCode::NOT_FOUND);
    let res = s.sandbox_call(Method::DELETE, routes::SANDBOX, SB, None).await;
    assert_eq!(res.status(), StatusCode::NO_CONTENT, "deleting is idempotent");
    s.sup.reconcile().await;
    assert!(s.vmm.log().contains(&format!("destroy sandbox/{SB}")));
    assert!(s.vmm.sandbox_ids(SB).is_empty());
}

#[tokio::test]
async fn file_routes_answer_what_cloud_guest_reads_in_the_workspace() {
    let s = Server::start().await;
    let res = s.sandbox_call(Method::GET, routes::SANDBOX_FILES, SB, None).await;
    assert_eq!(error(res, StatusCode::NOT_FOUND).await.code, "not_found");
    s.running_sandbox().await;

    let res = s.sandbox_call(Method::GET, routes::SANDBOX_FILES, SB, None).await;
    assert_eq!(res.status(), StatusCode::OK);
    let tree: FileTree = res.json().await.unwrap();
    let paths: Vec<&str> = tree.files.iter().map(|f| f.path.as_str()).collect();
    assert_eq!(paths, ["src", "src/main.rs"]);

    let file_url = |path: &str| format!("{}?path={path}", s.sandbox_url(routes::SANDBOX_FILE, SB));
    let res = s.http.get(file_url("src/main.rs")).bearer_auth(TOKEN).send().await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let file: FileContent = res.json().await.unwrap();
    assert_eq!((file.path.as_str(), file.text.as_deref()), ("src/main.rs", Some("fn main() {}")));
    for (path, status) in [
        ("nope.rs", StatusCode::NOT_FOUND),
        ("..%2F..%2Fetc%2Fpasswd", StatusCode::BAD_REQUEST),
        (".git%2FHEAD", StatusCode::BAD_REQUEST),
        ("--help", StatusCode::NOT_FOUND),
    ] {
        let res = s.http.get(file_url(path)).bearer_auth(TOKEN).send().await.unwrap();
        let e = error(res, status).await;
        assert!(!e.message.is_empty(), "{path}");
    }
    let res = s.sandbox_call(Method::GET, routes::SANDBOX_FILE, SB, None).await;
    assert_eq!(error(res, StatusCode::BAD_REQUEST).await.code, "invalid", "no path");
}

fn binary(frame: &Frame) -> Message {
    Message::Binary(frame.encode().into())
}

fn text(request: &ExecRequest) -> Message {
    Message::Text(serde_json::to_string(request).unwrap().into())
}

fn sh(script: &str, pty: Option<PtySize>) -> ExecRequest {
    ExecRequest { argv: vec!["sh".into(), "-c".into(), script.into()], pty, env: Default::default(), cwd: None }
}

/// Frames until the server closes the socket.
async fn frames(socket: &mut Socket) -> Vec<Frame> {
    let mut out = Vec::new();
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Some(message) = socket.next().await {
            match message {
                Ok(Message::Binary(b)) => out.push(Frame::decode(&b).unwrap()),
                Ok(Message::Close(_)) | Err(_) => return,
                Ok(_) => {}
            }
        }
    })
    .await
    .unwrap();
    out
}

fn stdout(frames: &[Frame]) -> String {
    frames.iter().filter_map(|f| if let Frame::Stdout(b) = f { Some(String::from_utf8_lossy(b).into_owned()) } else { None }).collect()
}

async fn gone(pid_file: &std::path::Path) -> bool {
    let pid = nix::unistd::Pid::from_raw(std::fs::read_to_string(pid_file).unwrap().trim().parse().unwrap());
    tokio::time::timeout(Duration::from_secs(10), async {
        while nix::sys::signal::kill(pid, None).is_ok() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .is_ok()
}

#[tokio::test]
async fn the_exec_socket_bridges_one_session_and_closes_after_exit() {
    let s = Server::start().await;
    match s.exec_socket("ghost-box", Some(TOKEN)).await {
        Err(tungstenite::Error::Http(res)) => assert_eq!(res.status(), StatusCode::NOT_FOUND),
        other => panic!("{:?}", other.map(|_| ())),
    }
    s.running_sandbox().await;

    let mut socket = s.exec_socket(SB, Some(TOKEN)).await.unwrap();
    socket.send(text(&sh("read line; echo \"got $line\"; echo oops >&2; exit 4", None))).await.unwrap();
    socket.send(binary(&Frame::Stdin(b"hi\n".to_vec()))).await.unwrap();
    let got = frames(&mut socket).await;
    assert_eq!(stdout(&got), "got hi\n");
    assert!(got.contains(&Frame::Stderr(b"oops\n".to_vec())));
    assert_eq!(got.last(), Some(&Frame::Exit(4)));

    let mut socket = s.exec_socket(SB, Some(TOKEN)).await.unwrap();
    socket.send(text(&sh("stty size", Some(PtySize { cols: 100, rows: 30 })))).await.unwrap();
    let got = frames(&mut socket).await;
    assert_eq!(stdout(&got), "30 100\r\n");
    assert_eq!(got.last(), Some(&Frame::Exit(0)));

    // A command that cannot run: its error, then the close.
    let mut socket = s.exec_socket(SB, Some(TOKEN)).await.unwrap();
    let missing = ExecRequest { argv: vec!["/nonexistent/prog".into()], pty: None, env: Default::default(), cwd: None };
    socket.send(text(&missing)).await.unwrap();
    assert!(matches!(frames(&mut socket).await.as_slice(), [Frame::Error(m)] if m.starts_with("/nonexistent/prog")));
}

#[tokio::test]
async fn closing_the_exec_socket_ends_the_process() {
    let s = Server::start().await;
    s.running_sandbox().await;
    let pid_file = s.tmp.path().join("session.pid");
    let mut socket = s.exec_socket(SB, Some(TOKEN)).await.unwrap();
    socket.send(text(&sh(&format!("echo $$ > '{}'; echo ready; exec sleep 30", pid_file.display()), None))).await.unwrap();
    match socket.next().await.unwrap().unwrap() {
        Message::Binary(b) => assert_eq!(Frame::decode(&b).unwrap(), Frame::Stdout(b"ready\n".to_vec())),
        other => panic!("{other:?}"),
    }
    socket.close(None).await.unwrap();
    drop(socket);
    assert!(gone(&pid_file).await, "the session outlived its socket");
}

#[tokio::test]
async fn the_exec_socket_refuses_what_a_client_may_not_send() {
    let s = Server::start().await;
    s.running_sandbox().await;

    // The first message must be the request, as text.
    let mut socket = s.exec_socket(SB, Some(TOKEN)).await.unwrap();
    socket.send(binary(&Frame::Stdin(b"ls\n".to_vec()))).await.unwrap();
    assert!(matches!(frames(&mut socket).await.as_slice(), [Frame::Error(m)] if m.contains("request")));
    let mut socket = s.exec_socket(SB, Some(TOKEN)).await.unwrap();
    socket.send(Message::Text("{\"argv\": 7}".into())).await.unwrap();
    assert!(matches!(frames(&mut socket).await.as_slice(), [Frame::Error(m)] if m.starts_with("bad exec request")));

    // Server-side frames, a second request, and oversized messages end the session.
    let oversized = Message::Binary(vec![0u8; MAX_FRAME + 1].into());
    let second_request = binary(&Frame::Request(sh("echo injected", None)));
    for (n, refused) in [binary(&Frame::Exit(0)), second_request, binary(&Frame::Stdout(b"x".to_vec())), oversized].into_iter().enumerate()
    {
        let pid_file = s.tmp.path().join(format!("refused-{n}.pid"));
        let mut socket = s.exec_socket(SB, Some(TOKEN)).await.unwrap();
        socket.send(text(&sh(&format!("echo $$ > '{}'; echo ready; exec sleep 30", pid_file.display()), None))).await.unwrap();
        match socket.next().await.unwrap().unwrap() {
            Message::Binary(b) => assert_eq!(Frame::decode(&b).unwrap(), Frame::Stdout(b"ready\n".to_vec())),
            other => panic!("{other:?}"),
        }
        let _ = socket.send(refused).await;
        let got = frames(&mut socket).await;
        assert!(got.iter().all(|f| matches!(f, Frame::Error(_))), "#{n}: {got:?}");
        assert!(gone(&pid_file).await, "#{n}: the session survived");
    }
}
