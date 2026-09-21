//! The `ahocloud` binary against an in-process fake control plane, with a temporary
//! `AHOCLOUD_HOME`.

#![allow(clippy::unwrap_used, reason = "test helpers fail loudly")]

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use aho_log::{EntryKind, JsonlLog, LogStore};
use aho_types::Principal;
use axum::response::IntoResponse;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use cloud_api::{ApprovalProof, Approve, ProposeRelease, Release, ReleaseStatus};
use cloud_contract::Manifest;
use cloud_contract::ledger::{Provenance, Stage, approval_message};
use cloud_contract::manifest::Source;
use cloud_contract::policy::ReleasePolicy;
use ed25519_dalek::{Signature, SigningKey, Verifier};
use parking_lot::Mutex;
use serde_json::{Value, json};

#[derive(Clone, Debug)]
struct Req {
    method: String,
    path: String,
    query: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Req {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    fn json<T: serde::de::DeserializeOwned>(&self) -> T {
        serde_json::from_slice(&self.body).unwrap()
    }
}

struct Fake {
    url: String,
    log: Arc<Mutex<Vec<Req>>>,
}

impl Fake {
    fn requests(&self) -> Vec<Req> {
        self.log.lock().clone()
    }
}

/// A control plane on 127.0.0.1 that records every request and answers with `handler`
/// (`(status, json)`; 204 sends no body).
fn fake(handler: impl Fn(&Req) -> (u16, Value) + Send + Sync + 'static) -> Fake {
    let log: Arc<Mutex<Vec<Req>>> = Arc::default();
    let (tx, rx) = std::sync::mpsc::channel();
    let handler = Arc::new(handler);
    let seen = log.clone();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(listener.local_addr().unwrap()).unwrap();
            let app = axum::Router::new().fallback(move |req: axum::extract::Request| {
                let (seen, handler) = (seen.clone(), handler.clone());
                async move {
                    let (parts, body) = req.into_parts();
                    let body = axum::body::to_bytes(body, usize::MAX).await.unwrap().to_vec();
                    let r = Req {
                        method: parts.method.to_string(),
                        path: parts.uri.path().to_owned(),
                        query: parts.uri.query().unwrap_or_default().to_owned(),
                        headers: parts.headers.iter().map(|(k, v)| (k.to_string(), v.to_str().unwrap_or_default().to_owned())).collect(),
                        body,
                    };
                    seen.lock().push(r.clone());
                    let (code, value) = handler(&r);
                    let status = axum::http::StatusCode::from_u16(code).unwrap();
                    if code == 204 {
                        return status.into_response();
                    }
                    (status, [("content-type", "application/json")], value.to_string()).into_response()
                }
            });
            axum::serve(listener, app).await.unwrap();
        });
    });
    Fake { url: format!("http://{}", rx.recv().unwrap()), log }
}

fn not_found() -> (u16, Value) {
    (404, json!({"code": "not_found", "message": "not in this fake"}))
}

fn ahocloud(home: &Path, url: &str, cwd: &Path, args: &[&str], stdin: &str) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_ahocloud"))
        .args(args)
        .current_dir(cwd)
        .env("AHOCLOUD_HOME", home)
        .env("AHOCLOUD_API_URL", url)
        .env_remove("AHOCLOUD_PROJECT")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
    child.wait_with_output().unwrap()
}

fn text(o: &Output) -> (String, String) {
    (String::from_utf8_lossy(&o.stdout).into_owned(), String::from_utf8_lossy(&o.stderr).into_owned())
}

fn assert_ok(o: &Output) {
    let (out, err) = text(o);
    assert!(o.status.success(), "exit {:?}\nstdout:\n{out}\nstderr:\n{err}", o.status.code());
}

const SEED: [u8; 32] = [42; 32];

fn logged_in(home: &Path, url: &str) {
    std::fs::create_dir_all(home).unwrap();
    let creds = json!({"api_url": url, "token": "tok-9", "key_id": "key-9", "secret_key": URL_SAFE_NO_PAD.encode(SEED)});
    std::fs::write(home.join("credentials.json"), creds.to_string()).unwrap();
}

