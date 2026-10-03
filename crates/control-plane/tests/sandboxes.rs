//! Sandboxes through the API, with the fake runtime and session host: the lifecycle, one slug
//! namespace with projects, the per-org limit, membership, the write-only environment, settings,
//! terminals, chats and files, auto-stop on the clock, the public port on the edge, and a
//! deployment without a sandbox image.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "test code: a failed unwrap is a failed test")]

mod common;

use std::collections::BTreeMap;

use axum::http::{Method, StatusCode};
use cloud_api::exec::{ExecRequest, PtySize};
use cloud_api::internal::{SandboxChatStart, SandboxTerminal, SiteState, Sites};
use cloud_api::routes::{self, fill};
use cloud_api::{DEFAULT_SANDBOX_EGRESS, SandboxSettings, SandboxState};
use cloud_contract::edge::EdgeConfig;
use cloud_contract::ledger::Stage;
use cloud_contract::manifest::Auth;
use cloud_control_plane::fakes::RuntimeCall;
use cloud_control_plane::store::SandboxRec;
use cloud_control_plane::{SandboxImage, sandboxes};
use common::*;
use serde_json::{Value, json};

const HOUR: u64 = 3600;
const SECRET: &str = "sk-ant-sandbox-secret-0123456789";

fn image() -> String {
    format!("aho.sandbox@sha256:{}", "c".repeat(64))
}

/// The harness with sandboxes turned on.
async fn harness() -> H {
    H::with(|c| c.sandbox_image = SandboxImage::parse(&image())).await
}

fn in_org(org: &str) -> String {
    fill(routes::ORG_SANDBOXES, &[("org", org)])
}

fn at(template: &str, slug: &str) -> String {
    fill(template, &[("s", slug)])
}

async fn create(h: &H, token: &str, org: &str, body: Value) -> (StatusCode, Value) {
    h.post(&in_org(org), Some(token), body).await
}

async fn create_ok(h: &H, token: &str, body: Value) -> Value {
    let (s, v) = create(h, token, "acme", body).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    v
}

/// `POST` without a body, as `ahocloud` sends start and stop.
async fn bare_post(h: &H, path: &str, token: &str) -> (StatusCode, Value) {
    h.req(Method::POST, path, Some(token), None).await
}

async fn put(h: &H, path: &str, token: &str, body: Value) -> (StatusCode, Value) {
    h.req(Method::PUT, path, Some(token), Some(body)).await
}

/// Carol, alone in org `other`.
async fn outsider(h: &H) -> Person {
    let carol = h.signup("carol@other.test", "Carol").await;
    h.ok(routes::ORGS, &carol.token, json!({"slug": "other", "name": "Other"})).await;
    carol
}

async fn sites(h: &H, since: Option<u64>) -> Sites {
    let path = match since {
        Some(v) => format!("{}?since={v}", cloud_api::internal::routes::SITES),
        None => cloud_api::internal::routes::SITES.to_owned(),
    };
    let (s, v) = h.req(Method::GET, &path, Some(SERVICE), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    serde_json::from_value(v).unwrap()
}

fn site<'a>(all: &'a Sites, host: &str) -> Option<&'a cloud_api::internal::Site> {
    all.sites.iter().find(|s| s.hostname == host)
}

fn stops(h: &H) -> Vec<String> {
    h.runtime
        .calls()
        .into_iter()
        .filter_map(|c| match c {
            RuntimeCall::StopSandbox { slug } => Some(slug),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn without_an_image_sandboxes_cannot_be_created_or_started() {
    let h = H::new().await;
    let (ana, _) = h.team("shop").await;
    let (s, e) = create(&h, &ana.token, "acme", json!({"slug": "dev"})).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::SERVICE_UNAVAILABLE, Some("sandboxes_disabled")), "{e}");
    assert_eq!(h.get_ok(&in_org("acme"), &ana.token).await, json!([]));

    // One made while an image was configured keeps its record but cannot boot.
    let rec = SandboxRec {
        slug: "old".into(),
        org: "acme".into(),
        created_by: ana.id.clone(),
        created_at: START,
        image: image(),
        settings: sandboxes::default_settings(),
        state: SandboxState::Stopped,
        error: None,
        stops_at: None,
        cpus: 2,
        mem_mib: 2048,
        disk_mib: 10240,
    };
    h.state.store.create_sandbox(&rec, 5).await.unwrap();
    let (s, e) = bare_post(&h, &at(routes::SANDBOX_START, "old"), &ana.token).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::SERVICE_UNAVAILABLE, Some("sandboxes_disabled")), "{e}");
    assert!(h.runtime.sandbox_specs().is_empty());
    assert_eq!(h.get_ok(&at(routes::SANDBOX, "old"), &ana.token).await["state"], "stopped");
}

