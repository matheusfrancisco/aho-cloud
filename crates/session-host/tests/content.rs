//! The workspace content routes over a real 127.0.0.1 server: package (archive, session log,
//! `deploy.json`, draft), file tree and file reads, with their auth, refusals and caps.
#![cfg(unix)]
// Helpers outside `#[test]` fns are test code too.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use aho_log::{EntryKind, LogStore, MemoryLog};
use aho_types::Principal;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use cloud_api::internal::{WorkspacePackage, WorkspaceStarted, routes};
use cloud_api::{FileContent, FileKind, FileTree};
use cloud_contract::draft::Strength;
use cloud_session_host::workspace::workspace_id;
use cloud_session_host::{CommandTemplate, Config, Limits, SessionHost, TokenKey, TokioSpawner};
use serde_json::{Value, json};
use tokio::net::TcpListener;

const SERVICE_TOKEN: &str = "service-token-for-tests";
const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";

struct Host {
    http: reqwest::Client,
    base: String,
    data: PathBuf,
    dir: tempfile::TempDir,
}

async fn host(limits: Limits) -> Host {
    let dir = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let data = dir.path().join("data");
    let config = Config {
        data_root: data.clone(),
        public_url: format!("ws://{addr}"),
        preview_url: "https://{project}.preview.test".into(),
        service_token: SERVICE_TOKEN.into(),
        token_key: TokenKey::new(SECRET).unwrap(),
        token_ttl: Duration::from_secs(60),
        // Never run: these routes do not start sessions.
        command: CommandTemplate { program: "/nonexistent/ahoc".into(), ..CommandTemplate::default() },
        limits,
        sandbox: cloud_session_host::SandboxConfig::default(),
    };
    let host = SessionHost::new(config, Arc::new(TokioSpawner)).unwrap();
    tokio::spawn(host.serve(listener, std::future::pending()));
    Host { http: reqwest::Client::new(), base: format!("http://{addr}"), data, dir }
}

impl Host {
    async fn post(&self, route: &str, bearer: Option<&str>, body: &Value) -> (u16, Value) {
        let mut req = self.http.post(format!("{}{route}", self.base)).json(body);
        if let Some(b) = bearer {
            req = req.bearer_auth(b);
        }
        let resp = req.send().await.unwrap();
        let status = resp.status().as_u16();
        (status, resp.json().await.unwrap())
    }

    async fn call(&self, route: &str, body: Value) -> (u16, Value) {
        self.post(route, Some(SERVICE_TOKEN), &body).await
    }

    /// Starts `(project, user)` and returns its project directory.
    async fn start(&self, project: &str, user: &str, session: Option<&str>) -> PathBuf {
        let (status, started) = self.call(routes::WORKSPACES, json!({"project": project, "user_id": user, "session_id": session})).await;
        assert_eq!(status, 200, "{started}");
        let started: WorkspaceStarted = serde_json::from_value(started).unwrap();
        assert!(started.rpc_url.ends_with(&workspace_id(project, user)));
        self.data.join("workspaces").join(workspace_id(project, user)).join("project")
    }

    async fn package(&self, project: &str, user: &str) -> WorkspacePackage {
        let (status, body) = self.call(routes::WORKSPACE_PACKAGE, json!({"project": project, "user_id": user})).await;
        assert_eq!(status, 200, "{body}");
        serde_json::from_value(body).unwrap()
    }
}

fn write(dir: &Path, files: &[(&str, &str)]) {
    for (f, body) in files {
        std::fs::create_dir_all(dir.join(f).parent().unwrap()).unwrap();
        std::fs::write(dir.join(f), body).unwrap();
    }
}

/// A small node app, with files a deploy must leave out.
fn app(dir: &Path) {
    write(
        dir,
        &[
            ("server.js", "require('http').createServer().listen(3000)\n"),
            ("Dockerfile", "FROM node:20\nCOPY . .\nEXPOSE 3000\nCMD [\"node\", \"server.js\"]\n"),
            ("lib/util.js", "module.exports = {}\n"),
            (".gitignore", "node_modules/\n*.log\n"),
            ("node_modules/left-pad/index.js", "ignored"),
            ("debug.log", "ignored"),
            (".git/HEAD", "ref: refs/heads/main\n"),
        ],
    );
}

