//! Typed client for the public routes: one method per route and verb, paths from
//! [`routes::fill`], bodies from this crate. `ahocloud` and the aho `deploy` capability use it.

use cloud_contract::policy::ReleasePolicy;
use reqwest::header::CONTENT_TYPE;
use reqwest::{Method, RequestBuilder};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::routes::{self, fill};
use crate::*;

/// Why a call failed.
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// The server answered with an [`ApiError`].
    #[error("{0}")]
    Api(ApiError),
    /// The request never completed (connect, TLS, timeout, body read).
    #[error("request failed: {0}")]
    Http(#[from] reqwest::Error),
    /// The server answered with something that is not the expected body.
    #[error("unexpected response: {0}")]
    Decode(String),
}

impl ClientError {
    /// The stable [`ApiError::code`], when the server sent one.
    pub fn code(&self) -> Option<&str> {
        match self {
            ClientError::Api(e) => Some(&e.code),
            _ => None,
        }
    }
}

/// A control-plane client. Cheap to clone.
#[derive(Clone, Debug)]
pub struct Client {
    base: String,
    token: Option<String>,
    http: reqwest::Client,
}

/// Percent-encodes one path segment or query value.
fn seg(s: &str) -> String {
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

impl Client {
    /// `base_url` like `https://api.aho.cloud`; `token` is sent as `Authorization: Bearer`.
    pub fn new(base_url: impl Into<String>, token: Option<String>) -> Self {
        let base = base_url.into().trim_end_matches('/').to_owned();
        Self { base, token, http: reqwest::Client::new() }
    }

    pub fn base_url(&self) -> &str {
        &self.base
    }

    fn url(&self, template: &str, args: &[(&str, &str)]) -> String {
        let encoded: Vec<(&str, String)> = args.iter().map(|(k, v)| (*k, seg(v))).collect();
        let pairs: Vec<(&str, &str)> = encoded.iter().map(|(k, v)| (*k, v.as_str())).collect();
        format!("{}{}", self.base, fill(template, &pairs))
    }

    fn request(&self, method: Method, url: String) -> RequestBuilder {
        let req = self.http.request(method, url);
        match &self.token {
            Some(t) => req.bearer_auth(t),
            None => req,
        }
    }

    async fn send<T: DeserializeOwned>(&self, req: RequestBuilder) -> Result<T, ClientError> {
        let resp = req.send().await?;
        let status = resp.status();
        let bytes = resp.bytes().await?;
        if !status.is_success() {
            return Err(match serde_json::from_slice::<ApiError>(&bytes) {
                Ok(e) => ClientError::Api(e),
                Err(_) => ClientError::Decode(format!("HTTP {status}: {}", snippet(&bytes))),
            });
        }
        // Empty 2xx bodies (204) decode as JSON `null`, i.e. `()` or `Option::None`.
        let body: &[u8] = if bytes.is_empty() { b"null" } else { &bytes };
        serde_json::from_slice(body).map_err(|e| ClientError::Decode(format!("HTTP {status}: {e}: {}", snippet(&bytes))))
    }

    async fn get<T: DeserializeOwned>(&self, template: &str, args: &[(&str, &str)]) -> Result<T, ClientError> {
        self.send(self.request(Method::GET, self.url(template, args))).await
    }

    async fn call<B: Serialize + ?Sized, T: DeserializeOwned>(
        &self,
        method: Method,
        template: &str,
        args: &[(&str, &str)],
        body: &B,
    ) -> Result<T, ClientError> {
        self.send(self.request(method, self.url(template, args)).json(body)).await
    }

    async fn post<B: Serialize + ?Sized, T: DeserializeOwned>(
        &self,
        template: &str,
        args: &[(&str, &str)],
        body: &B,
    ) -> Result<T, ClientError> {
        self.call(Method::POST, template, args, body).await
    }

    async fn upload<T: DeserializeOwned>(&self, url: String, content_type: &str, bytes: Vec<u8>) -> Result<T, ClientError> {
        self.send(self.request(Method::POST, url).header(CONTENT_TYPE, content_type).body(bytes)).await
    }

    // ---- auth -------------------------------------------------------------------------------

    pub async fn device_start(&self, body: &DeviceStart) -> Result<DeviceStarted, ClientError> {
        self.post(routes::DEVICE_START, &[], body).await
    }

    pub async fn device_approve(&self, body: &DeviceApprove) -> Result<(), ClientError> {
        self.post(routes::DEVICE_APPROVE, &[], body).await
    }

    pub async fn device_poll(&self, body: &DevicePoll) -> Result<DevicePolled, ClientError> {
        self.post(routes::DEVICE_POLL, &[], body).await
    }

    pub async fn passkey_register_start(&self, body: &PasskeyRegisterStart) -> Result<PasskeyChallenge, ClientError> {
        self.post(routes::PASSKEY_REGISTER_START, &[], body).await
    }

    pub async fn passkey_register_finish(&self, body: &PasskeyFinish) -> Result<Session, ClientError> {
        self.post(routes::PASSKEY_REGISTER_FINISH, &[], body).await
    }

    pub async fn passkey_login_start(&self, body: &PasskeyLoginStart) -> Result<PasskeyChallenge, ClientError> {
        self.post(routes::PASSKEY_LOGIN_START, &[], body).await
    }

    pub async fn passkey_login_finish(&self, body: &PasskeyFinish) -> Result<Session, ClientError> {
        self.post(routes::PASSKEY_LOGIN_FINISH, &[], body).await
    }

    pub async fn me(&self) -> Result<Me, ClientError> {
        self.get(routes::ME, &[]).await
    }

    // ---- orgs and projects --------------------------------------------------------------------

    pub async fn create_org(&self, body: &CreateOrg) -> Result<Org, ClientError> {
        self.post(routes::ORGS, &[], body).await
    }

    pub async fn org_members(&self, org: &str) -> Result<Vec<Member>, ClientError> {
        self.get(routes::ORG_MEMBERS, &[("org", org)]).await
    }

    pub async fn add_member(&self, org: &str, body: &AddMember) -> Result<Member, ClientError> {
        self.post(routes::ORG_MEMBERS, &[("org", org)], body).await
    }

    pub async fn org_policy(&self, org: &str) -> Result<ReleasePolicy, ClientError> {
        self.get(routes::ORG_POLICY, &[("org", org)]).await
    }

    pub async fn set_policy(&self, org: &str, body: &SetPolicy) -> Result<Org, ClientError> {
        self.call(Method::PUT, routes::ORG_POLICY, &[("org", org)], body).await
    }

    pub async fn org_projects(&self, org: &str) -> Result<Vec<Project>, ClientError> {
        self.get(routes::ORG_PROJECTS, &[("org", org)]).await
    }

    pub async fn create_project(&self, org: &str, body: &CreateProject) -> Result<Project, ClientError> {
        self.post(routes::ORG_PROJECTS, &[("org", org)], body).await
    }

    pub async fn project(&self, project: &str) -> Result<Project, ClientError> {
        self.get(routes::PROJECT, &[("p", project)]).await
    }

    pub async fn delete_project(&self, project: &str) -> Result<(), ClientError> {
        self.send(self.request(Method::DELETE, self.url(routes::PROJECT, &[("p", project)]))).await
    }

    // ---- uploads ------------------------------------------------------------------------------

    /// Uploads a source archive (`.tar.gz`) as the raw request body.
    pub async fn upload_archive(&self, project: &str, tar_gz: Vec<u8>) -> Result<ArchiveUploaded, ClientError> {
        self.upload(self.url(routes::ARCHIVES, &[("p", project)]), "application/gzip", tar_gz).await
    }

    /// Uploads an aho session log (JSONL) as the raw request body; the server verifies its chain.
    pub async fn upload_session(&self, project: &str, session_id: &str, jsonl: Vec<u8>) -> Result<SessionUploaded, ClientError> {
        let url = format!("{}?session_id={}", self.url(routes::SESSIONS, &[("p", project)]), seg(session_id));
        self.upload(url, "application/x-ndjson", jsonl).await
    }

    // ---- manifests and releases ---------------------------------------------------------------

    pub async fn check_manifest(&self, body: &CheckManifest) -> Result<CheckResult, ClientError> {
        self.post(routes::MANIFEST_CHECK, &[], body).await
    }

    pub async fn releases(&self, project: &str) -> Result<Vec<Release>, ClientError> {
        self.get(routes::RELEASES, &[("p", project)]).await
    }

    pub async fn propose(&self, project: &str, body: &ProposeRelease) -> Result<Release, ClientError> {
        self.post(routes::RELEASES, &[("p", project)], body).await
    }

    pub async fn release(&self, project: &str, release: &str) -> Result<Release, ClientError> {
        self.get(routes::RELEASE, &[("p", project), ("r", release)]).await
    }

    pub async fn approve(&self, project: &str, release: &str, body: &Approve) -> Result<Release, ClientError> {
        self.post(routes::APPROVE, &[("p", project), ("r", release)], body).await
    }

    pub async fn reject(&self, project: &str, release: &str, body: &Reject) -> Result<Release, ClientError> {
        self.post(routes::REJECT, &[("p", project), ("r", release)], body).await
    }

    pub async fn promote(&self, project: &str, release: &str, body: &Promote) -> Result<Release, ClientError> {
        self.post(routes::PROMOTE, &[("p", project), ("r", release)], body).await
    }

    pub async fn rollback(&self, project: &str, body: &Rollback) -> Result<Release, ClientError> {
        self.post(routes::ROLLBACK, &[("p", project)], body).await
    }

    pub async fn grant_standing_orders(&self, project: &str, body: &GrantStandingOrders) -> Result<(), ClientError> {
        self.post(routes::STANDING_ORDERS, &[("p", project)], body).await
    }

    pub async fn oncall(&self, project: &str, body: &OnCall) -> Result<(), ClientError> {
        self.post(routes::ONCALL, &[("p", project)], body).await
    }

    // ---- domains ------------------------------------------------------------------------------

    pub async fn domains(&self, project: &str) -> Result<Vec<DomainView>, ClientError> {
        self.get(routes::DOMAINS, &[("p", project)]).await
    }

    pub async fn add_domain(&self, project: &str, body: &AddDomain) -> Result<DomainView, ClientError> {
        self.post(routes::DOMAINS, &[("p", project)], body).await
    }

    pub async fn verify_domain(&self, project: &str, hostname: &str) -> Result<DomainView, ClientError> {
        self.send(self.request(Method::POST, self.url(routes::DOMAIN_VERIFY, &[("p", project), ("host", hostname)]))).await
    }

    // ---- workspaces, ledger, secrets ----------------------------------------------------------

    /// Starts (or resumes, with `session_id`) a web-UI workspace.
    pub async fn start_workspace(&self, project: &str, session_id: Option<&str>) -> Result<Workspace, ClientError> {
        let url = self.url(routes::WORKSPACES, &[("p", project)]);
        let req = self.request(Method::POST, url);
        let req = match session_id {
            Some(id) => req.json(&serde_json::json!({ "session_id": id })),
            None => req,
        };
        self.send(req).await
    }

    pub async fn ledger(&self, project: &str) -> Result<Vec<LedgerEntry>, ClientError> {
        self.get(routes::LEDGER, &[("p", project)]).await
    }

    pub async fn secrets(&self, project: &str) -> Result<Vec<SecretView>, ClientError> {
        self.get(routes::SECRETS, &[("p", project)]).await
    }

    /// The value travels only in the request body.
    pub async fn set_secret(&self, project: &str, body: &SetSecret) -> Result<SecretView, ClientError> {
        self.call(Method::PUT, routes::SECRETS, &[("p", project)], body).await
    }

    // ---- sandboxes ----------------------------------------------------------------------------

    pub async fn org_sandboxes(&self, org: &str) -> Result<Vec<Sandbox>, ClientError> {
        self.get(routes::ORG_SANDBOXES, &[("org", org)]).await
    }

    pub async fn create_sandbox(&self, org: &str, body: &CreateSandbox) -> Result<Sandbox, ClientError> {
        self.post(routes::ORG_SANDBOXES, &[("org", org)], body).await
    }

    pub async fn sandbox(&self, slug: &str) -> Result<Sandbox, ClientError> {
        self.get(routes::SANDBOX, &[("s", slug)]).await
    }

    /// Destroys the VM and its disk.
    pub async fn delete_sandbox(&self, slug: &str) -> Result<(), ClientError> {
        self.send(self.request(Method::DELETE, self.url(routes::SANDBOX, &[("s", slug)]))).await
    }

    pub async fn start_sandbox(&self, slug: &str) -> Result<Sandbox, ClientError> {
        self.send(self.request(Method::POST, self.url(routes::SANDBOX_START, &[("s", slug)]))).await
    }

    pub async fn stop_sandbox(&self, slug: &str) -> Result<Sandbox, ClientError> {
        self.send(self.request(Method::POST, self.url(routes::SANDBOX_STOP, &[("s", slug)]))).await
    }

    pub async fn set_sandbox_settings(&self, slug: &str, body: &SandboxSettings) -> Result<Sandbox, ClientError> {
        self.call(Method::PUT, routes::SANDBOX_SETTINGS, &[("s", slug)], body).await
    }

    /// A single-use terminal WebSocket; see [`crate::exec`] for its frames.
    pub async fn sandbox_terminal(&self, slug: &str, body: &TerminalRequest) -> Result<TerminalAccess, ClientError> {
        self.post(routes::SANDBOX_TERMINAL, &[("s", slug)], body).await
    }

    pub async fn sandbox_chat(&self, slug: &str, body: &StartSandboxChat) -> Result<SandboxChat, ClientError> {
        self.post(routes::SANDBOX_CHAT, &[("s", slug)], body).await
    }

    pub async fn sandbox_files(&self, slug: &str) -> Result<FileTree, ClientError> {
        self.get(routes::SANDBOX_FILES, &[("s", slug)]).await
    }

    pub async fn sandbox_file(&self, slug: &str, path: &str) -> Result<FileContent, ClientError> {
        let url = format!("{}?path={}", self.url(routes::SANDBOX_FILE, &[("s", slug)]), seg(path));
        self.send(self.request(Method::GET, url)).await
    }

    pub async fn sandbox_env(&self, slug: &str) -> Result<Vec<EnvVarView>, ClientError> {
        self.get(routes::SANDBOX_ENV, &[("s", slug)]).await
    }

    /// The value travels only in the request body.
    pub async fn set_sandbox_env(&self, slug: &str, body: &SetEnvVar) -> Result<EnvVarView, ClientError> {
        self.call(Method::PUT, routes::SANDBOX_ENV, &[("s", slug)], body).await
    }

    pub async fn delete_sandbox_env(&self, slug: &str, name: &str) -> Result<(), ClientError> {
        self.send(self.request(Method::DELETE, self.url(routes::SANDBOX_ENV_VAR, &[("s", slug), ("name", name)]))).await
    }
}

fn snippet(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut s: String = text.chars().take(200).collect();
    if text.chars().count() > 200 {
        s.push('…');
    }
    s
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::Router;
    use axum::body::Bytes;
    use axum::extract::{Request, State};
    use axum::http::StatusCode;
    use axum::response::{IntoResponse, Response};
    use cloud_contract::ledger::Stage;
    use tokio::sync::Mutex;

    use super::*;

    #[derive(Clone, Debug)]
    struct Seen {
        method: String,
        uri: String,
        auth: Option<String>,
        content_type: Option<String>,
        body: Vec<u8>,
    }

    type Log = Arc<Mutex<Vec<Seen>>>;

    async fn fake(State(log): State<Log>, req: Request) -> Response {
        let (parts, body) = req.into_parts();
        let body: Bytes = axum::body::to_bytes(body, usize::MAX).await.unwrap_or_default();
        let header = |n: &str| parts.headers.get(n).and_then(|v| v.to_str().ok()).map(str::to_owned);
        log.lock().await.push(Seen {
            method: parts.method.to_string(),
            uri: parts.uri.to_string(),
            auth: header("authorization"),
            content_type: header("content-type"),
            body: body.to_vec(),
        });
        let json = |v: serde_json::Value| ([("content-type", "application/json")], v.to_string()).into_response();
        match (parts.method.as_str(), parts.uri.path()) {
            ("GET", "/v1/me") => json(serde_json::json!({"user": {"id": "u1", "email": "a@x.io", "name": "A"}, "orgs": [{"org": "acme", "owner": true}]})),
            ("POST", "/v1/auth/device/approve") => StatusCode::NO_CONTENT.into_response(),
            ("POST", "/v1/projects/demo/archives") => json(serde_json::json!({"hash": "blake3:00", "bytes": body.len()})),
            ("POST", "/v1/projects/demo/sessions") => json(serde_json::json!({"session_id": "a1", "head_hash": "ff", "entries": 3})),
            ("PUT", "/v1/projects/demo/secrets") => json(serde_json::json!({"name": "K", "stages": ["live"], "fingerprint": "hmac:1"})),
            ("GET", "/v1/projects/demo/releases/r%2F1") => (
                StatusCode::NOT_FOUND,
                [("content-type", "application/json")],
                r#"{"code":"not_found","message":"no release r/1","refusals":[{"code":"project_slug","field":"/project","message":"m","fix":"f"}]}"#,
            )
                .into_response(),
            ("GET", "/v1/projects/demo/ledger") => (StatusCode::BAD_GATEWAY, "<html>upstream down</html>").into_response(),
            ("GET", "/v1/projects/demo/releases") => json(serde_json::json!({"not": "a list"})),
            _ => (StatusCode::IM_A_TEAPOT, "unrouted").into_response(),
        }
    }

    async fn serve() -> (String, Log) {
        let log: Log = Arc::default();
        let app = Router::new().fallback(fake).with_state(log.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{addr}/"), log)
    }

    #[tokio::test]
    async fn success_bodies_decode_and_the_token_is_a_bearer_header() {
        let (url, log) = serve().await;
        let c = Client::new(url, Some("tok-1".into()));
        let me = c.me().await.unwrap();
        assert_eq!((me.user.email.as_str(), me.orgs[0].owner), ("a@x.io", true));
        let seen = log.lock().await[0].clone();
        assert_eq!((seen.method.as_str(), seen.uri.as_str()), ("GET", "/v1/me"));
        assert_eq!(seen.auth.as_deref(), Some("Bearer tok-1"));
    }

    #[tokio::test]
    async fn anonymous_clients_send_no_authorization_and_accept_empty_2xx() {
        let (url, log) = serve().await;
        let c = Client::new(url, None);
        c.device_approve(&DeviceApprove { user_code: "WDJB-MJHT".into() }).await.unwrap();
        let seen = log.lock().await[0].clone();
        assert_eq!(seen.auth, None);
        assert_eq!(serde_json::from_slice::<serde_json::Value>(&seen.body).unwrap(), serde_json::json!({"user_code": "WDJB-MJHT"}));
    }

    #[tokio::test]
    async fn api_errors_decode_with_their_refusals() {
        let (url, _) = serve().await;
        let err = Client::new(url, None).release("demo", "r/1").await.unwrap_err();
        let ClientError::Api(e) = &err else { panic!("{err:?}") };
        assert_eq!((e.code.as_str(), e.refusals.len()), ("not_found", 1));
        assert_eq!(err.code(), Some("not_found"));
    }

    #[tokio::test]
    async fn non_api_errors_and_wrong_shapes_are_decode_errors() {
        let (url, _) = serve().await;
        let c = Client::new(url, None);
        match c.ledger("demo").await.unwrap_err() {
            ClientError::Decode(m) => assert!(m.contains("502") && m.contains("upstream down"), "{m}"),
            other => panic!("{other:?}"),
        }
        assert!(matches!(c.releases("demo").await.unwrap_err(), ClientError::Decode(_)));
    }

    #[tokio::test]
    async fn unreachable_servers_are_http_errors() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let err = Client::new(format!("http://{addr}"), None).me().await.unwrap_err();
        assert!(matches!(err, ClientError::Http(_)), "{err:?}");
    }

    #[tokio::test]
    async fn uploads_are_raw_bodies_and_path_args_are_encoded() {
        let (url, log) = serve().await;
        let c = Client::new(url, Some("t".into()));
        let tgz = vec![0x1f, 0x8b, 0, 1, 2];
        assert_eq!(c.upload_archive("demo", tgz.clone()).await.unwrap().bytes, 5);
        let s = c.upload_session("demo", "a1 b", b"{}\n".to_vec()).await.unwrap();
        assert_eq!(s.entries, 3);
        let seen = log.lock().await.clone();
        assert_eq!((seen[0].body.clone(), seen[0].content_type.as_deref()), (tgz, Some("application/gzip")));
        assert_eq!(seen[1].uri, "/v1/projects/demo/sessions?session_id=a1%20b");
        assert_eq!(seen[1].body, b"{}\n");
        let _ = c.release("demo", "r/1").await;
        assert_eq!(log.lock().await[2].uri, "/v1/projects/demo/releases/r%2F1");
    }

    #[tokio::test]
    async fn secret_values_travel_only_in_the_body() {
        let (url, log) = serve().await;
        let c = Client::new(url, Some("t".into()));
        let body = SetSecret { name: "K".into(), stage: Stage::Live, value: "s3cr3t-value".into() };
        c.set_secret("demo", &body).await.unwrap();
        let seen = log.lock().await[0].clone();
        assert_eq!(seen.method, "PUT");
        assert!(!seen.uri.contains("s3cr3t"));
        assert!(String::from_utf8(seen.body).unwrap().contains("\"value\":\"s3cr3t-value\""));
    }

    #[tokio::test]
    async fn sandbox_file_paths_and_env_names_are_encoded() {
        let (url, log) = serve().await;
        let c = Client::new(url, Some("t".into()));
        let _ = c.sandbox_file("box", "src/a b?.rs").await;
        let _ = c.delete_sandbox_env("box", "A/../B").await;
        let seen = log.lock().await.clone();
        assert_eq!(seen[0].uri, "/v1/sandboxes/box/file?path=src%2Fa%20b%3F.rs");
        assert_eq!((seen[1].method.as_str(), seen[1].uri.as_str()), ("DELETE", "/v1/sandboxes/box/env/A%2F..%2FB"));
    }
}
