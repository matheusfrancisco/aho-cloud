//! The caller's workspace from the console: its files, "Deploy to preview" (the session host
//! packs the files; the archive and aho session are re-verified and recorded like uploads; the
//! same proposal pipeline as `POST releases` runs), and "Go live" for a one-person org.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "test code: a failed unwrap is a failed test")]

mod common;

use aho_log::{LogStore, MemoryLog};
use axum::http::{Method, StatusCode};
use base64::Engine as _;
use cloud_api::internal::{PackagedSession, WorkspacePackage, WorkspaceRef};
use cloud_api::routes::{self, fill};
use cloud_contract::draft::{Draft, Evidence, Strength};
use cloud_contract::ledger::{Proposer, Stage};
use cloud_contract::manifest::Source;
use cloud_control_plane::approvals::{AuditKeys, audit_verifier};
use cloud_control_plane::blobs::content_hash;
use cloud_control_plane::store::BlobKind;
use cloud_control_plane::webauthn::RelyingParty;
use common::*;
use serde_json::{Value, json};

const ARCHIVE: &[u8] = b"\x1f\x8b\x08\x00 deterministic tar.gz of the workspace files";

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

/// The head hash aho-log computes for `log`.
fn head_of(log: &str) -> String {
    let l = MemoryLog::from_jsonl(log).unwrap();
    l.get(l.head().unwrap()).unwrap().hash.to_string()
}

/// A `deploy.json` as a workspace would carry it: built from source by its Dockerfile.
fn deploy_json(project: &str) -> Value {
    let mut m = manifest(project);
    m["source"] = json!({"kind": "git", "repo": "https://example.test/shop", "commit": "1".repeat(40)});
    m["build"] = json!({"kind": "dockerfile"});
    m
}

/// What the session host answers for a workspace holding `ARCHIVE`, optionally a `deploy.json`
/// and an aho session log.
fn package(manifest: Option<Value>, session: Option<(&str, &str)>) -> WorkspacePackage {
    WorkspacePackage {
        archive: b64(ARCHIVE),
        archive_hash: content_hash(ARCHIVE),
        files: 3,
        session: session.map(|(id, log)| PackagedSession {
            session_id: id.into(),
            head_hash: head_of(log),
            entries: u64::try_from(log.lines().count()).unwrap(),
            jsonl: b64(log.as_bytes()),
        }),
        manifest: manifest.map(|m| serde_json::from_value(m).unwrap()),
        manifest_error: None,
        draft: Draft::default(),
    }
}

async fn deploy(h: &H, project: &str, token: &str, body: Value) -> (StatusCode, Value) {
    h.post(&fill(routes::WORKSPACE_DEPLOY, &[("p", project)]), Some(token), body).await
}

/// Ids of the releases proposed in `project`'s ledger.
fn proposals(h: &H, project: &str) -> Vec<String> {
    h.state.ledgers.get(project).unwrap().state().unwrap().releases.into_keys().collect()
}

/// Ana alone in org `solo` with project `project`, device-logged-in.
async fn solo(h: &H, project: &str) -> Person {
    let mut ana = h.signup("ana@solo.test", "Ana").await;
    h.ok(routes::ORGS, &ana.token, json!({"slug": "solo", "name": "Solo"})).await;
    h.ok(&fill(routes::ORG_PROJECTS, &[("org", "solo")]), &ana.token, json!({"slug": project})).await;
    h.device_login(&mut ana, 1).await;
    ana
}