#[tokio::test]
async fn a_sandbox_boots_on_create_stops_starts_and_is_deleted_by_any_member() {
    let h = harness().await;
    let (ana, bob) = h.team("shop").await;
    let v = create_ok(&h, &ana.token, json!({"slug": "dev"})).await;
    assert_eq!((v["slug"].as_str(), v["org"].as_str(), v["state"].as_str()), (Some("dev"), Some("acme"), Some("running")), "{v}");
    assert_eq!(v["created_by"]["email"], "ana@acme.test");
    assert_eq!(v["image"], image());
    assert_eq!((v["cpus"].as_u64(), v["mem_mib"].as_u64(), v["disk_mib"].as_u64()), (Some(2), Some(2048), Some(10240)));
    assert_eq!(v["settings"], json!({"egress": DEFAULT_SANDBOX_EGRESS, "port": null, "auto_stop_hours": 8}));
    assert_eq!(v["stops_at"].as_u64(), Some(START + 8 * HOUR));
    assert!(v["url"].is_null(), "no public port, no URL");

    let specs = h.runtime.sandbox_specs();
    assert_eq!(specs.len(), 1);
    let spec = &specs[0];
    assert_eq!((spec.slug.as_str(), spec.image_repo.as_str()), ("dev", "aho.sandbox"));
    assert_eq!(spec.image_digest, format!("sha256:{}", "c".repeat(64)));
    assert_eq!((spec.cpus, spec.mem_mib, spec.disk_mib, spec.port), (2, 2048, 10240, None));
    assert_eq!(spec.egress, DEFAULT_SANDBOX_EGRESS);

    let listed = h.get_ok(&in_org("acme"), &bob.token).await;
    assert_eq!(listed.as_array().unwrap().iter().map(|s| s["slug"].as_str().unwrap()).collect::<Vec<_>>(), ["dev"]);

    // Bob stops it: the VM goes, the record (and the disk) stay.
    h.clock.advance(600);
    let (s, v) = bare_post(&h, &at(routes::SANDBOX_STOP, "dev"), &bob.token).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!((v["state"].as_str(), v["stops_at"].as_u64()), (Some("stopped"), None));
    assert_eq!(stops(&h), ["dev"]);
    assert_eq!(h.get_ok(&at(routes::SANDBOX, "dev"), &ana.token).await["state"], "stopped");

    // Starting boots it again with a fresh auto-stop clock.
    h.clock.advance(HOUR);
    let (s, v) = bare_post(&h, &at(routes::SANDBOX_START, "dev"), &bob.token).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!((v["state"].as_str(), v["stops_at"].as_u64()), (Some("running"), Some(START + 600 + HOUR + 8 * HOUR)));
    assert_eq!(h.runtime.sandbox_specs().len(), 2);

    // Deleting destroys the VM and its disk; the slug is free again.
    let (s, _) = h.req(Method::DELETE, &at(routes::SANDBOX, "dev"), Some(&bob.token), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert!(h.runtime.calls().contains(&RuntimeCall::DeleteSandbox { slug: "dev".into() }));
    assert!(h.runtime.sandbox("dev").is_none());
    assert_eq!(h.get(&at(routes::SANDBOX, "dev"), &ana.token).await.0, StatusCode::NOT_FOUND);
    assert_eq!(h.get_ok(&in_org("acme"), &ana.token).await, json!([]));
    create_ok(&h, &ana.token, json!({"slug": "dev"})).await;
}

#[tokio::test]
async fn the_runtime_live_state_shows_through() {
    let h = harness().await;
    let (ana, _) = h.team("shop").await;
    create_ok(&h, &ana.token, json!({"slug": "dev"})).await;
    h.runtime.fail_sandbox("dev", "image pull failed: manifest unknown");
    let v = h.get_ok(&at(routes::SANDBOX, "dev"), &ana.token).await;
    assert_eq!((v["state"].as_str(), v["error"].as_str()), (Some("failed"), Some("image pull failed: manifest unknown")));
    let listed = h.get_ok(&in_org("acme"), &ana.token).await;
    assert_eq!(listed[0]["state"], "failed");
    let (s, e) = h.post(&at(routes::SANDBOX_TERMINAL, "dev"), Some(&ana.token), json!({"cols": 80, "rows": 24})).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert!(e["message"].as_str().unwrap().contains("manifest unknown"), "{e}");
    assert!(h.host.terminals().is_empty());
}

#[tokio::test]
async fn sandbox_and_project_slugs_share_one_namespace() {
    let h = harness().await;
    let (ana, _) = h.team("shop").await;
    let (s, e) = create(&h, &ana.token, "acme", json!({"slug": "shop"})).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::CONFLICT, Some("conflict")));

    create_ok(&h, &ana.token, json!({"slug": "dev"})).await;
    let (s, e) = h.post(&fill(routes::ORG_PROJECTS, &[("org", "acme")]), Some(&ana.token), json!({"slug": "dev"})).await;
    assert_eq!(s, StatusCode::CONFLICT, "{e}");

    // A deleted project's slug stays retired for sandboxes too.
    let (s, _) = h.req(Method::DELETE, &fill(routes::PROJECT, &[("p", "shop")]), Some(&ana.token), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert_eq!(create(&h, &ana.token, "acme", json!({"slug": "shop"})).await.0, StatusCode::CONFLICT);

    for bad in ["", "Dev", "2dev", "dev_box", "dev.box", &"a".repeat(41)] {
        let (s, e) = create(&h, &ana.token, "acme", json!({"slug": bad})).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{bad:?}: {e}");
    }
    assert_eq!(h.runtime.sandbox_specs().iter().map(|s| s.slug.as_str()).collect::<Vec<_>>(), ["dev"], "refusals boot nothing");
}