fn manifest_json(project: &str) -> Value {
    json!({
        "api_version": "deploy.aho.cloud/v1",
        "project": project,
        "source": {"kind": "archive", "hash": format!("blake3:{}", "0".repeat(64))},
        "build": {"kind": "dockerfile", "path": "Dockerfile", "context": "."},
        "processes": [{"name": "web", "port": 8080, "expose": "public", "health": {"kind": "http", "path": "/healthz"}}],
        "auth": {"mode": "none"},
        "data_class": "none",
        "owner": {"name": "Ana", "email": "ana@example.com"},
        "budget": {"ceiling_usd_month": 20, "degrade": "sleep"}
    })
}

fn release_json(id: &str, m: &Manifest, provenance: Provenance, manifest_hash: Option<String>) -> Value {
    let diff = cloud_contract::diff::diff(None, m);
    let requirement = ReleasePolicy::default().requirement(&diff);
    serde_json::to_value(Release {
        id: id.into(),
        project: m.project.clone(),
        manifest_hash: manifest_hash.unwrap_or_else(|| m.hash()),
        manifest: m.clone(),
        provenance,
        image_digest: None,
        stage: None,
        status: ReleaseStatus::AwaitingApproval,
        diff,
        requirement,
        approvals: Vec::new(),
        taint: Vec::new(),
        undeclared_egress: Vec::new(),
    })
    .unwrap()
}

#[cfg(unix)]
fn mode(p: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).unwrap().permissions().mode() & 0o777
}

// ---- login ----------------------------------------------------------------------------------

#[test]
fn login_binds_a_fresh_device_key_and_stores_owner_only_credentials() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let polls = AtomicU32::new(0);
    let api = fake(move |r| match (r.method.as_str(), r.path.as_str()) {
        ("POST", "/v1/auth/device/start") => (
            200,
            json!({"device_code": "dc-1", "user_code": "WDJB-MJHT", "verification_uri": "https://console.test/device", "interval_secs": 0, "expires_in_secs": 30}),
        ),
        ("POST", "/v1/auth/device/poll") if polls.fetch_add(1, Ordering::SeqCst) == 0 => (200, json!({"status": "pending"})),
        ("POST", "/v1/auth/device/poll") => (
            200,
            json!({"status": "approved", "token": "tok-1", "key_id": "key-1", "user": {"id": "u1", "email": "ana@example.com", "name": "Ana"}}),
        ),
        ("GET", "/v1/me") => {
            (200, json!({"user": {"id": "u1", "email": "ana@example.com", "name": "Ana"}, "orgs": [{"org": "acme", "owner": true}]}))
        }
        _ => not_found(),
    });
    let out = ahocloud(&home, &api.url, tmp.path(), &["login", "--device-name", "ana@laptop"], "");
    assert_ok(&out);
    assert!(text(&out).0.contains("WDJB-MJHT"), "the user code is shown");

    let path = home.join("credentials.json");
    #[cfg(unix)]
    {
        assert_eq!(mode(&path), 0o600, "credentials are owner-only");
        assert_eq!(mode(&home), 0o700);
    }
    let stored: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        (stored["api_url"].as_str(), stored["token"].as_str(), stored["key_id"].as_str()),
        (Some(api.url.as_str()), Some("tok-1"), Some("key-1"))
    );
    let seed: [u8; 32] = URL_SAFE_NO_PAD.decode(stored["secret_key"].as_str().unwrap()).unwrap().try_into().unwrap();

    let reqs = api.requests();
    let start = &reqs[0];
    assert_eq!(start.path, "/v1/auth/device/start");
    assert_eq!(start.header("authorization"), None, "nothing to authenticate with yet");
    let body: cloud_api::DeviceStart = start.json();
    assert_eq!(body.device_name, "ana@laptop");
    assert_eq!(
        body.public_key,
        URL_SAFE_NO_PAD.encode(SigningKey::from_bytes(&seed).verifying_key().as_bytes()),
        "stored key is the one bound"
    );
    let polls: Vec<&Req> = reqs.iter().filter(|r| r.path == "/v1/auth/device/poll").collect();
    assert_eq!(polls.len(), 2, "polls until approved");
    assert!(polls.iter().all(|p| p.json::<Value>()["device_code"] == "dc-1"));

    // The stored token authenticates later commands.
    let out = ahocloud(&home, &api.url, tmp.path(), &["whoami"], "");
    assert_ok(&out);
    assert!(text(&out).0.contains("ana@example.com"));
    assert_eq!(api.requests().last().unwrap().header("authorization"), Some("Bearer tok-1"));

    assert_ok(&ahocloud(&home, &api.url, tmp.path(), &["logout"], ""));
    assert!(!path.exists());
    let out = ahocloud(&home, &api.url, tmp.path(), &["whoami"], "");
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out).1.contains("ahocloud login"));
}

