//! Test harness: an edge on 127.0.0.1:0 (plain app listener + port-80 listener) in front of an
//! in-process axum upstream that echoes what it received.

#![allow(dead_code)]

pub mod idp;

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Json;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocketUpgrade};
use axum::http::HeaderMap;
use cloud_api::internal::{Site, SiteState};
use cloud_contract::edge::EdgeConfig;
use cloud_contract::ledger::Stage;
use cloud_contract::manifest::Auth;
use cloud_edge::{Components, Edge, Http01, KeyStore, Listener, MagicLinkMail, MailError, Mailer, Settings, StaticDirectory};
use futures::future::BoxFuture;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

pub const SERVICE_TOKEN: &str = "svc-token-for-tests";
pub const ACME_TOKEN: &str = "tok_EN-abc123";
pub const ACME_KEY_AUTH: &str = "tok_EN-abc123.thumbprint";

/// Records every magic link instead of sending it.
#[derive(Default)]
pub struct CaptureMailer(pub Mutex<Vec<MagicLinkMail>>);

impl Mailer for CaptureMailer {
    fn send_magic_link<'a>(&'a self, mail: &'a MagicLinkMail) -> BoxFuture<'a, Result<(), MailError>> {
        self.0.lock().push(mail.clone());
        Box::pin(async { Ok(()) })
    }
}

struct Challenges;

impl Http01 for Challenges {
    fn key_authorization(&self, token: &str) -> Option<String> {
        (token == ACME_TOKEN).then(|| ACME_KEY_AUTH.to_owned())
    }
}

