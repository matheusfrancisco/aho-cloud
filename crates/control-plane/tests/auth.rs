//! Device flow, passkeys, sessions, membership.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "test code: a failed unwrap is a failed test")]

mod common;

use axum::http::{Method, StatusCode};
use cloud_api::routes::{self, fill};
use cloud_contract::ledger::{Stage, approval_message};
use cloud_control_plane::crypto::b64url;
use cloud_control_plane::fakes::SoftAuthenticator;
use common::*;
use ed25519_dalek::SigningKey;
use serde_json::json;

#[tokio::test]
async fn device_login_binds_the_key_once_and_its_signature_approves() {
    let h = H::new().await;
    let (ana, bob) = h.team("shop").await;
    let key = SigningKey::from_bytes(&[9; 32]);
    let started = h
        .ok(routes::DEVICE_START, &ana.token, json!({"device_name": "ana@desk", "public_key": b64url(key.verifying_key().as_bytes())}))
        .await;
    assert_eq!(started["verification_uri"], "https://console.test/device");

    // Pending until approved, and the code is single-use.
    let poll = json!({"device_code": started["device_code"]});
    assert_eq!(h.post(routes::DEVICE_POLL, None, poll.clone()).await.1["status"], "pending");
    let (s, e) = h.post(routes::DEVICE_APPROVE, None, json!({"user_code": started["user_code"]})).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::UNAUTHORIZED, Some("unauthorized")));
    let lowercase = started["user_code"].as_str().unwrap().to_lowercase().replace('-', " ");
    assert_eq!(h.post(routes::DEVICE_APPROVE, Some(&ana.token), json!({"user_code": lowercase})).await.0, StatusCode::NO_CONTENT);
    assert_eq!(h.post(routes::DEVICE_APPROVE, Some(&bob.token), json!({"user_code": started["user_code"]})).await.0, StatusCode::CONFLICT);
    let (_, approved) = h.post(routes::DEVICE_POLL, None, poll.clone()).await;
    assert_eq!(approved["status"], "approved");
    assert_eq!(approved["user"]["email"], "ana@acme.test");
    assert_eq!(h.post(routes::DEVICE_POLL, None, poll).await.1["status"], "expired", "redeemed codes cannot be redeemed again");
    let token = approved["token"].as_str().unwrap();
    assert_eq!(h.get_ok(routes::ME, token).await["orgs"], json!([{"org": "acme", "owner": true}]));

    // The bound key's signature over the approval message approves; other bytes, another
    // user's key, or a forged signature do not.
    let r = h.propose("shop", token, manifest("shop"), unattested()).await;
    let id = r["id"].as_str().unwrap();
    let path = fill(routes::APPROVE, &[("p", "shop"), ("r", id)]);
    let key_id = approved["key_id"].as_str().unwrap();
    let hash = r["manifest_hash"].as_str().unwrap();
    let sign = |msg: &[u8]| {
        use ed25519_dalek::Signer as _;
        b64url(&key.sign(msg).to_bytes())
    };
    let wrong_stage = sign(&approval_message("shop", id, hash, Stage::Live));
    let (s, e) = h
        .post(
            &path,
            Some(token),
            json!({"stage": "staging", "proof": {"method": "device_key", "key_id": key_id, "signature": wrong_stage}}),
        )
        .await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::FORBIDDEN, Some("forbidden")));
    let good = sign(&approval_message("shop", id, hash, Stage::Staging));
    let (s, _) = h
        .post(&path, Some(&bob.token), json!({"stage": "staging", "proof": {"method": "device_key", "key_id": key_id, "signature": good}}))
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "bob cannot use ana's key");
    let (s, v) = h
        .post(&path, Some(token), json!({"stage": "staging", "proof": {"method": "device_key", "key_id": key_id, "signature": good}}))
        .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(
        v["approvals"],
        json!([{"stage": "staging", "user": {"id": ana.id, "email": "ana@acme.test", "name": "Ana"}, "owner": true}])
    );
}

#[tokio::test]
async fn device_codes_expire_and_bad_keys_are_refused() {
    let h = H::new().await;
    let ana = h.signup("ana@acme.test", "Ana").await;
    let (s, e) = h.post(routes::DEVICE_START, None, json!({"device_name": "x", "public_key": "not-a-key"})).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::BAD_REQUEST, Some("invalid")));
    let key = SigningKey::from_bytes(&[3; 32]);
    let started =
        h.ok(routes::DEVICE_START, &ana.token, json!({"device_name": "x", "public_key": b64url(key.verifying_key().as_bytes())})).await;
    h.clock.advance(601);
    assert_eq!(h.post(routes::DEVICE_APPROVE, Some(&ana.token), json!({"user_code": started["user_code"]})).await.0, StatusCode::NOT_FOUND);
    assert_eq!(h.post(routes::DEVICE_POLL, None, json!({"device_code": started["device_code"]})).await.1["status"], "expired");
}

