//! Releases end to end: propose → build → preview → approvals → staging → live; refusals,
//! provenance, taint, rollback, standing orders, and secrets never leaving the server.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "test code: a failed unwrap is a failed test")]

mod common;

use axum::http::{Method, StatusCode};
use base64::Engine as _;
use cloud_api::internal::BuildResult;
use cloud_api::routes::{self, fill};
use cloud_contract::diff::Category;
use cloud_contract::ledger::{Stage, approval_message};
use cloud_contract::policy::{ReleasePolicy, StandingOrder};
use cloud_control_plane::approvals::{AuditKeys, audit_verifier, policy_message, standing_orders_message};
use cloud_control_plane::fakes::{FakeBuilder, RuntimeCall};
use cloud_control_plane::webauthn::RelyingParty;
use common::*;
use serde_json::{Value, json};

fn id(r: &Value) -> &str {
    r["id"].as_str().unwrap()
}

#[tokio::test]
async fn propose_build_preview_approve_staging_live() {
    let h = H::new().await;
    let (ana, bob) = h.team("shop").await;
    h.set_secret("shop", &ana.token, "STRIPE_KEY", Stage::Live, "sk_live_real_value_1").await;

    let r = h.propose("shop", &ana.device().token, manifest("shop"), unattested()).await;
    assert_eq!(r["status"], "active", "built and clean → auto-activated at preview: {r}");
    assert_eq!(r["stage"], "preview");
    assert_eq!(r["image_digest"], FakeBuilder::digest_for(id(&r)));
    assert_eq!(r["requirement"], json!({"approvals": 2, "owner_required": true}), "first release widens everything, auth needs an owner");
    let jobs = h.builder.jobs();
    assert_eq!((jobs.len(), jobs[0].release.as_str()), (1, id(&r)));

    let preview = &h.runtime.deployments()[0];
    assert_eq!(preview.stage, Stage::Preview);
    assert_eq!(preview.secrets["STRIPE_KEY"], "aho-cloud-placeholder:STRIPE_KEY");

    // Staging needs the quorum.
    let (s, e) = h.promote("shop", &ana.token, id(&r), Stage::Staging).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::CONFLICT, Some("quorum_not_met")));
    let (s, _) = h.approve("shop", &bob, &r, Stage::Staging).await;
    assert_eq!(s, StatusCode::OK);
    let (s, e) = h.promote("shop", &ana.token, id(&r), Stage::Staging).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::CONFLICT, Some("quorum_not_met")), "one of two approvals, and no owner yet");
    let (s, e) = h.approve("shop", &bob, &r, Stage::Staging).await;
    assert_eq!(s, StatusCode::CONFLICT, "one person approves once per stage: {e}");
    h.approve("shop", &ana, &r, Stage::Staging).await;
    let (s, v) = h.promote("shop", &ana.token, id(&r), Stage::Staging).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["stage"], "staging");
    let staging = h.runtime.deployments().last().cloned().unwrap();
    assert_eq!((staging.stage, staging.secrets["STRIPE_KEY"].as_str()), (Stage::Staging, "aho-cloud-placeholder:STRIPE_KEY"));

    // Staging approvals do not carry over to live.
    let (s, e) = h.promote("shop", &ana.token, id(&r), Stage::Live).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::CONFLICT, Some("quorum_not_met")));
    h.approve("shop", &ana, &r, Stage::Live).await;
    h.approve("shop", &bob, &r, Stage::Live).await;
    let (s, v) = h.promote("shop", &ana.token, id(&r), Stage::Live).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!((v["status"].as_str(), v["stage"].as_str()), (Some("active"), Some("live")));
    let live = h.runtime.deployments().last().cloned().unwrap();
    assert_eq!((live.stage, live.secrets["STRIPE_KEY"].as_str()), (Stage::Live, "sk_live_real_value_1"), "only live gets real secrets");
    assert_eq!(h.runtime.running("shop", Stage::Live).as_deref(), Some(id(&r)));

    let project = h.get_ok(&fill(routes::PROJECT, &[("p", "shop")]), &bob.token).await;
    assert_eq!(project["active"].as_array().unwrap().len(), 3);

    // A code-only follow-up needs one approval.
    let mut m2 = manifest("shop");
    m2["source"]["digest"] = json!(format!("sha256:{}", "b".repeat(64)));
    let r2 = h.propose("shop", &ana.token, m2, unattested()).await;
    assert_eq!(r2["requirement"], json!({"approvals": 1, "owner_required": false}));
    assert_eq!(r2["diff"]["changes"][0]["category"], "code");

    // Every recorded signature re-verifies against the registered credentials.
    let store = &h.state.store;
    let mut keys = AuditKeys { device_keys: Vec::new(), passkeys: Vec::new() };
    for u in [&ana.id, &bob.id] {
        keys.device_keys.extend(store.device_keys_for_user(u).await.unwrap());
        keys.passkeys.extend(store.passkeys_for_user(u).await.unwrap());
    }
    let rp = RelyingParty { id: RP_ID.into(), origin: ORIGIN.into(), name: "t".into() };
    let ledger = h.state.ledgers.get("shop").unwrap();
    ledger.audit(&audit_verifier(keys, rp.clone(), "shop".into(), "acme".into())).unwrap();
    let empty = AuditKeys { device_keys: Vec::new(), passkeys: Vec::new() };
    assert!(ledger.audit(&audit_verifier(empty, rp, "shop".into(), "acme".into())).is_err(), "without the keys nothing verifies");
}