pub struct Harness {
    pub edge: Edge,
    pub addr: SocketAddr,
    pub port80: SocketAddr,
    pub upstream: SocketAddr,
    pub hits: Arc<AtomicUsize>,
    pub dir: Arc<StaticDirectory>,
    pub mails: Arc<CaptureMailer>,
    pub keys: Arc<KeyStore>,
    pub http: reqwest::Client,
    _keys_dir: tempfile::TempDir,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

pub async fn listener() -> (TcpListener, SocketAddr) {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let a = l.local_addr().unwrap();
    (l, a)
}

/// The echo upstream: JSON of method, uri, headers and body; `/ws` is a WebSocket echo whose
/// first message says whether the handshake carried a principal.
pub async fn echo_upstream() -> (SocketAddr, Arc<AtomicUsize>, JoinHandle<()>) {
    async fn echo(State(hits): State<Arc<AtomicUsize>>, req: axum::extract::Request) -> Json<Value> {
        hits.fetch_add(1, Ordering::SeqCst);
        let (parts, body) = req.into_parts();
        let bytes = axum::body::to_bytes(body, 4 << 20).await.unwrap_or_default();
        let mut headers: HashMap<String, Vec<String>> = HashMap::new();
        for (k, v) in &parts.headers {
            headers.entry(k.as_str().to_owned()).or_default().push(v.to_str().unwrap_or("<bin>").to_owned());
        }
        Json(json!({
            "method": parts.method.as_str(),
            "uri": parts.uri.to_string(),
            "headers": headers,
            "body": String::from_utf8_lossy(&bytes),
        }))
    }
    async fn ws(headers: HeaderMap, up: WebSocketUpgrade) -> axum::response::Response {
        let principal = headers.get("x-principal-jwt").map(|v| v.to_str().unwrap().to_owned());
        up.on_upgrade(move |mut socket| async move {
            let hello = json!({ "principal": principal }).to_string();
            if socket.send(Message::Text(hello.into())).await.is_err() {
                return;
            }
            while let Some(Ok(msg)) = socket.recv().await {
                match msg {
                    Message::Text(t) => {
                        if socket.send(Message::Text(t)).await.is_err() {
                            break;
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        })
    }
    let hits = Arc::new(AtomicUsize::new(0));
    let app = axum::Router::new().route("/ws", axum::routing::get(ws)).fallback(echo).with_state(hits.clone());
    let (l, addr) = listener().await;
    let task = tokio::spawn(async move {
        axum::serve(l, app).await.unwrap();
    });
    (addr, hits, task)
}

impl Harness {
    pub async fn start() -> Self {
        Self::with_settings(Settings::default()).await
    }

    pub async fn with_settings(settings: Settings) -> Self {
        let (upstream, hits, up_task) = echo_upstream().await;
        let keys_dir = tempfile::tempdir().unwrap();
        let keys = Arc::new(KeyStore::open(keys_dir.path()).unwrap());
        let dir = Arc::new(StaticDirectory::default());
        let mails = Arc::new(CaptureMailer::default());
        let edge = Edge::new(Components {
            directory: dir.clone(),
            keys: keys.clone(),
            mailer: Some(mails.clone()),
            http01: Some(Arc::new(Challenges)),
            settings,
        })
        .unwrap();
        let (app_l, addr) = listener().await;
        let (redirect_l, port80) = listener().await;
        let e1 = edge.clone();
        let e2 = edge.clone();
        let t1 = tokio::spawn(async move { e1.serve(app_l, Listener::Plain).await });
        let t2 = tokio::spawn(async move { e2.serve(redirect_l, Listener::Redirect).await });
        let http = reqwest::Client::builder().redirect(reqwest::redirect::Policy::none()).build().unwrap();
        Self { edge, addr, port80, upstream, hits, dir, mails, keys, http, _keys_dir: keys_dir, tasks: vec![up_task, t1, t2] }
    }

    /// An active live site on `host` backed by the echo upstream.
    pub fn site(&self, host: &str, auth: Auth, edge_toml: &str) -> Site {
        Site {
            hostname: host.to_owned(),
            project: host.split('.').next().unwrap().to_owned(),
            stage: Stage::Live,
            state: SiteState::Active,
            upstreams: vec![self.upstream.to_string()],
            edge: if edge_toml.is_empty() { EdgeConfig::default() } else { EdgeConfig::parse(edge_toml).unwrap() },
            auth,
            password_hash: None,
            oidc_client_secret: None,
            verifier_secrets: Default::default(),
            service_token_hashes: vec![blake3::hash(SERVICE_TOKEN.as_bytes()).to_hex().to_string()],
            allowed_emails: vec![],
        }
    }

    pub fn add(&self, site: Site) {
        self.dir.upsert(site);
    }

    pub fn get(&self, host: &str, path: &str) -> reqwest::RequestBuilder {
        self.http.get(format!("http://{}{path}", self.addr)).header("host", host)
    }

    pub fn post(&self, host: &str, path: &str) -> reqwest::RequestBuilder {
        self.http.post(format!("http://{}{path}", self.addr)).header("host", host)
    }

    pub fn get80(&self, host: &str, path: &str) -> reqwest::RequestBuilder {
        self.http.get(format!("http://{}{path}", self.port80)).header("host", host)
    }

    pub fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    /// The JWKS the edge serves for `host`.
    pub async fn jwks(&self, host: &str) -> cloud_edge::Jwks {
        let r = self.get(host, "/.edge/jwks.json").send().await.unwrap();
        assert_eq!(r.status(), 200);
        r.json().await.unwrap()
    }
}

/// What the upstream received.
pub struct Echo(pub Value);

impl Echo {
    pub async fn from(resp: reqwest::Response) -> Self {
        assert_eq!(resp.status(), 200, "expected the app's answer");
        Echo(resp.json().await.unwrap())
    }

    pub fn header(&self, name: &str) -> Option<String> {
        self.0["headers"][name].as_array().and_then(|v| v.first()).and_then(|v| v.as_str()).map(str::to_owned)
    }

    pub fn headers(&self, name: &str) -> Vec<String> {
        self.0["headers"][name].as_array().map(|v| v.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect()).unwrap_or_default()
    }

    pub fn has(&self, name: &str) -> bool {
        !self.0["headers"][name].is_null()
    }

    pub fn body(&self) -> &str {
        self.0["body"].as_str().unwrap()
    }

    pub fn uri(&self) -> &str {
        self.0["uri"].as_str().unwrap()
    }
}

/// Value of the `Set-Cookie` named `name`, if any.
pub fn set_cookie(resp: &reqwest::Response, name: &str) -> Option<String> {
    resp.headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(|v| v.split(';').next()?.trim().strip_prefix(&format!("{name}=")).map(str::to_owned))
}

/// Full `Set-Cookie` line for `name`.
pub fn set_cookie_line(resp: &reqwest::Response, name: &str) -> Option<String> {
    resp.headers()
        .get_all("set-cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find(|v| v.starts_with(&format!("{name}=")))
        .map(str::to_owned)
}

pub fn location(resp: &reqwest::Response) -> String {
    resp.headers().get("location").expect("location header").to_str().unwrap().to_owned()
}

/// The hidden `name` field of an edge form.
pub fn form_field(html: &str, name: &str) -> String {
    let marker = format!("name=\"{name}\" value=\"");
    let start = html.find(&marker).unwrap_or_else(|| panic!("no {name} field in {html}")) + marker.len();
    html[start..].split('"').next().unwrap().to_owned()
}

/// A cheap Argon2id PHC string (tests only; production hashes come from the control plane).
pub fn password_hash(password: &str) -> String {
    use argon2::password_hash::{PasswordHasher, SaltString, rand_core::OsRng};
    let params = argon2::Params::new(1024, 1, 1, None).unwrap();
    let argon = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    argon.hash_password(password.as_bytes(), &SaltString::generate(&mut OsRng)).unwrap().to_string()
}

pub fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

/// Browser-side cookie jar for one host (the edge's cookies are `Secure`, which reqwest's jar
/// would not send over the plain test listener).
#[derive(Default, Clone)]
pub struct Jar(pub Vec<(String, String)>);

impl Jar {
    pub fn take(&mut self, resp: &reqwest::Response) {
        for line in resp.headers().get_all("set-cookie").iter().filter_map(|v| v.to_str().ok()) {
            let Some((name, value)) = line.split(';').next().and_then(|p| p.split_once('=')) else { continue };
            self.0.retain(|(n, _)| n != name);
            let expired = line.contains("Max-Age=0");
            if !expired {
                self.0.push((name.to_owned(), value.to_owned()));
            }
        }
    }

    pub fn header(&self) -> String {
        self.0.iter().map(|(n, v)| format!("{n}={v}")).collect::<Vec<_>>().join("; ")
    }

    pub fn get(&self, name: &str) -> Option<&str> {
        self.0.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }
}

/// Password login through the real form; returns the jar holding the session cookie.
pub async fn password_login(h: &Harness, host: &str, password: &str) -> Jar {
    let mut jar = Jar::default();
    let page = h.get(host, "/.edge/login?rd=/").send().await.unwrap();
    jar.take(&page);
    let csrf = form_field(&page.text().await.unwrap(), "csrf");
    let resp = h
        .post(host, "/.edge/login")
        .header("cookie", jar.header())
        .form(&[("csrf", csrf.as_str()), ("rd", "/"), ("password", password)])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 303, "password login");
    jar.take(&resp);
    jar
}
