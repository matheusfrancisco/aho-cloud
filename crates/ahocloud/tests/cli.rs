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

// ---- sandboxes ------------------------------------------------------------------------------

fn sandbox_json(slug: &str, state: &str, settings: &Value) -> Value {
    let url = if settings["port"].is_null() { Value::Null } else { json!(format!("https://{slug}.preview.test")) };
    json!({
        "slug": slug, "org": "acme", "state": state,
        "created_by": {"id": "u1", "email": "ana@example.com", "name": "Ana"}, "created_at": 1,
        "image": "aho.sandbox@sha256:00", "settings": settings, "url": url, "stops_at": null,
        "cpus": 2, "mem_mib": 2048, "disk_mib": 10240, "error": null,
    })
}

fn me_json(orgs: &[&str]) -> Value {
    let orgs: Vec<Value> = orgs.iter().map(|o| json!({"org": o, "owner": true})).collect();
    json!({"user": {"id": "u1", "email": "ana@example.com", "name": "Ana"}, "orgs": orgs})
}

/// A control plane that knows one org (`acme`) and answers every sandbox route.
fn sandbox_plane() -> Fake {
    fake(|r| {
        let current = json!({"egress": ["a.com", "b.com"], "port": 3000, "auto_stop_hours": 8});
        match (r.method.as_str(), r.path.as_str()) {
            ("GET", "/v1/me") => (200, me_json(&["acme"])),
            ("POST", "/v1/orgs/acme/sandboxes") => {
                let body: Value = r.json();
                let settings = if body["settings"].is_null() {
                    json!({"egress": ["api.anthropic.com"], "port": null, "auto_stop_hours": 8})
                } else {
                    body["settings"].clone()
                };
                (200, sandbox_json(body["slug"].as_str().unwrap(), "starting", &settings))
            }
            ("GET", "/v1/orgs/acme/sandboxes") => (
                200,
                json!([
                    sandbox_json("box", "running", &current),
                    sandbox_json("lab", "stopped", &json!({"egress": [], "port": null, "auto_stop_hours": 2}))
                ]),
            ),
            ("GET", "/v1/sandboxes/box") => (200, sandbox_json("box", "running", &current)),
            ("PUT", "/v1/sandboxes/box/settings") => (200, sandbox_json("box", "running", &r.json())),
            ("POST", "/v1/sandboxes/box/start") => (200, sandbox_json("box", "starting", &current)),
            ("POST", "/v1/sandboxes/box/stop") => (200, sandbox_json("box", "stopped", &current)),
            ("DELETE", "/v1/sandboxes/box") => (204, Value::Null),
            _ => not_found(),
        }
    })
}

#[test]
fn sandbox_create_picks_the_only_org_and_sends_the_settings_given() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let api = sandbox_plane();
    logged_in(&home, &api.url);

    let out = ahocloud(
        &home,
        &api.url,
        tmp.path(),
        &["sandbox", "create", "box", "--port", "3000", "--egress", "a.com", "--egress", "*.b.com", "--auto-stop-hours", "4"],
        "",
    );
    assert_ok(&out);
    let stdout = text(&out).0;
    assert!(stdout.contains("sandbox box · org acme · starting"), "{stdout}");
    assert!(stdout.contains("port 3000 at https://box.preview.test"), "{stdout}");
    let reqs = api.requests();
    let create = reqs.iter().find(|r| r.method == "POST").unwrap();
    assert_eq!(create.path, "/v1/orgs/acme/sandboxes");
    assert_eq!(create.header("authorization"), Some("Bearer tok-9"));
    assert_eq!(
        create.json::<Value>(),
        json!({"slug": "box", "settings": {"egress": ["a.com", "*.b.com"], "port": 3000, "auto_stop_hours": 4}})
    );

    // Without settings flags the server's defaults apply; one flag fills the rest with the defaults.
    assert_ok(&ahocloud(&home, &api.url, tmp.path(), &["sandbox", "create", "box", "--org", "acme"], ""));
    assert_eq!(api.requests().last().unwrap().json::<Value>(), json!({"slug": "box", "settings": null}));
    assert_ok(&ahocloud(&home, &api.url, tmp.path(), &["sandbox", "create", "box", "--org", "acme", "--port", "8080"], ""));
    let sent = api.requests().last().unwrap().json::<Value>();
    assert_eq!(sent["settings"]["port"], 8080);
    assert_eq!(sent["settings"]["auto_stop_hours"], cloud_api::SANDBOX_AUTO_STOP_DEFAULT_HOURS);
    assert_eq!(sent["settings"]["egress"], json!(cloud_api::DEFAULT_SANDBOX_EGRESS));
}