/// Writes an aho-format session log to `<project>/.aho/sessions/<id>.jsonl`, `age` old.
fn session(project: &Path, id: &str, notes: &[&str], age: Duration) -> PathBuf {
    let log = MemoryLog::new();
    for n in notes {
        log.append(None, Principal::Operator, EntryKind::Note { text: (*n).to_owned() }).unwrap();
    }
    let jsonl: String = log.read_from(None).iter().map(|e| format!("{}\n", serde_json::to_string(e).unwrap())).collect();
    let path = project.join(".aho/sessions").join(format!("{id}.jsonl"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, jsonl).unwrap();
    std::fs::File::options().write(true).open(&path).unwrap().set_modified(SystemTime::now() - age).unwrap();
    path
}

fn archive_names(bytes: &[u8]) -> Vec<(String, String)> {
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(bytes));
    tar.entries()
        .unwrap()
        .map(|e| {
            let mut e = e.unwrap();
            let mut body = String::new();
            e.read_to_string(&mut body).unwrap();
            (e.path().unwrap().to_string_lossy().into_owned(), body)
        })
        .collect()
}

fn field<'a>(p: &'a WorkspacePackage, pointer: &str) -> &'a Value {
    &p.draft.fields.get(pointer).unwrap_or_else(|| panic!("no {pointer} in {:?}", p.draft.fields.keys())).value
}

#[tokio::test]
async fn package_ships_the_files_and_the_recorded_session_verified() {
    let h = host(Limits::DEFAULT).await;
    let project = h.start("shop", "alice", Some("5e55")).await;
    app(&project);
    let recorded = session(&project, "5e55", &["built the server", "ready to ship"], Duration::from_secs(600));
    // Newer, but not the session this workspace resumes.
    session(&project, "abcd", &["another session"], Duration::ZERO);
    // aho's index lives next to the logs; it is not a session.
    write(&project.join(".aho/sessions"), &[("index.sqlite3", "not a log")]);

    let p = h.package("shop", "alice").await;
    let bytes = STANDARD.decode(&p.archive).unwrap();
    assert_eq!(p.archive_hash, format!("blake3:{}", blake3::hash(&bytes).to_hex()));
    let names = archive_names(&bytes);
    let paths: Vec<&str> = names.iter().map(|(n, _)| n.as_str()).collect();
    assert_eq!(paths, [".gitignore", "Dockerfile", "lib/util.js", "server.js"], "ignored, .git and .aho stay out");
    assert_eq!(names[3].1, "require('http').createServer().listen(3000)\n");
    assert_eq!(p.files, 4);

    let s = p.session.as_ref().expect("the workspace has a session");
    assert_eq!((s.session_id.as_str(), s.entries), ("5e55", 2), "the recorded session, not the newest");
    let jsonl = STANDARD.decode(&s.jsonl).unwrap();
    assert_eq!(jsonl, std::fs::read(&recorded).unwrap(), "the log travels byte for byte");
    let entries = cloud_draft::verify_jsonl(std::str::from_utf8(&jsonl).unwrap()).unwrap();
    assert_eq!(s.head_hash, entries.last().unwrap().hash.to_hex());

    assert_eq!((p.manifest.as_ref(), p.manifest_error.as_ref()), (None, None));
    assert_eq!(field(&p, "/source"), &json!({"kind": "archive", "hash": p.archive_hash}));
    assert_eq!(p.draft.fields["/source"].strength, Strength::Declared);
    assert_eq!(field(&p, "/project"), &json!("shop"));
    assert_eq!(field(&p, "/processes/0/port"), &json!(3000), "drafted from the Dockerfile");

    // Same tree, same archive: the hash names the code.
    assert_eq!(h.package("shop", "alice").await.archive_hash, p.archive_hash);
}

#[tokio::test]
async fn without_a_recorded_log_the_newest_session_attests() {
    let h = host(Limits::DEFAULT).await;
    let project = h.start("shop", "bob", None).await;
    app(&project);
    assert!(h.package("shop", "bob").await.session.is_none(), "no session yet: unattested");

    session(&project, "0001", &["old"], Duration::from_secs(600));
    session(&project, "0002", &["new", "newer"], Duration::from_secs(1));
    assert_eq!(h.package("shop", "bob").await.session.unwrap().session_id, "0002");

    // A recorded session whose log is not there falls back to the newest too.
    h.start("shop", "bob", Some("ffff")).await;
    assert_eq!(h.package("shop", "bob").await.session.unwrap().session_id, "0002");
}

#[tokio::test]
async fn a_tampered_session_log_refuses_the_package() {
    let h = host(Limits::DEFAULT).await;
    let project = h.start("shop", "carol", Some("5e55")).await;
    app(&project);
    let log = session(&project, "5e55", &["built the server", "ready to ship"], Duration::ZERO);
    let forged = std::fs::read_to_string(&log).unwrap().replace("ready to ship", "ready to skip");
    std::fs::write(&log, forged).unwrap();

    let (status, body) = h.call(routes::WORKSPACE_PACKAGE, json!({"project": "shop", "user_id": "carol"})).await;
    assert_eq!((status, body["code"].as_str()), (422, Some("invalid_session")), "{body}");
    let message = body["message"].as_str().unwrap();
    assert!(message.contains(".aho/sessions/5e55.jsonl") && message.contains("does not verify"), "{message}");
    assert!(!message.contains(h.dir.path().to_str().unwrap()), "host paths stay private: {message}");
}