fn members_changed(h: &H, project: &str) -> Vec<u64> {
    h.state
        .ledgers
        .get(project)
        .unwrap()
        .entries()
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.body {
            cloud_contract::ledger::LedgerBody::MembersChanged { count } => Some(u64::from(count)),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_workspace_with_deploy_json_ships_its_files_attested_and_previews() {
    let h = H::new().await;
    let (ana, _) = h.team("shop").await;
    let log = session_log(&["wrote deploy.json", "ran the tests"]);
    h.host.put_package("shop", &ana.id, package(Some(deploy_json("shop")), Some(("0a1b2c", &log))));
    let hash = content_hash(ARCHIVE);

    // The console posts nothing but the click.
    let (s, v) = h.req(Method::POST, &fill(routes::WORKSPACE_DEPLOY, &[("p", "shop")]), Some(&ana.token), None).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["status"], "proposed");
    let id = v["release"]["id"].as_str().unwrap().to_owned();
    assert_eq!(v["release"]["manifest"]["source"], json!({"kind": "archive", "hash": hash}), "source is always the packed archive");
    assert_eq!(v["release"]["provenance"], json!({"kind": "attested", "session_id": "0a1b2c", "head_hash": head_of(&log)}));
    assert_eq!(h.host.packed(), [WorkspaceRef { project: "shop".into(), user_id: ana.id.clone() }], "the caller's own workspace");

    // The archive is stored under its hash exactly like an upload, and the session is recorded.
    assert_eq!(h.state.blobs.get(&hash).await.unwrap().unwrap(), ARCHIVE);
    assert_eq!(h.state.store.blob("shop", &hash).await.unwrap().unwrap().kind, BlobKind::Archive);
    let session = h.state.store.session("shop", "0a1b2c", &head_of(&log)).await.unwrap().unwrap();
    assert_eq!(session.entries, 2);
    assert_eq!(h.state.blobs.get(&session.blob_hash).await.unwrap().unwrap(), log.as_bytes());

    // Build → preview, as for any proposal.
    h.state.idle().await;
    let r = h.release("shop", &ana.token, &id).await;
    assert_eq!((r["status"].as_str(), r["stage"].as_str()), (Some("active"), Some("preview")), "{r}");
    let jobs = h.builder.jobs();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0].source, Source::Archive { hash: hash.clone() });
    assert_eq!(jobs[0].archive_url.as_deref(), Some(format!("http://control.test/internal/v1/archives/{hash}").as_str()));
    let preview = &h.runtime.deployments()[0];
    assert_eq!((preview.stage, preview.release.as_str()), (Stage::Preview, id.as_str()));
    assert_eq!(h.runtime.running("shop", Stage::Preview).as_deref(), Some(id.as_str()));
    let st = h.state.ledgers.get("shop").unwrap().state().unwrap();
    assert_eq!(st.releases[&id].proposer, Proposer::User { user_id: ana.id.clone() });
}

#[tokio::test]
async fn without_deploy_json_the_draft_comes_back_and_the_confirmed_manifest_ships() {
    let h = H::new().await;
    let (ana, _) = h.team("shop").await;
    let mut pkg = package(None, None);
    let evidence = || Evidence { uri: "file:Dockerfile:1".into(), note: "EXPOSE 8080".into() };
    pkg.draft.offer("/processes/0/port", json!(8080), Strength::Inferred, evidence());
    pkg.draft.offer("/source/kind", json!("git"), Strength::Inferred, evidence());
    h.host.put_package("shop", &ana.id, pkg);
    let hash = content_hash(ARCHIVE);

    let (s, v) = deploy(&h, "shop", &ana.token, json!({})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["status"], "needs_manifest");
    let fields = &v["draft"]["fields"];
    assert_eq!(fields["/source"]["value"], json!({"kind": "archive", "hash": hash}));
    assert_eq!((fields["/source"]["strength"].as_str(), fields["/source"]["confirmed"].as_bool()), (Some("declared"), Some(true)));
    assert_eq!(fields["/project"]["value"], "shop");
    assert!(fields.get("/source/kind").is_none(), "a drafted source cannot shadow the archive: {fields}");
    assert_eq!(fields["/processes/0/port"]["strength"], "inferred", "the workspace's own findings still need the human");
    assert!(proposals(&h, "shop").is_empty(), "a draft proposes nothing");
    assert!(h.builder.jobs().is_empty());

    // The console confirms (here: a full manifest with some other source) and sends it back.
    let (s, v) = deploy(&h, "shop", &ana.token, json!({"manifest": deploy_json("shop")})).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["status"], "proposed");
    assert_eq!(v["release"]["manifest"]["source"], json!({"kind": "archive", "hash": hash}));
    assert_eq!(v["release"]["provenance"]["kind"], "unattested", "no aho session in the workspace");
    h.state.idle().await;
    let r = h.release("shop", &ana.token, v["release"]["id"].as_str().unwrap()).await;
    assert_eq!(r["stage"], "preview", "{r}");
}