#[tokio::test]
async fn refused_manifests_list_every_refusal_and_record_nothing() {
    let h = H::new().await;
    let (ana, _) = h.team("shop").await;
    let mut bad = manifest("shop");
    bad["api_version"] = json!("deploy.aho.cloud/v0");
    bad["processes"][0]["port"] = Value::Null;
    bad["secrets"][0]["reach"] = json!(["evil.test"]);
    let (s, e) =
        h.post(&fill(routes::RELEASES, &[("p", "shop")]), Some(&ana.token), json!({"manifest": bad, "provenance": unattested()})).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::UNPROCESSABLE_ENTITY, Some("refused")));
    let codes: Vec<&str> = e["refusals"].as_array().unwrap().iter().map(|r| r["code"].as_str().unwrap()).collect();
    for want in ["api_version", "public_without_port", "secret_reach_not_in_egress"] {
        assert!(codes.contains(&want), "{want} in {codes:?}");
    }
    let (s, e) = h
        .post(
            &fill(routes::RELEASES, &[("p", "shop")]),
            Some(&ana.token),
            json!({"manifest": manifest("shop"), "edge_toml": "default = \"edge\"", "provenance": unattested()}),
        )
        .await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "edge mode needs an edge login: {e}");
    let (s, _) = h
        .post(
            &fill(routes::RELEASES, &[("p", "shop")]),
            Some(&ana.token),
            json!({"manifest": manifest("other"), "provenance": unattested()}),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "a manifest for another project");
    let (s, _) = h
        .post(
            &fill(routes::RELEASES, &[("p", "shop")]),
            Some(&ana.token),
            json!({"manifest": {"api_version": 1}, "provenance": unattested()}),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "structurally invalid JSON body");
    assert!(h.get_ok(&fill(routes::LEDGER, &[("p", "shop")]), &ana.token).await.as_array().unwrap().is_empty());
    assert!(h.builder.jobs().is_empty());

    let check = h.ok(routes::MANIFEST_CHECK, &ana.token, json!({"manifest": bad})).await;
    assert!(!check["refusals"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn failed_builds_are_reported_and_never_activate() {
    let h = H::new().await;
    let (ana, _) = h.team("shop").await;
    h.builder.push(BuildResult::Failed { reason: "Dockerfile: no such file".into(), log: "step 1".into() });
    let r = h.propose("shop", &ana.token, manifest("shop"), unattested()).await;
    assert_eq!(r["status"], "build_failed");
    assert!(h.runtime.deployments().is_empty());
    let (s, _) = h.promote("shop", &ana.token, id(&r), Stage::Preview).await;
    assert_eq!(s, StatusCode::CONFLICT, "not built");
}

#[tokio::test]
async fn attested_provenance_needs_a_verified_upload_and_tampered_logs_are_refused() {
    let h = H::new().await;
    let (ana, _) = h.team("shop").await;
    let log = session_log(&["drafted the manifest", "proposed"]);
    let (s, up) = upload_session(&h, "shop", &ana.token, "0f1e2d", &log).await;
    assert_eq!(s, StatusCode::OK, "{up}");
    assert_eq!(up["entries"], 2);
    let head = up["head_hash"].as_str().unwrap().to_owned();

    let attested = json!({"kind": "attested", "session_id": "0f1e2d", "head_hash": head});
    let r = h.propose("shop", &ana.token, manifest("shop"), attested).await;
    assert_eq!(r["provenance"]["kind"], "attested");

    // Editing a byte of an entry breaks its hash; the upload is refused and cannot be cited.
    let tampered = log.replacen("drafted the manifest", "drafted the manifesto", 1);
    let (s, e) = upload_session(&h, "shop", &ana.token, "0f1e2d", &tampered).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::BAD_REQUEST, Some("invalid")), "{e}");
    // Dropping the first line breaks the linkage of the rest.
    let truncated: String = log.lines().skip(1).map(|l| format!("{l}\n")).collect();
    assert_eq!(upload_session(&h, "shop", &ana.token, "0f1e2d", &truncated).await.0, StatusCode::BAD_REQUEST);
    for provenance in [
        json!({"kind": "attested", "session_id": "0f1e2d", "head_hash": "0".repeat(64)}),
        json!({"kind": "attested", "session_id": "another", "head_hash": head}),
    ] {
        let (s, e) = h
            .post(
                &fill(routes::RELEASES, &[("p", "shop")]),
                Some(&ana.token),
                json!({"manifest": manifest("shop"), "provenance": provenance}),
            )
            .await;
        assert_eq!((s, e["code"].as_str()), (StatusCode::BAD_REQUEST, Some("invalid")));
    }
    // Uploaded to another project does not count here.
    h.ok(&fill(routes::ORG_PROJECTS, &[("org", "acme")]), &ana.token, json!({"slug": "blog"})).await;
    let (s, up2) = upload_session(&h, "blog", &ana.token, "abc", &session_log(&["x"])).await;
    assert_eq!(s, StatusCode::OK);
    let cross = json!({"kind": "attested", "session_id": "abc", "head_hash": up2["head_hash"]});
    let (s, _) = h
        .post(&fill(routes::RELEASES, &[("p", "shop")]), Some(&ana.token), json!({"manifest": manifest("shop"), "provenance": cross}))
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(upload_session(&h, "shop", &ana.token, "../etc", &log).await.0, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn archives_are_content_addressed_and_the_builder_fetches_them_with_the_service_token() {
    let h = H::new().await;
    let (ana, _) = h.team("shop").await;
    let bytes = b"\x1f\x8b fake tarball".to_vec();
    let (s, up) =
        h.send(Method::POST, &fill(routes::ARCHIVES, &[("p", "shop")]), Some(&ana.token), Some("application/gzip"), bytes.clone()).await;
    assert_eq!(s, StatusCode::OK, "{up}");
    let hash = up["hash"].as_str().unwrap().to_owned();
    assert_eq!(hash, format!("blake3:{}", blake3::hash(&bytes).to_hex()));
    let mut m = manifest("shop");
    m["source"] = json!({"kind": "archive", "hash": hash});
    m["build"] = json!({"kind": "dockerfile"});
    h.propose("shop", &ana.token, m.clone(), unattested()).await;
    let url = h.builder.jobs()[0].archive_url.clone().unwrap();
    let path = url.strip_prefix("http://control.test").unwrap();
    let (s, _) = h.send(Method::GET, path, Some(&ana.token), None, Vec::new()).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "customer tokens cannot use internal routes");
    let resp = h.send(Method::GET, path, Some(SERVICE), None, Vec::new()).await;
    assert_eq!(resp.0, StatusCode::OK);

    m["source"]["hash"] = json!(format!("blake3:{}", "c".repeat(64)));
    let (s, _) =
        h.post(&fill(routes::RELEASES, &[("p", "shop")]), Some(&ana.token), json!({"manifest": m, "provenance": unattested()})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "archive never uploaded");
}

#[tokio::test]
async fn a_leaked_secret_taints_the_release_until_rotated() {
    let h = H::new().await;
    let (ana, _) = h.team("shop").await;
    let leaked = "sk_live_leaked_4242424242";
    h.set_secret("shop", &ana.token, "STRIPE_KEY", Stage::Live, leaked).await;
    let encoded = base64::engine::general_purpose::STANDARD.encode(format!("Authorization: Basic {leaked}:"));
    let (_, up) = upload_session(&h, "shop", &ana.token, "sess1", &session_log(&[&format!("curl -H '{encoded}'"), "hello"])).await;

    let r = h.propose("shop", &ana.token, manifest("shop"), unattested()).await;
    assert_eq!(r["status"], "tainted", "{r}");
    let taint = r["taint"].as_array().unwrap();
    assert_eq!(taint.len(), 1);
    assert_eq!(taint[0]["secret"], "STRIPE_KEY");
    assert!(taint[0]["location"].as_str().unwrap().starts_with("aho-log:sess1#"));
    assert_ne!(
        taint[0]["location"],
        format!("aho-log:sess1#{}", up["head_hash"].as_str().unwrap()),
        "points at the entry that carried it, not the head"
    );
    assert!(h.runtime.deployments().is_empty(), "a tainted release is never deployed");
    let (s, e) = h.promote("shop", &ana.token, id(&r), Stage::Preview).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::CONFLICT, Some("tainted")));

    // Re-setting the same value is not a rotation.
    h.set_secret("shop", &ana.token, "STRIPE_KEY", Stage::Live, leaked).await;
    assert_eq!(h.promote("shop", &ana.token, id(&r), Stage::Preview).await.0, StatusCode::CONFLICT);
    // A new value is.
    h.set_secret("shop", &ana.token, "STRIPE_KEY", Stage::Live, "sk_live_rotated_777777").await;
    let (s, v) = h.promote("shop", &ana.token, id(&r), Stage::Preview).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["status"], "active");
    assert!(v["taint"].as_array().unwrap().is_empty());
    let kinds: Vec<String> = h
        .get_ok(&fill(routes::LEDGER, &[("p", "shop")]), &ana.token)
        .await
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["body"]["kind"].as_str().unwrap().to_owned())
        .collect();
    assert!(kinds.contains(&"secret_rotated".to_owned()) && kinds.contains(&"taint_checked".to_owned()), "{kinds:?}");
}