// ---- manifest check -------------------------------------------------------------------------

#[test]
fn manifest_check_exit_codes() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    let home = dir.join("home");
    let url = "http://127.0.0.1:9";
    std::fs::write(dir.join("deploy.json"), manifest_json("demo").to_string()).unwrap();
    let out = ahocloud(&home, url, dir, &["manifest", "check"], "");
    assert_ok(&out);
    assert!(text(&out).0.starts_with("accepted"));

    let mut bad = manifest_json("demo");
    bad["owner"]["email"] = json!("nobody");
    bad["budget"]["ceiling_usd_month"] = json!(0);
    std::fs::write(dir.join("bad.json"), bad.to_string()).unwrap();
    let out = ahocloud(&home, url, dir, &["manifest", "check", "bad.json"], "");
    assert_eq!(out.status.code(), Some(1));
    let (stdout, _) = text(&out);
    assert!(stdout.contains("/owner/email") && stdout.contains("/budget/ceiling_usd_month"), "{stdout}");

    std::fs::write(dir.join("edge.toml"), "default = \"public\"\n[[route]]\npath = \"api\"\nmode = \"public\"\n").unwrap();
    let out = ahocloud(&home, url, dir, &["manifest", "check", "deploy.json", "--edge", "edge.toml"], "");
    assert_eq!(out.status.code(), Some(1), "{:?}", text(&out));
    assert!(text(&out).0.contains("edge.toml"));

    std::fs::write(dir.join("broken.json"), "{\"api_version\":").unwrap();
    assert_eq!(ahocloud(&home, url, dir, &["manifest", "check", "broken.json"], "").status.code(), Some(2));
    assert_eq!(ahocloud(&home, url, dir, &["manifest", "check", "missing.json"], "").status.code(), Some(2));
}

// ---- manifest init --------------------------------------------------------------------------