#[tokio::test]
async fn a_request_manifest_wins_over_deploy_json_and_must_name_this_project() {
    let h = H::new().await;
    let (ana, _) = h.team("shop").await;
    let mut from_file = deploy_json("shop");
    from_file["egress"] = json!(["api.stripe.com", "from-file.example.com"]);
    h.host.put_package("shop", &ana.id, package(Some(from_file), None));

    let (s, e) = deploy(&h, "shop", &ana.token, json!({"manifest": deploy_json("other")})).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::BAD_REQUEST, Some("invalid")), "{e}");
    assert!(e["message"].as_str().unwrap().contains("`other`"), "{e}");
    let (s, e) = h.post(&fill(routes::WORKSPACE_DEPLOY, &[("p", "shop")]), Some(&ana.token), json!({"manifest": 7})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{e}");
    assert!(proposals(&h, "shop").is_empty());

    let (s, v) = deploy(&h, "shop", &ana.token, json!({"manifest": deploy_json("shop")})).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["release"]["manifest"]["egress"], json!(["api.stripe.com"]), "the request's manifest, not deploy.json");

    // deploy.json alone must name the project too.
    h.host.put_package("shop", &ana.id, package(Some(deploy_json("other")), None));
    let (s, _) = deploy(&h, "shop", &ana.token, json!({})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(proposals(&h, "shop").len(), 1);
}

#[tokio::test]
async fn refused_deploys_propose_nothing() {
    let h = H::new().await;
    let (ana, _) = h.team("shop").await;
    let nothing_proposed = |h: &H| proposals(h, "shop").is_empty() && h.builder.jobs().is_empty();

    // An unusable deploy.json is refused with the session host's reason, not drafted around.
    let mut pkg = package(None, None);
    pkg.manifest_error = Some("deploy.json: not a deploy manifest: missing field `processes`".into());
    h.host.put_package("shop", &ana.id, pkg);
    let (s, e) = deploy(&h, "shop", &ana.token, json!({})).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::UNPROCESSABLE_ENTITY, Some("refused")), "{e}");
    assert_eq!(e["message"], "deploy.json: not a deploy manifest: missing field `processes`");
    assert!(nothing_proposed(&h));
    assert!(h.state.store.blob("shop", &content_hash(ARCHIVE)).await.unwrap().is_none(), "nothing stored without a manifest");

    // An archive that does not hash to what the session host named is its fault: internal.
    let mut pkg = package(Some(deploy_json("shop")), None);
    pkg.archive_hash = content_hash(b"some other archive");
    h.host.put_package("shop", &ana.id, pkg);
    let (s, e) = deploy(&h, "shop", &ana.token, json!({})).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::BAD_GATEWAY, Some("internal")), "{e}");
    let mut pkg = package(Some(deploy_json("shop")), None);
    pkg.archive = "not base64!".into();
    h.host.put_package("shop", &ana.id, pkg);
    let (s, e) = deploy(&h, "shop", &ana.token, json!({})).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::BAD_GATEWAY, Some("internal")), "{e}");
    assert!(h.state.store.blob("shop", &content_hash(ARCHIVE)).await.unwrap().is_none());

    // A session log edited in the workspace fails its chain here, as an upload would.
    let log = session_log(&["drafted the manifest", "proposed"]);
    let tampered = log.replacen("drafted the manifest", "drafted the manifesto", 1);
    let mut pkg = package(Some(deploy_json("shop")), Some(("0a1b2c", &log)));
    pkg.session.as_mut().unwrap().jsonl = b64(tampered.as_bytes());
    h.host.put_package("shop", &ana.id, pkg);
    let (s, e) = deploy(&h, "shop", &ana.token, json!({})).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::BAD_REQUEST, Some("invalid")), "{e}");
    assert!(e["message"].as_str().unwrap().starts_with("session log refused"), "{e}");
    assert!(h.state.store.sessions("shop").await.unwrap().is_empty(), "a tampered log is never recorded");

    // An intact log whose head is not the one the session host named.
    let mut pkg = package(Some(deploy_json("shop")), Some(("0a1b2c", &log)));
    pkg.session.as_mut().unwrap().head_hash = "0".repeat(64);
    h.host.put_package("shop", &ana.id, pkg);
    let (s, e) = deploy(&h, "shop", &ana.token, json!({})).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::BAD_GATEWAY, Some("internal")), "{e}");
    assert!(h.state.store.sessions("shop").await.unwrap().is_empty());
    assert!(h.state.store.blob("shop", &content_hash(ARCHIVE)).await.unwrap().is_none());
    assert!(nothing_proposed(&h));

    // The same validation as `POST releases`: every refusal at once.
    let mut bad = deploy_json("shop");
    bad["api_version"] = json!("deploy.aho.cloud/v0");
    bad["secrets"][0]["reach"] = json!(["evil.test"]);
    h.host.put_package("shop", &ana.id, package(Some(bad), None));
    let (s, e) = deploy(&h, "shop", &ana.token, json!({})).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::UNPROCESSABLE_ENTITY, Some("refused")), "{e}");
    let codes: Vec<&str> = e["refusals"].as_array().unwrap().iter().map(|r| r["code"].as_str().unwrap()).collect();
    assert!(codes.contains(&"api_version") && codes.contains(&"secret_reach_not_in_egress"), "{codes:?}");
    assert!(nothing_proposed(&h), "stored like an upload, but never proposed");
}

