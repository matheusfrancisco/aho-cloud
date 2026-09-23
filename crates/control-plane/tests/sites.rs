//! Domains, the edge's site directory, observations, workspaces.
#![allow(clippy::unwrap_used, clippy::expect_used, reason = "test code: a failed unwrap is a failed test")]

mod common;

use std::net::IpAddr;

use axum::http::{Method, StatusCode};
use cloud_api::internal::{SiteState, Sites};
use cloud_api::routes::{self, fill};
use cloud_contract::ledger::Stage;
use common::*;
use serde_json::{Value, json};

/// Creates the records a DomainView asks for in the fake DNS.
fn publish(h: &H, view: &Value) {
    for r in view["records"].as_array().unwrap() {
        let (name, value) = (r["name"].as_str().unwrap(), r["value"].as_str().unwrap());
        match r["kind"].as_str().unwrap() {
            "TXT" => h.dns.set_txt(name, &[value]),
            "CNAME" => h.dns.set_cname(name, value),
            "A" | "AAAA" => {
                let ip: IpAddr = value.parse().unwrap();
                h.dns.set_addrs(name, &[ip]);
            }
            "NS" => h.dns.set_ns(name, &[value]),
            other => panic!("unexpected record kind {other}"),
        }
    }
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

fn hosts(s: &Sites) -> Vec<(&str, SiteState)> {
    let mut out: Vec<(&str, SiteState)> = s.sites.iter().map(|x| (x.hostname.as_str(), x.state)).collect();
    out.sort_by(|a, b| a.0.cmp(b.0));
    out
}

#[tokio::test]
async fn sites_serve_verified_declared_domains_only_and_tombstone_deleted_projects() {
    let h = H::new().await;
    let (ana, bob) = h.team("shop").await;
    let (s, _) = h.req(Method::GET, cloud_api::internal::routes::SITES, Some(&ana.token), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "the edge's directory needs the service token");

    let domains = fill(routes::DOMAINS, &[("p", "shop")]);
    let shop = h.ok(&domains, &ana.token, json!({"hostname": "shop.example.com", "mode": "cname"})).await;
    h.ok(&domains, &ana.token, json!({"hostname": "pending.example.com", "mode": "cname"})).await;
    let undeclared = h.ok(&domains, &ana.token, json!({"hostname": "extra.example.com", "mode": "cname"})).await;
    assert_eq!(shop["state"], "pending");
    assert!(shop["records"].as_array().unwrap().iter().any(|r| r["kind"] == "TXT" && r["name"] == "_aho-cloud.shop.example.com"));
    publish(&h, &shop);
    publish(&h, &undeclared);
    let verify = |host: &str| fill(routes::DOMAIN_VERIFY, &[("p", "shop"), ("host", host)]);
    assert_eq!(h.ok(&verify("shop.example.com"), &ana.token, json!(null)).await["state"], "verified");
    assert_eq!(h.ok(&verify("extra.example.com"), &ana.token, json!(null)).await["state"], "verified");
    assert_eq!(h.ok(&verify("pending.example.com"), &ana.token, json!(null)).await["state"], "pending");
    assert_eq!(h.post(&verify("nope.example.com"), Some(&ana.token), json!(null)).await.0, StatusCode::NOT_FOUND);
    let listed = h.get_ok(&domains, &bob.token).await;
    assert_eq!(listed.as_array().unwrap().len(), 3);

    // Another tenant cannot take a bound hostname.
    h.ok(&fill(routes::ORG_PROJECTS, &[("org", "acme")]), &ana.token, json!({"slug": "blog"})).await;
    let (s, e) =
        h.post(&fill(routes::DOMAINS, &[("p", "blog")]), Some(&ana.token), json!({"hostname": "shop.example.com", "mode": "cname"})).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::CONFLICT, Some("conflict")));

    // Live release declaring shop + pending (not extra).
    let mut m = manifest("shop");
    m["domains"] =
        json!([{"hostname": "shop.example.com", "verification": "cname"}, {"hostname": "pending.example.com", "verification": "cname"}]);
    let r = h.propose("shop", &ana.token, m, unattested()).await;
    h.set_secret("shop", &ana.token, "STRIPE_KEY", Stage::Live, "sk_live_site_value_01").await;
    h.take_live("shop", &[&ana, &bob], &r).await;

    let all = sites(&h, None).await;
    assert_eq!(
        hosts(&all),
        vec![("shop.example.com", SiteState::Active), ("shop.preview.aho.test", SiteState::Active)],
        "pending, undeclared and the unreleased blog are not served"
    );
    let live = all.sites.iter().find(|s| s.hostname == "shop.example.com").unwrap();
    assert_eq!((live.stage, live.project.as_str()), (Stage::Live, "shop"));
    assert_eq!(live.upstreams, vec!["10.0.0.1:8080".to_owned()]);
    assert!(live.allowed_emails.is_empty(), "live is public");
    let preview = all.sites.iter().find(|s| s.stage == Stage::Preview).unwrap();
    assert_eq!(preview.allowed_emails, vec!["ana@acme.test".to_owned(), "bob@acme.test".to_owned()], "preview is for the org's reviewers");

    // Unchanged → empty delta at the same version.
    let again = sites(&h, Some(all.version)).await;
    assert_eq!((again.version, again.sites.len()), (all.version, 0));

    // Deleting the project tombstones every hostname it held.
    let (s, _) = h.req(Method::DELETE, &fill(routes::PROJECT, &[("p", "shop")]), Some(&ana.token), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert!(h.runtime.calls().iter().any(|c| matches!(c, cloud_control_plane::fakes::RuntimeCall::Sleep { stage: Stage::Live, .. })));
    let delta = sites(&h, Some(all.version)).await;
    assert!(delta.version > all.version);
    let changed = hosts(&delta);
    for host in ["shop.example.com", "shop.preview.aho.test"] {
        assert!(changed.contains(&(host, SiteState::Tombstoned)), "{host} in {changed:?}");
    }
    let full = sites(&h, None).await;
    let tomb = full.sites.iter().find(|s| s.hostname == "shop.example.com").unwrap();
    assert_eq!((tomb.state, tomb.upstreams.len()), (SiteState::Tombstoned, 0));
    assert_eq!(h.get(&fill(routes::PROJECT, &[("p", "shop")]), &ana.token).await.0, StatusCode::NOT_FOUND);
    let (s, _) = h.post(&fill(routes::ORG_PROJECTS, &[("org", "acme")]), Some(&ana.token), json!({"slug": "shop"})).await;
    assert_eq!(s, StatusCode::CONFLICT, "a deleted project's slug is retired");
}