#[test]
fn sandbox_commands_need_an_org_when_you_have_several() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let api = fake(|r| match (r.method.as_str(), r.path.as_str()) {
        ("GET", "/v1/me") => (200, me_json(&["acme", "zeta"])),
        _ => not_found(),
    });
    logged_in(&home, &api.url);
    for args in [&["sandbox", "create", "box"][..], &["sandbox", "ls"]] {
        let out = ahocloud(&home, &api.url, tmp.path(), args, "");
        assert_eq!(out.status.code(), Some(2));
        assert!(text(&out).1.contains("pass --org (one of acme, zeta)"), "{}", text(&out).1);
    }
    assert!(api.requests().iter().all(|r| r.path == "/v1/me"), "nothing is created in a guessed org");
}

#[test]
fn sandbox_ls_start_stop_render_what_the_server_says() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let api = sandbox_plane();
    logged_in(&home, &api.url);

    let out = ahocloud(&home, &api.url, tmp.path(), &["sandbox", "ls"], "");
    assert_ok(&out);
    let rows: Vec<String> = text(&out).0.lines().map(str::to_owned).collect();
    assert_eq!(rows, ["box  running  acme  https://box.preview.test", "lab  stopped  acme  -"]);
    assert_eq!(api.requests().last().unwrap().path, "/v1/orgs/acme/sandboxes");

    let out = ahocloud(&home, &api.url, tmp.path(), &["sandbox", "stop", "box"], "");
    assert_ok(&out);
    assert!(text(&out).0.contains("· stopped"));
    let out = ahocloud(&home, &api.url, tmp.path(), &["sandbox", "start", "box"], "");
    assert_ok(&out);
    assert!(text(&out).0.contains("· starting"));
    let calls: Vec<(String, String)> = api.requests().iter().skip(2).map(|r| (r.method.clone(), r.path.clone())).collect();
    assert_eq!(calls, [("POST".into(), "/v1/sandboxes/box/stop".into()), ("POST".into(), "/v1/sandboxes/box/start".into())]);

    let out = ahocloud(&home, &api.url, tmp.path(), &["sandbox", "start", "nope"], "");
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out).1.contains("not in this fake"), "server errors are shown");
}

#[test]
fn sandbox_rm_deletes_only_after_a_yes() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let api = sandbox_plane();
    logged_in(&home, &api.url);
    let deletes = || api.requests().iter().filter(|r| r.method == "DELETE").count();

    for answer in ["", "n\n", "yes please\n"] {
        let out = ahocloud(&home, &api.url, tmp.path(), &["sandbox", "rm", "box"], answer);
        assert_eq!(out.status.code(), Some(2), "answer {answer:?}");
        assert!(text(&out).1.contains("cannot be undone"));
    }
    assert_eq!(deletes(), 0);
    assert_ok(&ahocloud(&home, &api.url, tmp.path(), &["sandbox", "rm", "box"], "Y\n"));
    assert_eq!(deletes(), 1);
    let out = ahocloud(&home, &api.url, tmp.path(), &["sandbox", "rm", "box", "--yes"], "");
    assert_ok(&out);
    assert!(text(&out).0.contains("deleted sandbox box"));
    assert_eq!(deletes(), 2);
    assert_eq!(api.requests().last().unwrap().path, "/v1/sandboxes/box");
}