#[tokio::test]
async fn a_session_log_linked_from_another_workspace_does_not_attest() {
    let h = host(Limits::DEFAULT).await;
    let theirs = h.start("shop", "dave", None).await;
    let foreign = session(&theirs, "d00d", &["dave's work"], Duration::ZERO);
    let mine = h.start("shop", "eve", Some("d00d")).await;
    app(&mine);
    std::fs::create_dir_all(mine.join(".aho/sessions")).unwrap();
    std::os::unix::fs::symlink(&foreign, mine.join(".aho/sessions/d00d.jsonl")).unwrap();
    assert!(h.package("shop", "eve").await.session.is_none());

    std::fs::remove_dir_all(mine.join(".aho")).unwrap();
    std::os::unix::fs::symlink(theirs.join(".aho"), mine.join(".aho")).unwrap();
    let (status, body) = h.call(routes::WORKSPACE_PACKAGE, json!({"project": "shop", "user_id": "eve"})).await;
    assert_eq!((status, body["code"].as_str()), (422, Some("invalid_session")), "{body}");
}

#[tokio::test]
async fn deploy_json_comes_back_parsed_or_its_problem_does() {
    let h = host(Limits::DEFAULT).await;
    let project = h.start("shop", "frank", None).await;
    app(&project);
    let manifest = json!({
        "api_version": "deploy.aho.cloud/v1",
        "project": "elsewhere",
        "source": {"kind": "archive", "hash": format!("blake3:{}", "0".repeat(64))},
        "build": {"kind": "dockerfile", "path": "Dockerfile", "context": "."},
        "processes": [{"name": "web", "port": 8080, "expose": "public", "health": {"kind": "http", "path": "/healthz"}}],
        "auth": {"mode": "none"},
        "data_class": "none",
        "owner": {"name": "Ana", "email": "ana@example.com"},
        "budget": {"ceiling_usd_month": 20, "degrade": "sleep"}
    });
    write(&project, &[("deploy.json", &manifest.to_string())]);
    let p = h.package("shop", "frank").await;
    assert_eq!(p.manifest.as_ref().map(|m| m.processes[0].port), Some(Some(8080)));
    assert_eq!(p.manifest_error, None);
    assert_eq!(field(&p, "/processes/0/port"), &json!(8080), "deploy.json beats the Dockerfile's 3000");
    assert_eq!(field(&p, "/owner/email"), &json!("ana@example.com"));
    assert_eq!(field(&p, "/project"), &json!("shop"), "the workspace's project, whatever deploy.json says");
    assert_eq!(field(&p, "/source"), &json!({"kind": "archive", "hash": p.archive_hash}), "the uploaded archive, always");
    let packed: Vec<String> = archive_names(&STANDARD.decode(&p.archive).unwrap()).into_iter().map(|(n, _)| n).collect();
    assert!(!packed.contains(&"deploy.json".to_owned()), "the manifest names the archive, so it is not in it");

    write(&project, &[("deploy.json", "{\"api_version\":")]);
    let p = h.package("shop", "frank").await;
    assert!(p.manifest.is_none());
    let problem = p.manifest_error.as_deref().unwrap();
    assert!(problem.starts_with("deploy.json: not a deploy manifest:"), "{problem}");
    assert_eq!(field(&p, "/processes/0/port"), &json!(3000), "the draft still comes from the files");
}

#[tokio::test]
async fn an_archive_over_the_cap_is_413() {
    let h = host(Limits { max_archive_bytes: 64, ..Limits::DEFAULT }).await;
    let project = h.start("shop", "gina", None).await;
    app(&project);
    let (status, body) = h.call(routes::WORKSPACE_PACKAGE, json!({"project": "shop", "user_id": "gina"})).await;
    assert_eq!((status, body["code"].as_str()), (413, Some("too_large")), "{body}");
}