#[tokio::test]
async fn only_members_reach_a_workspace_and_only_their_own() {
    let h = H::new().await;
    let (ana, bob) = h.team("shop").await;
    h.host.put_package("shop", &ana.id, package(Some(deploy_json("shop")), None));
    h.host.put_file("shop", &ana.id, "deploy.json", "{}");
    let eve = h.signup("eve@else.test", "Eve").await;

    let files = fill(routes::WORKSPACE_FILES, &[("p", "shop")]);
    let file = format!("{}?path=deploy.json", fill(routes::WORKSPACE_FILE, &[("p", "shop")]));
    for (s, e) in [deploy(&h, "shop", &eve.token, json!({})).await, h.get(&files, &eve.token).await, h.get(&file, &eve.token).await] {
        assert_eq!((s, e["code"].as_str()), (StatusCode::FORBIDDEN, Some("forbidden")), "{e}");
    }
    for (s, _) in [
        h.req(Method::POST, &fill(routes::WORKSPACE_DEPLOY, &[("p", "shop")]), None, Some(json!({}))).await,
        h.req(Method::GET, &files, None, None).await,
    ] {
        assert_eq!(s, StatusCode::UNAUTHORIZED);
    }
    // Bob is a member but has no workspace of his own; Ana's is not his to ship or read.
    for (s, e) in [deploy(&h, "shop", &bob.token, json!({})).await, h.get(&files, &bob.token).await, h.get(&file, &bob.token).await] {
        assert_eq!((s, e["code"].as_str()), (StatusCode::NOT_FOUND, Some("not_found")), "{e}");
    }
    assert!(h.host.packed().is_empty());
    assert!(proposals(&h, "shop").is_empty());
}

#[tokio::test]
async fn files_and_file_proxy_the_callers_workspace() {
    let h = H::new().await;
    let (ana, _) = h.team("shop").await;
    h.host.put_file("shop", &ana.id, "deploy.json", "{\"project\": \"shop\"}");
    h.host.put_file("shop", &ana.id, "src/main.rs", "fn main() {}\n");

    let tree = h.get_ok(&fill(routes::WORKSPACE_FILES, &[("p", "shop")]), &ana.token).await;
    assert_eq!(
        tree,
        json!({"files": [
            {"path": "deploy.json", "kind": "file", "size": 19},
            {"path": "src", "kind": "dir", "size": 0},
            {"path": "src/main.rs", "kind": "file", "size": 13},
        ], "truncated": false})
    );

    let base = fill(routes::WORKSPACE_FILE, &[("p", "shop")]);
    let content = h.get_ok(&format!("{base}?path=src/main.rs"), &ana.token).await;
    assert_eq!(content, json!({"path": "src/main.rs", "size": 13, "text": "fn main() {}\n", "truncated": false}));
    let (s, e) = h.get(&format!("{base}?path=src/lib.rs"), &ana.token).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::NOT_FOUND, Some("not_found")), "{e}");
    for bad in ["", "?path=", "?path=../../etc/passwd", "?path=/etc/passwd", "?path=src/../../x", "?path=a%00b"] {
        let (s, e) = h.get(&format!("{base}{bad}"), &ana.token).await;
        assert_eq!((s, e["code"].as_str()), (StatusCode::BAD_REQUEST, Some("invalid")), "{bad}: {e}");
    }
}