#[test]
fn sandbox_settings_change_only_what_the_flags_name() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let api = sandbox_plane();
    logged_in(&home, &api.url);

    // No flags: show, change nothing.
    let out = ahocloud(&home, &api.url, tmp.path(), &["sandbox", "settings", "box"], "");
    assert_ok(&out);
    assert!(text(&out).0.contains("egress a.com, b.com"));
    assert!(api.requests().iter().all(|r| r.method == "GET"));

    let out = ahocloud(
        &home,
        &api.url,
        tmp.path(),
        &["sandbox", "settings", "box", "--no-port", "--allow", "c.com", "--allow", "a.com", "--deny", "b.com"],
        "",
    );
    assert_ok(&out);
    assert!(text(&out).0.contains("no public port"));
    let put = api.requests().last().unwrap().clone();
    assert_eq!((put.method.as_str(), put.path.as_str()), ("PUT", "/v1/sandboxes/box/settings"));
    assert_eq!(put.json::<Value>(), json!({"egress": ["a.com", "c.com"], "port": null, "auto_stop_hours": 8}));

    assert_ok(&ahocloud(&home, &api.url, tmp.path(), &["sandbox", "settings", "box", "--egress", "x.io", "--auto-stop-hours", "72"], ""));
    assert_eq!(api.requests().last().unwrap().json::<Value>(), json!({"egress": ["x.io"], "port": 3000, "auto_stop_hours": 72}));
}

#[test]
fn sandbox_env_values_come_from_stdin_and_travel_only_in_the_body() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let api = fake(|r| match (r.method.as_str(), r.path.as_str()) {
        ("PUT", "/v1/sandboxes/box/env") => (200, json!({"name": "ANTHROPIC_API_KEY", "fingerprint": "hmac:cd34"})),
        ("GET", "/v1/sandboxes/box/env") => (200, json!([{"name": "ANTHROPIC_API_KEY", "fingerprint": "hmac:cd34"}])),
        ("DELETE", "/v1/sandboxes/box/env/ANTHROPIC_API_KEY") => (204, Value::Null),
        _ => not_found(),
    });
    logged_in(&home, &api.url);
    const VALUE: &str = "sk-ant-supersecret";

    let out = ahocloud(&home, &api.url, tmp.path(), &["sandbox", "env", "set", "box", "ANTHROPIC_API_KEY"], &format!("{VALUE}\r\n"));
    assert_ok(&out);
    let (stdout, stderr) = text(&out);
    assert!(!stdout.contains(VALUE) && !stderr.contains(VALUE), "the value is never printed");
    let reqs = api.requests();
    assert_eq!(reqs.len(), 1);
    let r = &reqs[0];
    assert_eq!((r.method.as_str(), r.path.as_str()), ("PUT", "/v1/sandboxes/box/env"));
    assert_eq!(r.json::<Value>(), json!({"name": "ANTHROPIC_API_KEY", "value": VALUE}), "one trailing CRLF is stripped");
    assert!(!r.path.contains(VALUE) && !r.query.contains(VALUE));
    assert!(r.headers.iter().all(|(_, v)| !v.contains(VALUE)));

    // No value argument exists, and an empty stdin sets nothing.
    let out = ahocloud(&home, &api.url, tmp.path(), &["sandbox", "env", "set", "box", "ANTHROPIC_API_KEY", VALUE], "");
    assert_eq!(out.status.code(), Some(2));
    let out = ahocloud(&home, &api.url, tmp.path(), &["sandbox", "env", "set", "box", "ANTHROPIC_API_KEY"], "");
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out).1.contains("sandbox env set box ANTHROPIC_API_KEY < value.txt"));
    assert_eq!(api.requests().len(), 1);

    let out = ahocloud(&home, &api.url, tmp.path(), &["sandbox", "env", "ls", "box"], "");
    assert_ok(&out);
    assert_eq!(text(&out).0, "ANTHROPIC_API_KEY  hmac:cd34\n");
    assert_ok(&ahocloud(&home, &api.url, tmp.path(), &["sandbox", "env", "rm", "box", "ANTHROPIC_API_KEY"], ""));
    assert_eq!(api.requests().last().unwrap().method, "DELETE");
}