/// Sets the org policy to one approval for any release (owner still required for auth and
/// domain changes), signed by the owner.
async fn one_approval_policy(h: &H, ana: &Person) {
    let policy = ReleasePolicy { non_widening: 1, widening: 1, owner_only: [Category::Domain, Category::Auth].into() };
    let proof = ana.device_proof(&policy_message("acme", &policy));
    let (s, v) = h
        .req(
            Method::PUT,
            &fill(routes::ORG_POLICY, &[("org", "acme")]),
            Some(&ana.device().token),
            Some(json!({"policy": policy, "proof": proof})),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["policy"]["widening"], 1);
}

/// r1 live, then r2 (adds an egress host) live.
async fn two_live_releases(h: &H, ana: &Person, bob: &Person) -> (Value, Value) {
    h.set_secret("shop", &ana.token, "STRIPE_KEY", Stage::Live, "sk_live_value_000001").await;
    let r1 = h.propose("shop", &ana.token, manifest("shop"), unattested()).await;
    h.take_live("shop", &[ana], &r1).await;
    let mut m2 = manifest("shop");
    m2["egress"] = json!(["api.stripe.com", "api.openai.com"]);
    let r2 = h.propose("shop", &ana.token, m2, unattested()).await;
    h.take_live("shop", &[bob], &r2).await;
    assert_eq!(h.runtime.running("shop", Stage::Live).as_deref(), Some(id(&r2)));
    (r1, r2)
}