#[test]
fn manifest_init_asks_about_every_inferred_field() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("app");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("Dockerfile"), "FROM node:20\nEXPOSE 3000\nCMD [\"node\", \"a.js\"]\n").unwrap();
    std::fs::write(dir.join(".env.example"), "STRIPE_KEY=sk_test_example\n").unwrap();
    let home = tmp.path().join("home");

    // No answers and no --accept-inferred: nothing is accepted silently, nothing is written.
    let out = ahocloud(&home, "http://127.0.0.1:9", &dir, &["manifest", "init"], "");
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out).1.contains("--accept-inferred"), "{}", text(&out).1);
    assert!(!dir.join("deploy.json").exists());

    // Inferred fields in pointer order: build context/kind/path, command, expose, name, port
    // (corrected to 8080), secret name. Then the required fields no file says.
    let answers = "\n\n\n\n\n\n8080\n\nshop\n/healthz\napi.stripe.com\n\n\nAna\nana@example.com\n20\n\n";
    let out = ahocloud(&home, "http://127.0.0.1:9", &dir, &["manifest", "init"], answers);
    assert_ok(&out);
    let (_, stderr) = text(&out);
    assert!(stderr.contains("file:Dockerfile:2  EXPOSE 3000"), "evidence is shown: {stderr}");
    assert!(!stderr.contains("sk_test_example"), "example values are never read");

    let m = Manifest::from_json(&std::fs::read_to_string(dir.join("deploy.json")).unwrap()).unwrap();
    assert_eq!(m.project, "shop");
    let web = &m.processes[0];
    assert_eq!((web.port, web.command.clone()), (Some(8080), Some(vec!["node".to_owned(), "a.js".to_owned()])));
    assert_eq!(serde_json::to_value(&web.health).unwrap(), json!({"kind": "http", "path": "/healthz", "port": null}));
    assert_eq!(m.secrets[0].name, "STRIPE_KEY");
    assert_eq!(m.secrets[0].reach, ["api.stripe.com"]);
    assert_eq!(m.egress, ["api.stripe.com"]);
    assert!(matches!(&m.source, Source::Archive { hash } if hash.starts_with("blake3:")));
    assert!(cloud_api::CheckResult::of(&m, None).accepted(), "{:?}", cloud_api::CheckResult::of(&m, None));
}

// ---- deploy ---------------------------------------------------------------------------------

const SESSION: &str = "1f2e3d";

/// A workspace with a git dir, an aho session, ignored files and a confirmed deploy.json.
fn workspace(root: &Path) -> std::path::PathBuf {
    let ws = root.join("ws");
    for (f, body) in [
        ("Dockerfile", "FROM node:20\nEXPOSE 8080\nCMD [\"node\", \"server.js\"]\n"),
        ("server.js", "require('http').createServer().listen(8080)\n"),
        (".gitignore", "node_modules/\n*.log\nsecret.txt\n"),
        ("secret.txt", "hunter2"),
        ("debug.log", "noise"),
        ("node_modules/left-pad/index.js", "module.exports = 1"),
        ("lib/util.js", "exports.x = 1"),
        (".git/HEAD", "ref: refs/heads/main\n"),
        (".git/config", "[core]\n"),
    ] {
        std::fs::create_dir_all(ws.join(f).parent().unwrap()).unwrap();
        std::fs::write(ws.join(f), body).unwrap();
    }
    std::fs::write(ws.join("deploy.json"), manifest_json("demo").to_string()).unwrap();
    let log = JsonlLog::open(ws.join(format!(".aho/sessions/{SESSION}.jsonl"))).unwrap();
    log.append(None, Principal::Operator, EntryKind::Note { text: "built the server".into() }).unwrap();
    log.append(None, Principal::Operator, EntryKind::Note { text: "ready to ship".into() }).unwrap();
    ws
}

fn control_plane(archive_hash_override: Option<String>) -> Fake {
    fake(move |r| match (r.method.as_str(), r.path.as_str()) {
        ("GET", "/v1/projects/demo") => (200, json!({"slug": "demo", "org": "acme", "active": [], "domains": []})),
        ("POST", "/v1/projects/demo/archives") => {
            let hash = archive_hash_override.clone().unwrap_or_else(|| format!("blake3:{}", blake3::hash(&r.body).to_hex()));
            (200, json!({"hash": hash, "bytes": r.body.len()}))
        }
        ("POST", "/v1/projects/demo/sessions") => {
            let id = r.query.strip_prefix("session_id=").unwrap();
            let entries = cloud_draft::verify_jsonl(std::str::from_utf8(&r.body).unwrap()).unwrap();
            (200, serde_json::to_value(cloud_draft::SessionSummary::of(id, &entries).unwrap()).unwrap())
        }
        ("POST", "/v1/projects/demo/releases") => {
            let p: ProposeRelease = r.json();
            (200, release_json("rel-1", &p.manifest, p.provenance, None))
        }
        _ => not_found(),
    })
}