// ---- ssh ------------------------------------------------------------------------------------

/// What the fake terminal does once the client has sent its frames.
#[derive(Clone, Copy)]
enum Ending {
    /// After `CloseStdin`: echo the stdin received to stdout, a line to stderr, then `Exit(code)`.
    Exit(i32),
    /// At once: an `Error` frame.
    Error,
    /// At once: a close handshake without a status.
    Hangup,
}

struct Terminal {
    url: String,
    tokens: Arc<Mutex<Vec<String>>>,
    frames: Arc<Mutex<Vec<cloud_api::exec::Frame>>>,
}

/// A session-host terminal WebSocket on 127.0.0.1 speaking exec frames.
fn terminal(ending: Ending) -> Terminal {
    use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
    use cloud_api::exec::Frame;

    let tokens: Arc<Mutex<Vec<String>>> = Arc::default();
    let frames: Arc<Mutex<Vec<Frame>>> = Arc::default();
    let (tx, rx) = std::sync::mpsc::channel();
    let (seen_tokens, seen_frames) = (tokens.clone(), frames.clone());
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            tx.send(listener.local_addr().unwrap()).unwrap();
            let app = axum::Router::new().route(
                "/terminal",
                axum::routing::get(move |q: axum::extract::RawQuery, ws: WebSocketUpgrade| {
                    let (tokens, frames) = (seen_tokens.clone(), seen_frames.clone());
                    async move {
                        tokens.lock().push(q.0.unwrap_or_default());
                        ws.on_upgrade(move |mut socket: WebSocket| async move {
                            let send = |f: Frame| Message::Binary(f.encode().into());
                            match ending {
                                Ending::Error => {
                                    let _ = socket.send(send(Frame::Error("sandbox box is stopped".into()))).await;
                                    return;
                                }
                                Ending::Hangup => {
                                    let _ = socket.send(Message::Close(None)).await;
                                    return;
                                }
                                Ending::Exit(_) => {}
                            }
                            let mut stdin = Vec::new();
                            while let Some(Ok(msg)) = socket.recv().await {
                                let Message::Binary(b) = msg else { continue };
                                let frame = Frame::decode(&b).unwrap();
                                frames.lock().push(frame.clone());
                                match frame {
                                    Frame::Stdin(b) => stdin.extend(b),
                                    Frame::CloseStdin => break,
                                    _ => {}
                                }
                            }
                            let Ending::Exit(code) = ending else { return };
                            for f in [Frame::Stdout(stdin), Frame::Stderr(b"remote warning\n".to_vec()), Frame::Exit(code)] {
                                socket.send(send(f)).await.unwrap();
                            }
                        })
                    }
                }),
            );
            axum::serve(listener, app).await.unwrap();
        });
    });
    Terminal { url: format!("ws://{}/terminal", rx.recv().unwrap()), tokens, frames }
}

fn terminal_plane(ws_url: String) -> Fake {
    fake(move |r| match (r.method.as_str(), r.path.as_str()) {
        ("POST", "/v1/sandboxes/box/terminal") => (200, json!({"url": ws_url, "token": "tk/1+="})),
        _ => not_found(),
    })
}