#[tokio::test]
async fn the_tree_is_what_ships_plus_deploy_json_and_directories() {
    let h = host(Limits::DEFAULT).await;
    let project = h.start("shop", "hank", None).await;
    app(&project);
    write(&project, &[("deploy.json", "{}")]);
    session(&project, "5e55", &["x"], Duration::ZERO);
    let (status, body) = h.call(routes::WORKSPACE_FILES, json!({"project": "shop", "user_id": "hank"})).await;
    assert_eq!(status, 200, "{body}");
    let tree: FileTree = serde_json::from_value(body).unwrap();
    let listed: Vec<(&str, FileKind)> = tree.files.iter().map(|f| (f.path.as_str(), f.kind)).collect();
    assert_eq!(
        listed,
        [
            (".gitignore", FileKind::File),
            ("Dockerfile", FileKind::File),
            ("deploy.json", FileKind::File),
            ("lib", FileKind::Dir),
            ("lib/util.js", FileKind::File),
            ("server.js", FileKind::File),
        ]
    );
    assert!(!tree.truncated);
    assert_eq!(tree.files[5].size, 44);

    let h = host(Limits { max_tree_entries: 2, ..Limits::DEFAULT }).await;
    let project = h.start("shop", "hank", None).await;
    app(&project);
    let (_, body) = h.call(routes::WORKSPACE_FILES, json!({"project": "shop", "user_id": "hank"})).await;
    let tree: FileTree = serde_json::from_value(body).unwrap();
    assert_eq!((tree.files.len(), tree.truncated), (2, true));
}

#[tokio::test]
async fn file_reads_serve_the_workspace_and_nothing_around_it() {
    let h = host(Limits { max_file_bytes: 16, ..Limits::DEFAULT }).await;
    let project = h.start("shop", "iris", None).await;
    app(&project);
    session(&project, "5e55", &["x"], Duration::ZERO);
    write(&project, &[("img.png", "\u{89}PNG\r\n\u{1a}\n\0\0\0")]);
    std::os::unix::fs::symlink("../workspace.json", project.join("state")).unwrap();
    std::os::unix::fs::symlink("lib/util.js", project.join("util")).unwrap();
    let read = |path: &str| h.call(routes::WORKSPACE_FILE, json!({"project": "shop", "user_id": "iris", "path": path}));

    let (status, body) = read("server.js").await;
    assert_eq!(status, 200, "{body}");
    let c: FileContent = serde_json::from_value(body).unwrap();
    assert_eq!((c.text.as_deref(), c.truncated, c.size), (Some("require('http')."), true, 44), "cut at max_file_bytes");
    let c: FileContent = serde_json::from_value(read("util").await.1).unwrap();
    assert_eq!(c.text.as_deref(), Some("module.exports ="), "links inside the workspace resolve");
    let c: FileContent = serde_json::from_value(read("img.png").await.1).unwrap();
    assert_eq!((c.text, c.truncated), (None, false), "binary");

    for bad in ["../workspace.json", "/etc/passwd", "lib/../server.js", "state", ".aho/sessions/5e55.jsonl", ".git/HEAD", "lib", "a\\b"] {
        let (status, body) = read(bad).await;
        assert_eq!((status, body["code"].as_str()), (400, Some("invalid")), "{bad}: {body}");
    }
    let (status, body) = read("nope.js").await;
    assert_eq!((status, body["code"].as_str()), (404, Some("not_found")), "{body}");
}

#[tokio::test]
async fn unknown_workspaces_are_404_and_never_created() {
    let h = host(Limits::DEFAULT).await;
    let who = json!({"project": "shop", "user_id": "nobody"});
    for (route, body) in [
        (routes::WORKSPACE_PACKAGE, who.clone()),
        (routes::WORKSPACE_FILES, who.clone()),
        (routes::WORKSPACE_FILE, json!({"project": "shop", "user_id": "nobody", "path": "server.js"})),
    ] {
        let (status, body) = h.call(route, body).await;
        assert_eq!((status, body["code"].as_str()), (404, Some("not_found")), "{route}: {body}");
    }
    assert!(!h.data.join("workspaces").exists(), "looking never starts a workspace");
}

#[tokio::test]
async fn the_service_token_is_required_and_refs_are_checked() {
    let h = host(Limits::DEFAULT).await;
    let project = h.start("shop", "jo", None).await;
    app(&project);
    let who = json!({"project": "shop", "user_id": "jo", "path": "server.js"});
    for route in [routes::WORKSPACE_PACKAGE, routes::WORKSPACE_FILES, routes::WORKSPACE_FILE] {
        for bearer in [None, Some("wrong-token-of-enough-length"), Some(&SERVICE_TOKEN[..SERVICE_TOKEN.len() - 1])] {
            let (status, body) = h.post(route, bearer, &who).await;
            assert_eq!((status, body["code"].as_str()), (401, Some("unauthorized")), "{route} {bearer:?}");
        }
        let (status, body) = h.call(route, json!({"project": "Shop", "user_id": "jo", "path": "server.js"})).await;
        assert_eq!((status, body["code"].as_str()), (400, Some("invalid")), "{route}: {body}");
        let (status, body) = h.call(route, json!({"project": "shop"})).await;
        assert_eq!((status, body["code"].as_str()), (400, Some("invalid")), "{route}: {body}");
    }
}