#[tokio::test]
async fn an_org_has_at_most_max_sandboxes() {
    let h = H::with(|c| {
        c.sandbox_image = SandboxImage::parse(&image());
        c.max_sandboxes_per_org = 2;
    })
    .await;
    let (ana, _) = h.team("shop").await;
    create_ok(&h, &ana.token, json!({"slug": "one"})).await;
    create_ok(&h, &ana.token, json!({"slug": "two"})).await;
    let (s, e) = create(&h, &ana.token, "acme", json!({"slug": "three"})).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::CONFLICT, Some("conflict")), "{e}");
    assert_eq!(h.get(&at(routes::SANDBOX, "three"), &ana.token).await.0, StatusCode::NOT_FOUND);

    let carol = outsider(&h).await;
    let (s, e) = create(&h, &carol.token, "other", json!({"slug": "three"})).await;
    assert_eq!(s, StatusCode::CREATED, "the limit is per org: {e}");

    let (s, _) = h.req(Method::DELETE, &at(routes::SANDBOX, "one"), Some(&ana.token), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    create_ok(&h, &ana.token, json!({"slug": "four"})).await;
}

#[tokio::test]
async fn only_members_of_the_org_reach_its_sandboxes() {
    let h = harness().await;
    let (ana, _) = h.team("shop").await;
    create_ok(&h, &ana.token, json!({"slug": "dev"})).await;
    let carol = outsider(&h).await;
    let c = Some(carol.token.as_str());

    let settings = json!({"egress": [], "port": null, "auto_stop_hours": 1});
    let attempts: Vec<(Method, String, Option<Value>)> = vec![
        (Method::GET, in_org("acme"), None),
        (Method::POST, in_org("acme"), Some(json!({"slug": "mine"}))),
        (Method::GET, at(routes::SANDBOX, "dev"), None),
        (Method::DELETE, at(routes::SANDBOX, "dev"), None),
        (Method::POST, at(routes::SANDBOX_START, "dev"), None),
        (Method::POST, at(routes::SANDBOX_STOP, "dev"), None),
        (Method::PUT, at(routes::SANDBOX_SETTINGS, "dev"), Some(settings)),
        (Method::POST, at(routes::SANDBOX_TERMINAL, "dev"), Some(json!({"cols": 80, "rows": 24}))),
        (Method::POST, at(routes::SANDBOX_CHAT, "dev"), Some(json!({}))),
        (Method::GET, at(routes::SANDBOX_FILES, "dev"), None),
        (Method::GET, format!("{}?path=a.txt", at(routes::SANDBOX_FILE, "dev")), None),
        (Method::GET, at(routes::SANDBOX_ENV, "dev"), None),
        (Method::PUT, at(routes::SANDBOX_ENV, "dev"), Some(json!({"name": "K", "value": "v"}))),
        (Method::DELETE, fill(routes::SANDBOX_ENV_VAR, &[("s", "dev"), ("name", "K")]), None),
    ];
    for (method, path, body) in attempts {
        let (s, e) = h.req(method.clone(), &path, c, body.clone()).await;
        assert_eq!((s, e["code"].as_str()), (StatusCode::FORBIDDEN, Some("forbidden")), "{method} {path}: {e}");
        let (s, _) = h.req(method.clone(), &path, None, body).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "{method} {path} without a session");
    }
    assert_eq!(h.get_ok(&in_org("other"), &carol.token).await, json!([]));
    assert_eq!(h.get(&at(routes::SANDBOX, "nope"), &ana.token).await.0, StatusCode::NOT_FOUND);

    // Nothing an outsider asked for happened.
    assert!(h.runtime.calls().is_empty(), "{:?}", h.runtime.calls());
    assert_eq!(h.runtime.sandbox_specs().len(), 1);
    assert!(h.host.terminals().is_empty() && h.host.chats().is_empty());
    assert!(h.state.store.sandbox_env("dev").await.unwrap().is_empty());
}