#[tokio::test]
async fn rollback_is_free_when_narrowing_and_signed_when_widening() {
    let h = H::new().await;
    let (ana, bob) = h.team("shop").await;
    let (s, _) = h
        .req(
            Method::PUT,
            &fill(routes::ORG_POLICY, &[("org", "acme")]),
            Some(&bob.device().token),
            Some(
                json!({"policy": ReleasePolicy::default(), "proof": bob.device_proof(&policy_message("acme", &ReleasePolicy::default()))}),
            ),
        )
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "members cannot change the policy");
    one_approval_policy(&h, &ana).await;
    let (r1, r2) = two_live_releases(&h, &ana, &bob).await;
    let path = fill(routes::ROLLBACK, &[("p", "shop")]);

    // Back to r1: it fits within r2, so no proof is needed.
    let (s, v) = h.post(&path, Some(&bob.token), json!({"release": id(&r1)})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(h.runtime.running("shop", Stage::Live).as_deref(), Some(id(&r1)));
    assert_eq!(h.runtime.deployments().last().unwrap().secrets["STRIPE_KEY"], "sk_live_value_000001");

    // Forward to r2 widens the live surface again: refused without a signed approval.
    let (s, e) = h.post(&path, Some(&bob.token), json!({"release": id(&r2)})).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::CONFLICT, Some("quorum_not_met")));
    let wrong = bob.device_proof(&approval_message("shop", id(&r1), r2["manifest_hash"].as_str().unwrap(), Stage::Live));
    let (s, _) = h.post(&path, Some(&bob.device().token), json!({"release": id(&r2), "proof": wrong})).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "proof over another release");
    let proof = bob.device_proof(&approval_message("shop", id(&r2), r2["manifest_hash"].as_str().unwrap(), Stage::Live));
    let (s, v) = h.post(&path, Some(&bob.device().token), json!({"release": id(&r2), "proof": proof})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(h.runtime.running("shop", Stage::Live).as_deref(), Some(id(&r2)));

    // A release that never went live is no rollback target.
    let r3 = h.propose("shop", &ana.token, manifest("shop"), unattested()).await;
    let (s, _) = h.post(&path, Some(&ana.token), json!({"release": id(&r3)})).await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(h.post(&path, Some(&ana.token), json!({"release": "r-missing"})).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn standing_orders_let_on_call_roll_back_within_the_ceiling() {
    let h = H::new().await;
    let (ana, bob) = h.team("shop").await;
    one_approval_policy(&h, &ana).await;
    let (r1, r2) = two_live_releases(&h, &ana, &bob).await;
    let oncall = fill(routes::ONCALL, &[("p", "shop")]);
    let rollback_r1 = json!({"action": {"kind": "rollback", "fits_within_ceiling": true}, "release": id(&r1)});

    let (s, _) = h.post(&oncall, Some(SERVICE), rollback_r1.clone()).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "nothing granted yet");

    let orders = vec![StandingOrder::RollbackWithinCeiling, StandingOrder::Restart];
    let grant_path = fill(routes::STANDING_ORDERS, &[("p", "shop")]);
    let (s, _) = h
        .post(
            &grant_path,
            Some(&bob.device().token),
            json!({"orders": orders, "ttl_secs": 3600, "proof": bob.device_proof(&standing_orders_message("shop", &orders))}),
        )
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "only owners grant standing orders");
    let (s, _) = h.post(&grant_path, Some(&ana.device().token), json!({"orders": orders, "ttl_secs": 3600, "proof": ana.device_proof(&standing_orders_message("shop", &[StandingOrder::Restart]))})).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "signature over other orders");
    let (s, v) = h
        .post(
            &grant_path,
            Some(&ana.device().token),
            json!({"orders": orders, "ttl_secs": 3600, "proof": ana.device_proof(&standing_orders_message("shop", &orders))}),
        )
        .await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{v}");

    // A member who is not an owner is not on call.
    assert_eq!(h.post(&oncall, Some(&bob.token), rollback_r1.clone()).await.0, StatusCode::FORBIDDEN);
    let (s, v) = h.post(&oncall, Some(SERVICE), rollback_r1).await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{v}");
    assert_eq!(h.runtime.running("shop", Stage::Live).as_deref(), Some(id(&r1)));

    // Rolling "back" to the wider r2 does not fit, whatever the caller claims.
    let (s, _) =
        h.post(&oncall, Some(SERVICE), json!({"action": {"kind": "rollback", "fits_within_ceiling": true}, "release": id(&r2)})).await;
    assert!(matches!(s, StatusCode::FORBIDDEN | StatusCode::CONFLICT), "{s}");
    assert_eq!(h.runtime.running("shop", Stage::Live).as_deref(), Some(id(&r1)));

    // Granted verbs run; others do not; the owner may act too.
    assert_eq!(h.post(&oncall, Some(&ana.token), json!({"action": {"kind": "restart"}})).await.0, StatusCode::NO_CONTENT);
    assert_eq!(h.runtime.calls(), vec![RuntimeCall::Restart { project: "shop".into(), stage: Stage::Live }]);
    assert_eq!(h.post(&oncall, Some(SERVICE), json!({"action": {"kind": "scale", "replicas": 3}})).await.0, StatusCode::FORBIDDEN);

    // Grants expire.
    h.clock.advance(3601);
    assert_eq!(h.post(&oncall, Some(SERVICE), json!({"action": {"kind": "restart"}})).await.0, StatusCode::FORBIDDEN);
    assert_eq!(h.runtime.calls().len(), 1);
}