#[test]
fn ssh_streams_piped_stdin_then_closes_it_and_exits_with_the_remote_status() {
    use cloud_api::exec::Frame;

    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let term = terminal(Ending::Exit(7));
    let api = terminal_plane(term.url.clone());
    logged_in(&home, &api.url);
    let input = "ls -la\nexit\n".repeat(20_000);

    let out = ahocloud(&home, &api.url, tmp.path(), &["ssh", "box", "--", "sh", "-c", "cat"], &input);
    let (stdout, stderr) = text(&out);
    assert_eq!(out.status.code(), Some(7), "stderr:\n{stderr}");
    assert_eq!(stdout, input, "stdout frames are written as they come");
    assert!(stderr.contains("remote warning"), "stderr frames go to stderr");

    let req = &api.requests()[0];
    assert_eq!(
        req.json::<Value>(),
        json!({"cols": 80, "rows": 24, "command": ["sh", "-c", "cat"], "no_pty": true}),
        "not a tty: pipes, 80x24"
    );
    assert_eq!(term.tokens.lock().as_slice(), ["token=tk%2F1%2B%3D"], "the token is the query, encoded");
    let frames = term.frames.lock().clone();
    let (last, data) = frames.split_last().unwrap();
    assert_eq!(*last, Frame::CloseStdin, "CloseStdin once stdin ends, after the data");
    let mut sent = Vec::new();
    for f in data {
        let Frame::Stdin(b) = f else { panic!("only Stdin before CloseStdin, got {f:?}") };
        assert!(b.len() < cloud_api::exec::MAX_FRAME, "a frame (kind byte included) fits the limit");
        sent.extend_from_slice(b);
    }
    assert_eq!(sent, input.as_bytes());

    // Without a command the server picks the login shell.
    let term = terminal(Ending::Exit(0));
    let api = terminal_plane(term.url.clone());
    assert_ok(&ahocloud(&home, &api.url, tmp.path(), &["ssh", "box"], ""));
    assert_eq!(api.requests()[0].json::<Value>(), json!({"cols": 80, "rows": 24, "command": null, "no_pty": true}));
    assert_eq!(term.frames.lock().as_slice(), [Frame::CloseStdin]);
}

#[test]
fn ssh_asks_for_a_pty_only_at_a_terminal_or_with_dash_t() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let pty_asked = |args: &[&str]| {
        let term = terminal(Ending::Exit(0));
        let api = terminal_plane(term.url.clone());
        logged_in(&home, &api.url);
        assert_ok(&ahocloud(&home, &api.url, tmp.path(), args, "echo hi\n"));
        !api.requests()[0].json::<Value>()["no_pty"].as_bool().unwrap()
    };
    assert!(!pty_asked(&["ssh", "box"]), "piped stdin gets pipes, so output is byte-faithful");
    assert!(!pty_asked(&["ssh", "-T", "box"]));
    assert!(pty_asked(&["ssh", "-t", "box"]), "-t forces a PTY even for piped stdin");
    assert!(pty_asked(&["ssh", "box", "--pty", "--", "top"]));
    let out = ahocloud(&home, "http://127.0.0.1:9", tmp.path(), &["ssh", "-t", "-T", "box"], "");
    assert_eq!(out.status.code(), Some(2), "-t and -T conflict");
}

#[test]
fn ssh_fails_with_the_sandbox_error_or_a_missing_status() {
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");

    let term = terminal(Ending::Error);
    let api = terminal_plane(term.url.clone());
    logged_in(&home, &api.url);
    let out = ahocloud(&home, &api.url, tmp.path(), &["ssh", "box"], "ls\n");
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out).1.contains("sandbox box is stopped"), "{}", text(&out).1);

    let term = terminal(Ending::Hangup);
    let api = terminal_plane(term.url.clone());
    let out = ahocloud(&home, &api.url, tmp.path(), &["ssh", "box"], "ls\n");
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out).1.contains("without an exit status"), "{}", text(&out).1);

    // A refused terminal request never opens a socket.
    let out = ahocloud(&home, &api.url, tmp.path(), &["ssh", "other"], "");
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out).1.contains("not in this fake"));
}