#[tokio::test]
async fn passkeys_register_log_in_and_approve() {
    let h = H::new().await;
    let (mut ana, _bob) = h.team("shop").await;

    // Log in with the registered passkey; each ceremony answers once.
    let ch = h.ok(routes::PASSKEY_LOGIN_START, &ana.token, json!({"email": "ANA@acme.test"})).await;
    let assertion = ana.auth.assert(&ch["options"]);
    let finish = json!({"ceremony_id": ch["ceremony_id"], "credential": assertion});
    let (s, session) = h.post(routes::PASSKEY_LOGIN_FINISH, None, finish.clone()).await;
    assert_eq!(s, StatusCode::OK, "{session}");
    assert_eq!(h.get_ok(routes::ME, session["token"].as_str().unwrap()).await["user"]["id"], ana.id.as_str());
    assert_eq!(h.post(routes::PASSKEY_LOGIN_FINISH, None, finish).await.0, StatusCode::UNAUTHORIZED);

    // A phishing origin, or a stranger's authenticator, cannot log in.
    let ch = h.ok(routes::PASSKEY_LOGIN_START, &ana.token, json!({"email": "ana@acme.test"})).await;
    ana.auth.origin = "https://console.evil.test".into();
    let phished = ana.auth.assert(&ch["options"]);
    ana.auth.origin = ORIGIN.into();
    assert_eq!(
        h.post(routes::PASSKEY_LOGIN_FINISH, None, json!({"ceremony_id": ch["ceremony_id"], "credential": phished})).await.0,
        StatusCode::UNAUTHORIZED
    );
    let mut stranger = SoftAuthenticator::new(RP_ID, ORIGIN);
    let ch_s = h.ok(routes::PASSKEY_REGISTER_START, &ana.token, json!({"email": "eve@else.test", "name": "Eve"})).await;
    stranger.register(&ch_s["options"]);
    let ch = h.ok(routes::PASSKEY_LOGIN_START, &ana.token, json!({"email": "ana@acme.test"})).await;
    let foreign = stranger.assert(&json!({"publicKey": {"challenge": ch["options"]["publicKey"]["challenge"]}}));
    assert_eq!(
        h.post(routes::PASSKEY_LOGIN_FINISH, None, json!({"ceremony_id": ch["ceremony_id"], "credential": foreign})).await.0,
        StatusCode::UNAUTHORIZED
    );

    // An unknown email gets a challenge too, which cannot succeed.
    let ch = h.ok(routes::PASSKEY_LOGIN_START, &ana.token, json!({"email": "nobody@acme.test"})).await;
    let attempt = ana.auth.assert(&json!({"publicKey": {"challenge": ch["options"]["publicKey"]["challenge"]}}));
    assert_eq!(
        h.post(routes::PASSKEY_LOGIN_FINISH, None, json!({"ceremony_id": ch["ceremony_id"], "credential": attempt})).await.0,
        StatusCode::UNAUTHORIZED
    );

    // An existing email cannot be claimed by registering a new passkey without its session.
    let (s, e) = h.post(routes::PASSKEY_REGISTER_START, None, json!({"email": "ana@acme.test", "name": "Mallory"})).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::CONFLICT, Some("conflict")));
    let (s, _) = h.post(routes::PASSKEY_REGISTER_START, Some(&ana.token), json!({"email": "ana@acme.test", "name": "Ana"})).await;
    assert_eq!(s, StatusCode::OK, "the account owner may add a passkey");

    // Passkey approval: challenge = SHA-256(approval message); a replayed assertion fails.
    let r = h.propose("shop", &ana.token, manifest("shop"), unattested()).await;
    let id = r["id"].as_str().unwrap();
    let msg = approval_message("shop", id, r["manifest_hash"].as_str().unwrap(), Stage::Staging);
    let proof = ana.passkey_proof(&msg);
    let path = fill(routes::APPROVE, &[("p", "shop"), ("r", id)]);
    let (s, v) = h.post(&path, Some(&ana.token), json!({"stage": "staging", "proof": proof.clone()})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["approvals"][0]["user"]["id"], ana.id.as_str());
    let (s, _) = h.post(&path, Some(&ana.token), json!({"stage": "staging", "proof": proof})).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "replayed assertion (counter did not rise)");
    let other_stage = ana.passkey_proof(&approval_message("shop", id, r["manifest_hash"].as_str().unwrap(), Stage::Live));
    let (s, _) = h.post(&path, Some(&ana.token), json!({"stage": "staging", "proof": other_stage})).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "assertion over another stage's message");
}