fn archive_names(bytes: &[u8]) -> Vec<String> {
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
    tar.entries()
        .unwrap()
        .map(|e| {
            let mut e = e.unwrap();
            let mut sink = Vec::new();
            e.read_to_end(&mut sink).unwrap();
            e.path().unwrap().to_string_lossy().into_owned()
        })
        .collect()
}

#[test]
fn deploy_uploads_a_filtered_archive_and_the_session_then_proposes_attested() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = workspace(tmp.path());
    let home = tmp.path().join("home");
    let api = control_plane(None);
    logged_in(&home, &api.url);

    let out = ahocloud(&home, &api.url, &ws, &["deploy", "--accept-inferred"], "");
    assert_ok(&out);
    let (stdout, _) = text(&out);
    assert!(stdout.contains("proposed release rel-1") && stdout.contains("widen"), "{stdout}");

    let reqs = api.requests();
    let paths: Vec<(&str, &str)> = reqs.iter().map(|r| (r.method.as_str(), r.path.as_str())).collect();
    assert_eq!(
        paths,
        [
            ("GET", "/v1/projects/demo"),
            ("POST", "/v1/projects/demo/archives"),
            ("POST", "/v1/projects/demo/sessions"),
            ("POST", "/v1/projects/demo/releases")
        ]
    );
    assert!(reqs.iter().all(|r| r.header("authorization") == Some("Bearer tok-9")));

    let archive = &reqs[1];
    let names = archive_names(&archive.body);
    assert_eq!(names, [".gitignore", "Dockerfile", "lib/util.js", "server.js"], "gitignored files, .git, .aho and deploy.json stay home");

    let session_file = std::fs::read(ws.join(format!(".aho/sessions/{SESSION}.jsonl"))).unwrap();
    assert_eq!(reqs[2].query, format!("session_id={SESSION}"));
    assert_eq!(reqs[2].body, session_file, "the session log is uploaded as is");

    let proposal: ProposeRelease = reqs[3].json();
    let entries = cloud_draft::verify_jsonl(std::str::from_utf8(&session_file).unwrap()).unwrap();
    let head = cloud_draft::SessionSummary::of(SESSION, &entries).unwrap().head_hash;
    assert_eq!(proposal.provenance, Provenance::Attested { session_id: SESSION.into(), head_hash: head });
    let archive_hash = format!("blake3:{}", blake3::hash(&archive.body).to_hex());
    assert_eq!(proposal.manifest.source, Source::Archive { hash: archive_hash.clone() });
    assert_eq!(proposal.manifest.processes[0].command, Some(vec!["node".to_owned(), "server.js".to_owned()]), "Dockerfile CMD, accepted");
    assert_eq!(proposal.manifest.owner.email, "ana@example.com", "deploy.json's declared fields are kept");

    let written = Manifest::from_json(&std::fs::read_to_string(ws.join("deploy.json")).unwrap()).unwrap();
    assert_eq!(written, proposal.manifest, "deploy.json is what was proposed");
}

#[test]
fn deploy_stops_when_the_cloud_stored_different_bytes() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = workspace(tmp.path());
    let home = tmp.path().join("home");
    let api = control_plane(Some(format!("blake3:{}", "e".repeat(64))));
    logged_in(&home, &api.url);
    let out = ahocloud(&home, &api.url, &ws, &["deploy", "--accept-inferred"], "");
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out).1.contains("nothing was proposed"));
    assert!(api.requests().iter().all(|r| r.path != "/v1/projects/demo/releases"));
}

// ---- approve --------------------------------------------------------------------------------

fn approval_plane(manifest_hash: Option<String>) -> (Fake, Manifest) {
    let m: Manifest = serde_json::from_value(manifest_json("demo")).unwrap();
    let served = m.clone();
    let api = fake(move |r| match (r.method.as_str(), r.path.as_str()) {
        ("GET", "/v1/projects/demo/releases/rel-7") | ("POST", "/v1/projects/demo/releases/rel-7/approve") => {
            (200, release_json("rel-7", &served, Provenance::Unattested { commit: None, ci_run: None }, manifest_hash.clone()))
        }
        _ => not_found(),
    });
    (api, m)
}