#[tokio::test]
async fn secret_values_never_appear_in_any_response() {
    let h = H::new().await;
    let (ana, _) = h.team("shop").await;
    let live = "sv-live-9f8e7d6c5b4a3921";
    let staging = "sv-staging-0a1b2c3d4e5f";
    let set = h.set_secret("shop", &ana.token, "STRIPE_KEY", Stage::Live, live).await;
    assert_eq!(set["stages"], json!(["live"]));
    assert!(set["fingerprint"].as_str().unwrap().starts_with("hmac-sha256:"));
    let set = h.set_secret("shop", &ana.token, "STRIPE_KEY", Stage::Staging, staging).await;
    assert_eq!(set["stages"], json!(["staging", "live"]));
    let r = h.propose("shop", &ana.token, manifest("shop"), unattested()).await;

    let paths = [
        routes::ME.to_owned(),
        routes::ORGS.to_owned(),
        fill(routes::ORG_MEMBERS, &[("org", "acme")]),
        fill(routes::ORG_POLICY, &[("org", "acme")]),
        fill(routes::ORG_PROJECTS, &[("org", "acme")]),
        fill(routes::PROJECT, &[("p", "shop")]),
        fill(routes::RELEASES, &[("p", "shop")]),
        fill(routes::RELEASE, &[("p", "shop"), ("r", id(&r))]),
        fill(routes::DOMAINS, &[("p", "shop")]),
        fill(routes::LEDGER, &[("p", "shop")]),
        fill(routes::SECRETS, &[("p", "shop")]),
    ];
    let b64 = |v: &str| base64::engine::general_purpose::STANDARD_NO_PAD.encode(v);
    for p in paths {
        let body = h.get_ok(&p, &ana.token).await.to_string();
        for v in [live, staging] {
            assert!(!body.contains(v) && !body.contains(&b64(v)), "{p} leaked a secret value: {body}");
        }
    }
    // At rest: sealed.
    for s in h.state.store.secrets("shop").await.unwrap() {
        assert!(!s.ciphertext.windows(live.len()).any(|w| w == live.as_bytes()));
    }
    // And non-live deployments never carry them.
    for d in h.runtime.deployments() {
        assert!(d.secrets.values().all(|v| v.starts_with("aho-cloud-placeholder:")), "{:?}", d.stage);
    }
}