#[tokio::test]
async fn observations_report_undeclared_egress() {
    let h = H::new().await;
    let (ana, _) = h.team("shop").await;
    let r = h.propose("shop", &ana.token, manifest("shop"), unattested()).await;
    let obs = json!({"project": "shop", "release": r["id"], "listeners": [8080], "egress": ["api.stripe.com", "evil.example.net"]});
    assert_eq!(h.post(cloud_api::internal::routes::OBSERVATIONS, Some(&ana.token), obs.clone()).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(h.post(cloud_api::internal::routes::OBSERVATIONS, Some(SERVICE), obs).await.0, StatusCode::NO_CONTENT);
    let v = h.release("shop", &ana.token, r["id"].as_str().unwrap()).await;
    assert_eq!(v["undeclared_egress"], json!(["evil.example.net"]));
    let bad = json!({"project": "shop", "release": "r-nope", "listeners": [], "egress": []});
    assert_eq!(h.post(cloud_api::internal::routes::OBSERVATIONS, Some(SERVICE), bad).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn workspaces_start_through_the_session_host() {
    let h = H::new().await;
    let (ana, _) = h.team("shop").await;
    let path = fill(routes::WORKSPACES, &[("p", "shop")]);
    let (s, w) = h.req(Method::POST, &path, Some(&ana.token), None).await;
    assert_eq!(s, StatusCode::OK, "{w}");
    assert_eq!(w["rpc_url"], "wss://sessions.test/rpc/shop");
    assert!(w["session_id"].is_null(), "a new workspace has no session until its first hello_ok: {w}");
    let resumed = h.ok(&path, &ana.token, json!({"session_id": "abc123"})).await;
    assert_eq!(resumed["session_id"], "abc123");
    let reqs = h.host.requests();
    assert_eq!(
        (reqs[0].user_id.as_str(), reqs[0].session_id.as_deref(), reqs[1].session_id.as_deref()),
        (ana.id.as_str(), None, Some("abc123"))
    );
    assert_eq!(h.post(&path, Some(&ana.token), json!({"session_id": "bad id!"})).await.0, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn unknown_routes_and_methods_answer_api_errors() {
    let h = H::new().await;
    let (s, e) = h.req(Method::GET, "/v1/nope", None, None).await;
    assert_eq!((s, e["code"].as_str()), (StatusCode::NOT_FOUND, Some("not_found")));
    let (s, e) = h.req(Method::DELETE, routes::ME, None, None).await;
    assert!(s.is_client_error() && e["code"].is_string(), "{s} {e}");
}