#[test]
fn approve_signs_the_approval_message_with_the_device_key() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let (api, m) = approval_plane(None);
    logged_in(&home, &api.url);
    let out = ahocloud(&home, &api.url, tmp.path(), &["approve", "rel-7", "--stage", "staging", "--project", "demo"], "");
    assert_ok(&out);
    assert!(text(&out).0.contains("widen"), "the card is shown before signing");

    let reqs = api.requests();
    let post = reqs.iter().find(|r| r.method == "POST").unwrap();
    assert_eq!(post.path, "/v1/projects/demo/releases/rel-7/approve");
    assert_eq!(post.header("authorization"), Some("Bearer tok-9"));
    let body: Approve = post.json();
    assert_eq!(body.stage, Stage::Staging);
    let ApprovalProof::DeviceKey { key_id, signature } = body.proof else { panic!("device key proof expected") };
    assert_eq!(key_id, "key-9");
    let sig = Signature::from_slice(&URL_SAFE_NO_PAD.decode(signature).unwrap()).unwrap();
    let public = SigningKey::from_bytes(&SEED).verifying_key();
    assert!(public.verify(&approval_message("demo", "rel-7", &m.hash(), Stage::Staging), &sig).is_ok());
    assert!(public.verify(&approval_message("demo", "rel-7", &m.hash(), Stage::Live), &sig).is_err(), "bound to the stage");
}

#[test]
fn approve_refuses_a_manifest_that_does_not_match_its_hash() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let (api, _) = approval_plane(Some(format!("blake3:{}", "0".repeat(64))));
    logged_in(&home, &api.url);
    let out = ahocloud(&home, &api.url, tmp.path(), &["approve", "rel-7", "--stage", "live", "--project", "demo"], "");
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out).1.contains("refusing to sign"), "{:?}", text(&out));
    assert!(api.requests().iter().all(|r| r.method != "POST"), "nothing signed was sent");
}

// ---- secrets --------------------------------------------------------------------------------

#[test]
fn secret_values_travel_only_in_the_request_body() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let api = fake(|r| match (r.method.as_str(), r.path.as_str()) {
        ("PUT", "/v1/projects/demo/secrets") => (200, json!({"name": "STRIPE_KEY", "stages": ["live"], "fingerprint": "hmac:ab12"})),
        _ => not_found(),
    });
    logged_in(&home, &api.url);
    const VALUE: &str = "sk_live_51Hsupersecret";

    let out = ahocloud(
        &home,
        &api.url,
        tmp.path(),
        &["secrets", "set", "STRIPE_KEY", "--stage", "live", "--project", "demo"],
        &format!("{VALUE}\n"),
    );
    assert_ok(&out);
    let (stdout, stderr) = text(&out);
    assert!(!stdout.contains(VALUE) && !stderr.contains(VALUE), "the value is never printed");
    let reqs = api.requests();
    assert_eq!(reqs.len(), 1);
    let r = &reqs[0];
    assert_eq!((r.method.as_str(), r.path.as_str()), ("PUT", "/v1/projects/demo/secrets"));
    assert_eq!(r.json::<Value>(), json!({"name": "STRIPE_KEY", "stage": "live", "value": VALUE}), "one trailing newline is stripped");
    assert!(!r.path.contains(VALUE) && !r.query.contains(VALUE));
    assert!(r.headers.iter().all(|(_, v)| !v.contains(VALUE)));

    // There is no argument for the value: passing one is a usage error and sends nothing.
    let out = ahocloud(&home, &api.url, tmp.path(), &["secrets", "set", "STRIPE_KEY", VALUE, "--stage", "live", "--project", "demo"], "");
    assert_eq!(out.status.code(), Some(2));
    // An empty stdin is refused rather than setting an empty secret.
    let out = ahocloud(&home, &api.url, tmp.path(), &["secrets", "set", "STRIPE_KEY", "--stage", "live", "--project", "demo"], "");
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(api.requests().len(), 1);
}