#[tokio::test]
async fn env_vars_are_write_only_and_reach_the_vm_decrypted_at_its_next_start() {
    let h = harness().await;
    let (ana, bob) = h.team("shop").await;
    create_ok(&h, &ana.token, json!({"slug": "dev"})).await;
    let env = at(routes::SANDBOX_ENV, "dev");

    let (s, v) = put(&h, &env, &bob.token, json!({"name": "ANTHROPIC_API_KEY", "value": SECRET})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let mut keys: Vec<&String> = v.as_object().unwrap().keys().collect();
    keys.sort();
    assert_eq!(keys, ["fingerprint", "name"], "a name and a fingerprint, never the value");
    let fingerprint = v["fingerprint"].as_str().unwrap().to_owned();
    assert!(fingerprint.starts_with("hmac-sha256:"));
    assert_eq!(put(&h, &env, &bob.token, json!({"name": "OTHER", "value": "v2"})).await.0, StatusCode::OK);

    let listed = h.get_ok(&env, &ana.token).await;
    assert_eq!(
        listed,
        json!([{"name": "ANTHROPIC_API_KEY", "fingerprint": fingerprint}, {"name": "OTHER", "fingerprint": listed[1]["fingerprint"]}])
    );
    for body in [listed.to_string(), h.get_ok(&at(routes::SANDBOX, "dev"), &ana.token).await.to_string(), v.to_string()] {
        assert!(!body.contains(SECRET), "a value came back: {body}");
    }
    for var in h.state.store.sandbox_env("dev").await.unwrap() {
        assert!(!var.ciphertext.windows(SECRET.len()).any(|w| w == SECRET.as_bytes()), "stored in the clear");
    }
    assert!(h.runtime.sandbox_specs().last().unwrap().env.is_empty(), "values reach the VM at its next start");

    // Replacing a value changes its fingerprint.
    let (_, replaced) = put(&h, &env, &bob.token, json!({"name": "OTHER", "value": "v3"})).await;
    assert_ne!(replaced["fingerprint"], listed[1]["fingerprint"]);

    assert_eq!(bare_post(&h, &at(routes::SANDBOX_START, "dev"), &ana.token).await.0, StatusCode::OK);
    let expected = BTreeMap::from([("ANTHROPIC_API_KEY".to_owned(), SECRET.to_owned()), ("OTHER".to_owned(), "v3".to_owned())]);
    assert_eq!(h.runtime.sandbox_specs().last().unwrap().env, expected);

    for (name, value) in [("lower", "v"), ("1KEY", "v"), ("KEY-1", "v"), ("", "v"), ("KEY", "")] {
        let (s, e) = put(&h, &env, &bob.token, json!({"name": name, "value": value})).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{name:?}={value:?}: {e}");
    }
    let (s, _) = put(&h, &env, &bob.token, json!({"name": "BIG", "value": "x".repeat(64 * 1024 + 1)})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    let other = fill(routes::SANDBOX_ENV_VAR, &[("s", "dev"), ("name", "OTHER")]);
    assert_eq!(h.req(Method::DELETE, &other, Some(&bob.token), None).await.0, StatusCode::NO_CONTENT);
    assert_eq!(h.req(Method::DELETE, &other, Some(&bob.token), None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(bare_post(&h, &at(routes::SANDBOX_START, "dev"), &ana.token).await.0, StatusCode::OK);
    assert_eq!(h.runtime.sandbox_specs().last().unwrap().env.keys().collect::<Vec<_>>(), ["ANTHROPIC_API_KEY"]);

    // The environment goes with the sandbox.
    assert_eq!(h.req(Method::DELETE, &at(routes::SANDBOX, "dev"), Some(&ana.token), None).await.0, StatusCode::NO_CONTENT);
    create_ok(&h, &ana.token, json!({"slug": "dev"})).await;
    assert_eq!(h.get_ok(&env, &ana.token).await, json!([]));
    assert!(h.runtime.sandbox_specs().last().unwrap().env.is_empty());
}

#[tokio::test]
async fn settings_are_validated_and_reapplied_to_a_running_sandbox() {
    let h = harness().await;
    let (ana, bob) = h.team("shop").await;
    let s = |egress: &[&str], port: Option<u16>, hours: u16| json!({"egress": egress, "port": port, "auto_stop_hours": hours});
    let bad = [
        s(&["not a host"], None, 8),
        s(&["https://api.example.com"], None, 8),
        s(&["example.com:443"], None, 8),
        s(&[], Some(0), 8),
        s(&[], Some(22), 8),
        s(&[], Some(1023), 8),
        s(&[], None, 0),
        s(&[], None, 73),
    ];
    for settings in &bad {
        let (st, e) = create(&h, &ana.token, "acme", json!({"slug": "dev", "settings": settings})).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{settings}: {e}");
    }
    assert!(h.runtime.sandbox_specs().is_empty());

    let v = create_ok(&h, &ana.token, json!({"slug": "dev", "settings": s(&[" API.Example.com", "api.example.com"], None, 2)})).await;
    assert_eq!(v["settings"]["egress"], json!(["api.example.com"]), "normalized");
    assert_eq!(v["stops_at"].as_u64(), Some(START + 2 * HOUR));

    let path = at(routes::SANDBOX_SETTINGS, "dev");
    for settings in &bad {
        let (st, e) = put(&h, &path, &bob.token, settings.clone()).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{settings}: {e}");
    }
    assert_eq!(h.runtime.sandbox_specs().len(), 1, "refused settings change nothing");

    // Running: the VM is replaced with the new settings and the auto-stop clock restarts.
    h.clock.advance(100);
    let (st, v) = put(&h, &path, &bob.token, s(&["registry.npmjs.org", "*.example.com"], Some(3000), 4)).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    let spec = h.runtime.sandbox_specs().last().cloned().unwrap();
    assert_eq!((spec.egress.as_slice(), spec.port), (&["registry.npmjs.org".to_owned(), "*.example.com".to_owned()][..], Some(3000)));
    assert_eq!(v["url"], "https://dev.preview.aho.test");
    assert_eq!(v["stops_at"].as_u64(), Some(START + 100 + 4 * HOUR));
    let stored: SandboxSettings =
        serde_json::from_value(h.get_ok(&at(routes::SANDBOX, "dev"), &ana.token).await["settings"].clone()).unwrap();
    assert_eq!(stored.port, Some(3000));

    // Stopped: the settings are kept for the next start, nothing boots.
    assert_eq!(bare_post(&h, &at(routes::SANDBOX_STOP, "dev"), &ana.token).await.0, StatusCode::OK);
    let (st, v) = put(&h, &path, &bob.token, s(&[], None, 1)).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    assert_eq!((v["state"].as_str(), v["url"].as_str()), (Some("stopped"), None));
    assert_eq!(h.runtime.sandbox_specs().len(), 2);
    assert_eq!(bare_post(&h, &at(routes::SANDBOX_START, "dev"), &ana.token).await.0, StatusCode::OK);
    let spec = h.runtime.sandbox_specs().last().cloned().unwrap();
    assert_eq!((spec.egress.len(), spec.port), (0, None));
}

#[tokio::test]
async fn terminals_and_chats_need_a_running_sandbox_and_carry_the_callers_request() {
    let h = harness().await;
    let (ana, bob) = h.team("shop").await;
    create_ok(&h, &ana.token, json!({"slug": "dev"})).await;
    let terminal = at(routes::SANDBOX_TERMINAL, "dev");
    let chat = at(routes::SANDBOX_CHAT, "dev");

    assert_eq!(bare_post(&h, &at(routes::SANDBOX_STOP, "dev"), &ana.token).await.0, StatusCode::OK);
    let (s, e) = h.post(&terminal, Some(&bob.token), json!({"cols": 100, "rows": 30})).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::CONFLICT, Some("conflict")), "{e}");
    assert_eq!(bare_post(&h, &chat, &bob.token).await.0, StatusCode::CONFLICT);
    assert_eq!(h.get(&at(routes::SANDBOX_FILES, "dev"), &bob.token).await.0, StatusCode::CONFLICT);
    assert_eq!(h.get(&format!("{}?path=a.txt", at(routes::SANDBOX_FILE, "dev")), &bob.token).await.0, StatusCode::CONFLICT);
    assert!(h.host.terminals().is_empty() && h.host.chats().is_empty());

    assert_eq!(bare_post(&h, &at(routes::SANDBOX_START, "dev"), &ana.token).await.0, StatusCode::OK);
    let access = h.ok(&terminal, &bob.token, json!({"cols": 100, "rows": 30})).await;
    assert_eq!(access["url"], "wss://sessions.test/terminal");
    assert!(access["token"].as_str().is_some_and(|t| !t.is_empty()));
    let login_shell = ExecRequest {
        argv: vec!["bash".into(), "-l".into()],
        pty: Some(PtySize { cols: 100, rows: 30 }),
        env: BTreeMap::from([("TERM".to_owned(), "xterm-256color".to_owned())]),
        cwd: Some("/workspace".into()),
    };
    assert_eq!(h.host.terminals()[0], SandboxTerminal { sandbox: "dev".into(), user_id: bob.id.clone(), exec: login_shell.clone() });

    h.ok(&terminal, &ana.token, json!({"cols": 5000, "rows": 0, "command": ["aho"]})).await;
    let aho = ExecRequest { argv: vec!["aho".into()], pty: Some(PtySize { cols: 1000, rows: 1 }), ..login_shell };
    assert_eq!(h.host.terminals()[1], SandboxTerminal { sandbox: "dev".into(), user_id: ana.id.clone(), exec: aho });
    assert_eq!(h.post(&terminal, Some(&ana.token), json!({"cols": 80, "rows": 24, "command": []})).await.0, StatusCode::BAD_REQUEST);
    // `no_pty` (like `ssh -T`): pipes, same command, cwd and environment.
    h.ok(&terminal, &ana.token, json!({"cols": 80, "rows": 24, "command": ["git", "status"], "no_pty": true})).await;
    let piped = &h.host.terminals()[2];
    assert_eq!((piped.exec.argv.as_slice(), piped.exec.pty), (&["git".to_owned(), "status".to_owned()][..], None));
    assert_eq!(piped.exec.cwd.as_deref(), Some("/workspace"));

    let (s, c) = bare_post(&h, &chat, &bob.token).await;
    assert_eq!(s, StatusCode::OK, "{c}");
    assert_eq!(c["rpc_url"], "wss://sessions.test/sandbox-rpc/dev");
    let resumed = h.ok(&chat, &ana.token, json!({"session_id": "abc123"})).await;
    assert_eq!(resumed["session_id"], "abc123");
    assert_eq!(
        h.host.chats(),
        [
            SandboxChatStart { sandbox: "dev".into(), user_id: bob.id.clone(), session_id: None },
            SandboxChatStart { sandbox: "dev".into(), user_id: ana.id.clone(), session_id: Some("abc123".into()) },
        ]
    );
    assert_eq!(h.post(&chat, Some(&ana.token), json!({"session_id": "bad id!"})).await.0, StatusCode::BAD_REQUEST);
    assert_eq!(h.host.chats().len(), 2);
}

#[tokio::test]
async fn the_files_of_a_running_sandbox_come_from_the_runtime() {
    let h = harness().await;
    let (ana, bob) = h.team("shop").await;
    create_ok(&h, &ana.token, json!({"slug": "dev"})).await;
    h.runtime.put_sandbox_file("dev", "src/main.rs", "fn main() {}\n");
    let tree = h.get_ok(&at(routes::SANDBOX_FILES, "dev"), &bob.token).await;
    let paths: Vec<&str> = tree["files"].as_array().unwrap().iter().map(|f| f["path"].as_str().unwrap()).collect();
    assert_eq!(paths, ["src", "src/main.rs"]);
    let file = |path: &str| format!("{}?path={path}", at(routes::SANDBOX_FILE, "dev"));
    let content = h.get_ok(&file("src/main.rs"), &bob.token).await;
    assert_eq!((content["text"].as_str(), content["size"].as_u64()), (Some("fn main() {}\n"), Some(13)));
    assert_eq!(h.get(&file("missing.rs"), &bob.token).await.0, StatusCode::NOT_FOUND);
    for bad in ["../etc/passwd", "%2Fetc%2Fpasswd", ""] {
        assert_eq!(h.get(&file(bad), &bob.token).await.0, StatusCode::BAD_REQUEST, "{bad}");
    }
    assert_eq!(h.get(&at(routes::SANDBOX_FILE, "dev"), &bob.token).await.0, StatusCode::BAD_REQUEST, "no ?path=");
}

#[tokio::test]
async fn sandboxes_stop_themselves_after_their_auto_stop_hours() {
    let h = harness().await;
    let (ana, _) = h.team("shop").await;
    create_ok(&h, &ana.token, json!({"slug": "long"})).await;
    create_ok(&h, &ana.token, json!({"slug": "short", "settings": {"egress": [], "auto_stop_hours": 1}})).await;
    assert_eq!(sandboxes::auto_stop(&h.state).await.unwrap(), 0);

    h.clock.advance(HOUR - 1);
    assert_eq!(sandboxes::auto_stop(&h.state).await.unwrap(), 0);
    h.clock.advance(1);
    assert_eq!(sandboxes::auto_stop(&h.state).await.unwrap(), 1);
    assert_eq!(stops(&h), ["short"]);
    let short = h.get_ok(&at(routes::SANDBOX, "short"), &ana.token).await;
    assert_eq!((short["state"].as_str(), short["stops_at"].as_u64()), (Some("stopped"), None));
    assert_eq!(h.get_ok(&at(routes::SANDBOX, "long"), &ana.token).await["state"], "running");
    assert_eq!(sandboxes::auto_stop(&h.state).await.unwrap(), 0, "a stopped sandbox is not stopped again");

    // Starting `long` again an hour before its deadline restarts its clock.
    h.clock.advance(6 * HOUR);
    assert_eq!(bare_post(&h, &at(routes::SANDBOX_START, "long"), &ana.token).await.0, StatusCode::OK);
    h.clock.advance(8 * HOUR - 1);
    assert_eq!(sandboxes::auto_stop(&h.state).await.unwrap(), 0);
    h.clock.advance(1);
    assert_eq!(sandboxes::auto_stop(&h.state).await.unwrap(), 1);
    assert_eq!(stops(&h), ["short", "long"]);
}

#[tokio::test]
async fn a_public_port_is_served_at_the_preview_host_to_org_members_only() {
    let h = harness().await;
    let (ana, _) = h.team("shop").await;
    create_ok(&h, &ana.token, json!({"slug": "web"})).await;
    let v = create_ok(&h, &ana.token, json!({"slug": "dev", "settings": {"egress": [], "port": 8080, "auto_stop_hours": 8}})).await;
    assert_eq!(v["url"], "https://dev.preview.aho.test");

    let host = "dev.preview.aho.test";
    let all = sites(&h, None).await;
    let s = site(&all, host).expect("the sandbox's port is served");
    assert_eq!((s.project.as_str(), s.stage, s.state), ("dev", Stage::Preview, SiteState::Active));
    assert_eq!(s.upstreams, ["10.0.1.1:8080"]);
    assert_eq!(s.auth, Auth::MagicLink { allowed_domains: Vec::new() });
    assert_eq!(s.edge, EdgeConfig::default(), "every route needs the sign-in");
    assert_eq!(s.allowed_emails, ["ana@acme.test", "bob@acme.test"]);
    assert!(site(&all, "web.preview.aho.test").is_none(), "no port, no site");

    // A new member may sign in too.
    let dana = h.signup("dana@acme.test", "Dana").await;
    h.ok(&fill(routes::ORG_MEMBERS, &[("org", "acme")]), &ana.token, json!({"email": dana.email, "owner": false})).await;
    let with_dana = sites(&h, Some(all.version)).await;
    assert_eq!(site(&with_dana, host).unwrap().allowed_emails, ["ana@acme.test", "bob@acme.test", "dana@acme.test"]);

    // Stopping takes it off the edge (once as a tombstone in the delta); starting brings it back.
    assert_eq!(bare_post(&h, &at(routes::SANDBOX_STOP, "dev"), &ana.token).await.0, StatusCode::OK);
    let delta = sites(&h, Some(with_dana.version)).await;
    assert_eq!(site(&delta, host).map(|s| s.state), Some(SiteState::Tombstoned));
    assert!(site(&sites(&h, None).await, host).is_none());
    assert_eq!(bare_post(&h, &at(routes::SANDBOX_START, "dev"), &ana.token).await.0, StatusCode::OK);
    assert_eq!(site(&sites(&h, None).await, host).map(|s| s.state), Some(SiteState::Active));

    // A crashed VM has no address to serve.
    h.runtime.fail_sandbox("dev", "oom");
    assert!(site(&sites(&h, None).await, host).is_none());
    assert_eq!(bare_post(&h, &at(routes::SANDBOX_START, "dev"), &ana.token).await.0, StatusCode::OK);

    // Unsetting the port removes it; another port serves there.
    let settings = at(routes::SANDBOX_SETTINGS, "dev");
    let (_, v) = put(&h, &settings, &ana.token, json!({"egress": [], "port": null, "auto_stop_hours": 8})).await;
    assert!(v["url"].is_null());
    assert!(site(&sites(&h, None).await, host).is_none());
    put(&h, &settings, &ana.token, json!({"egress": [], "port": 9000, "auto_stop_hours": 8})).await;
    assert_eq!(site(&sites(&h, None).await, host).unwrap().upstreams, ["10.0.1.1:9000"]);

    // Deleting removes it.
    assert_eq!(h.req(Method::DELETE, &at(routes::SANDBOX, "dev"), Some(&ana.token), None).await.0, StatusCode::NO_CONTENT);
    assert!(site(&sites(&h, None).await, host).is_none());
}

#[tokio::test]
async fn sandbox_urls_follow_the_preview_url_template() {
    let h = H::with(|c| {
        c.sandbox_image = SandboxImage::parse(&image());
        c.preview_url = Some("http://{project}.preview.localhost:8443".into());
    })
    .await;
    let (ana, _) = h.team("shop").await;
    let v = create_ok(&h, &ana.token, json!({"slug": "dev", "settings": {"egress": [], "port": 3000, "auto_stop_hours": 8}})).await;
    assert_eq!(v["url"], "http://dev.preview.localhost:8443");
}