#[tokio::test]
async fn a_solo_owner_deploys_the_workspace_and_takes_it_live_alone() {
    let h = H::new().await;
    let ana = solo(&h, "blog").await;
    assert_eq!(members_changed(&h, "blog"), [1], "recorded at project creation");
    let log = session_log(&["built the blog"]);
    h.host.put_package("blog", &ana.id, package(Some(deploy_json("blog")), Some(("s1", &log))));
    h.set_secret("blog", &ana.token, "STRIPE_KEY", Stage::Live, "sk_live_solo_value_01").await;

    let (s, v) = deploy(&h, "blog", &ana.token, json!({})).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    h.state.idle().await;
    let r = h.release("blog", &ana.token, v["release"]["id"].as_str().unwrap()).await;
    assert_eq!(r["stage"], "preview");
    assert_eq!(
        r["requirement"],
        json!({"approvals": 1, "owner_required": true}),
        "the first release widens everything; a one-person org needs its owner once, not a second approver"
    );

    // The owner signs each stage with her device key and promotes: preview → staging → live.
    h.take_live("blog", &[&ana], &r).await;
    let id = r["id"].as_str().unwrap();
    let live = h.release("blog", &ana.token, id).await;
    assert_eq!((live["status"].as_str(), live["stage"].as_str()), (Some("active"), Some("live")), "{live}");
    assert_eq!(h.runtime.running("blog", Stage::Live).as_deref(), Some(id));
    let deployed = h.runtime.deployments().last().cloned().unwrap();
    assert_eq!((deployed.stage, deployed.secrets["STRIPE_KEY"].as_str()), (Stage::Live, "sk_live_solo_value_01"));

    // Each approval is a real signature over its stage's message.
    let mut keys = AuditKeys { device_keys: Vec::new(), passkeys: Vec::new() };
    keys.device_keys.extend(h.state.store.device_keys_for_user(&ana.id).await.unwrap());
    let rp = RelyingParty { id: RP_ID.into(), origin: ORIGIN.into(), name: "t".into() };
    h.state.ledgers.get("blog").unwrap().audit(&audit_verifier(keys, rp, "blog".into(), "solo".into())).unwrap();
    let st = h.state.ledgers.get("blog").unwrap().state().unwrap();
    let stages: Vec<Stage> = st.releases[id].approvals.iter().map(|(s, _)| *s).collect();
    assert_eq!(stages, [Stage::Staging, Stage::Live]);
}

#[tokio::test]
async fn the_quorum_cap_follows_org_membership() {
    let h = H::new().await;
    let ana = solo(&h, "blog").await;
    h.set_secret("blog", &ana.token, "STRIPE_KEY", Stage::Live, "sk_live_solo_value_02").await;
    let r = h.propose("blog", &ana.token, manifest("blog"), unattested()).await;
    let id = r["id"].as_str().unwrap();
    assert_eq!(r["requirement"], json!({"approvals": 1, "owner_required": true}));
    let (s, _) = h.approve("blog", &ana, &r, Stage::Staging).await;
    assert_eq!(s, StatusCode::OK);
    let (s, v) = h.promote("blog", &ana.token, id, Stage::Staging).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    h.approve("blog", &ana, &r, Stage::Live).await;

    // Bob joins: every project of the org now needs the policy's two approvals again.
    let bob = h.signup("bob@solo.test", "Bob").await;
    h.ok(&fill(routes::ORG_MEMBERS, &[("org", "solo")]), &ana.token, json!({"email": bob.email, "owner": false})).await;
    assert_eq!(members_changed(&h, "blog"), [1, 2]);
    assert_eq!(h.release("blog", &ana.token, id).await["requirement"], json!({"approvals": 2, "owner_required": true}));
    let (s, e) = h.promote("blog", &ana.token, id, Stage::Live).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::CONFLICT, Some("quorum_not_met")), "ana's lone live approval no longer suffices: {e}");

    // Changing a member's role is not a membership change; a new project starts at the count.
    h.ok(&fill(routes::ORG_MEMBERS, &[("org", "solo")]), &ana.token, json!({"email": bob.email, "owner": true})).await;
    assert_eq!(members_changed(&h, "blog"), [1, 2]);
    h.ok(&fill(routes::ORG_PROJECTS, &[("org", "solo")]), &ana.token, json!({"slug": "docs"})).await;
    assert_eq!(members_changed(&h, "docs"), [2]);
}

#[tokio::test]
async fn a_two_member_org_still_needs_two_approvals_to_widen() {
    let h = H::new().await;
    let (ana, bob) = h.team("shop").await;
    assert_eq!(members_changed(&h, "shop"), [2]);
    h.host.put_package("shop", &ana.id, package(Some(deploy_json("shop")), None));
    let (s, v) = deploy(&h, "shop", &ana.token, json!({})).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    h.state.idle().await;
    let r = h.release("shop", &ana.token, v["release"]["id"].as_str().unwrap()).await;
    assert_eq!(r["requirement"], json!({"approvals": 2, "owner_required": true}));
    h.approve("shop", &ana, &r, Stage::Staging).await;
    let (s, e) = h.promote("shop", &ana.token, r["id"].as_str().unwrap(), Stage::Staging).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::CONFLICT, Some("quorum_not_met")), "{e}");
    h.approve("shop", &bob, &r, Stage::Staging).await;
    let (s, v) = h.promote("shop", &ana.token, r["id"].as_str().unwrap(), Stage::Staging).await;
    assert_eq!(s, StatusCode::OK, "{v}");
}