#[tokio::test]
async fn non_members_are_forbidden_and_anonymous_callers_unauthorized() {
    let h = H::new().await;
    let (ana, _bob) = h.team("shop").await;
    let r = h.propose("shop", &ana.token, manifest("shop"), unattested()).await;
    let id = r["id"].as_str().unwrap();
    let eve = h.signup("eve@else.test", "Eve").await;
    let paths = [
        (Method::GET, fill(routes::PROJECT, &[("p", "shop")])),
        (Method::GET, fill(routes::RELEASES, &[("p", "shop")])),
        (Method::GET, fill(routes::RELEASE, &[("p", "shop"), ("r", id)])),
        (Method::GET, fill(routes::SECRETS, &[("p", "shop")])),
        (Method::GET, fill(routes::LEDGER, &[("p", "shop")])),
        (Method::GET, fill(routes::DOMAINS, &[("p", "shop")])),
        (Method::GET, fill(routes::ORG_MEMBERS, &[("org", "acme")])),
        (Method::GET, fill(routes::ORG_PROJECTS, &[("org", "acme")])),
        (Method::DELETE, fill(routes::PROJECT, &[("p", "shop")])),
    ];
    for (m, p) in &paths {
        let (s, e) = h.req(m.clone(), p, Some(&eve.token), None).await;
        assert_eq!((s, e["code"].as_str()), (StatusCode::FORBIDDEN, Some("forbidden")), "{m} {p}");
        let (s, e) = h.req(m.clone(), p, None, None).await;
        assert_eq!((s, e["code"].as_str()), (StatusCode::UNAUTHORIZED, Some("unauthorized")), "{m} {p}");
    }
    let posts = [
        (fill(routes::RELEASES, &[("p", "shop")]), json!({"manifest": manifest("shop"), "provenance": unattested()})),
        (fill(routes::PROMOTE, &[("p", "shop"), ("r", id)]), json!({"stage": "preview"})),
        (fill(routes::REJECT, &[("p", "shop"), ("r", id)]), json!({"stage": "staging", "reason": "no"})),
        (fill(routes::DOMAINS, &[("p", "shop")]), json!({"hostname": "shop.example.com", "mode": "cname"})),
        (fill(routes::WORKSPACES, &[("p", "shop")]), json!({})),
        (fill(routes::ORG_MEMBERS, &[("org", "acme")]), json!({"email": "eve@else.test", "owner": true})),
    ];
    for (p, body) in posts {
        let (s, _) = h.post(&p, Some(&eve.token), body).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "POST {p}");
    }
    let (s, _) = h
        .req(
            Method::PUT,
            &fill(routes::SECRETS, &[("p", "shop")]),
            Some(&eve.token),
            Some(json!({"name": "K", "stage": "live", "value": "v"})),
        )
        .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = h.req(Method::GET, routes::ME, Some("ahs_forged"), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    // Sessions expire.
    h.clock.advance(31 * 24 * 3600);
    assert_eq!(h.req(Method::GET, routes::ME, Some(&ana.token), None).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn members_are_not_owners() {
    let h = H::new().await;
    let (ana, bob) = h.team("shop").await;
    let (s, _) =
        h.post(&fill(routes::ORG_MEMBERS, &[("org", "acme")]), Some(&bob.token), json!({"email": "ana@acme.test", "owner": false})).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "only owners manage members");
    let (s, _) = h.req(Method::DELETE, &fill(routes::PROJECT, &[("p", "shop")]), Some(&bob.token), None).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "only owners delete projects");
    let (s, e) =
        h.post(&fill(routes::ORG_MEMBERS, &[("org", "acme")]), Some(&ana.token), json!({"email": "ana@acme.test", "owner": false})).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::CONFLICT, Some("conflict")), "the last owner cannot demote themselves");
    let members = h.get_ok(&fill(routes::ORG_MEMBERS, &[("org", "acme")]), &bob.token).await;
    assert_eq!(members.as_array().unwrap().len(), 2);
}
