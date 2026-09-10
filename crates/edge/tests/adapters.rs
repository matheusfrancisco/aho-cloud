//! The edge's adapters to other systems, against in-process fakes: the control plane's
//! `SITES` feed (`PollingDirectory`) and an SMTP server (`SmtpMailer`).

// Test code: helpers shared across tests panic on unexpected results by design.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Json, Router};
use cloud_api::internal::{Site, SiteState, Sites, routes};
use cloud_contract::manifest::Auth;
use cloud_edge::{Directory, DirectoryError, MagicLinkMail, Mailer, PollingDirectory, SmtpMailer};
use parking_lot::Mutex;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[derive(Default)]
struct FakeControlPlane {
    /// Full snapshot served without `since`.
    full: Option<Sites>,
    /// Delta served with `since`.
    delta: Option<Sites>,
    /// `since` values seen, in order (`None` = full fetch).
    seen: Vec<Option<u64>>,
}

type Cp = Arc<Mutex<FakeControlPlane>>;

async fn sites(State(cp): State<Cp>, headers: HeaderMap, Query(q): Query<std::collections::HashMap<String, String>>) -> Response {
    if headers.get("authorization").and_then(|v| v.to_str().ok()) != Some(&format!("Bearer {}", common::SERVICE_TOKEN)) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let since = q.get("since").map(|s| s.parse::<u64>().unwrap());
    let mut cp = cp.lock();
    cp.seen.push(since);
    let body = if since.is_some() { cp.delta.clone() } else { cp.full.clone() };
    Json(body.unwrap()).into_response()
}

fn site(h: &str, state: SiteState, upstream: &str) -> Site {
    Site {
        hostname: h.to_owned(),
        project: h.split('.').next().unwrap().to_owned(),
        stage: cloud_contract::ledger::Stage::Live,
        state,
        upstreams: vec![upstream.to_owned()],
        edge: Default::default(),
        auth: Auth::None,
        password_hash: None,
        oidc_client_secret: None,
        verifier_secrets: Default::default(),
        service_token_hashes: vec![],
        allowed_emails: vec![],
    }
}

#[tokio::test]
async fn polling_directory_applies_full_delta_and_reset() {
    let cp: Cp = Arc::default();
    let app = Router::new().route(routes::SITES, axum::routing::get(sites)).with_state(cp.clone());
    let (l, addr) = common::listener().await;
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    let base = format!("http://{addr}");

    cp.lock().full = Some(Sites {
        version: 5,
        sites: vec![site("a.test", SiteState::Active, "10.0.0.1:80"), site("B.Test", SiteState::Active, "10.0.0.2:80")],
    });
    let dir = PollingDirectory::new(&base, common::SERVICE_TOKEN).unwrap();
    assert!(dir.site("a.test").is_none());
    assert_eq!(dir.poll().await.unwrap(), 5);
    let a_before = dir.site("a.test").unwrap();
    assert_eq!(dir.site("b.test").unwrap().upstreams, vec!["10.0.0.2:80"], "hostnames are keyed lowercase");

    // Delta: b is tombstoned, c appears, a is untouched.
    cp.lock().delta = Some(Sites {
        version: 7,
        sites: vec![site("b.test", SiteState::Tombstoned, "10.0.0.2:80"), site("c.test", SiteState::Active, "10.0.0.3:80")],
    });
    assert_eq!(dir.poll().await.unwrap(), 7);
    assert!(Arc::ptr_eq(&a_before, &dir.site("a.test").unwrap()), "unchanged sites keep their identity");
    assert_eq!(dir.site("b.test").unwrap().state, SiteState::Tombstoned);
    assert!(dir.site("c.test").is_some());

    // The control plane went backwards (restored): refetch everything and replace.
    cp.lock().delta = Some(Sites { version: 2, sites: vec![] });
    cp.lock().full = Some(Sites { version: 2, sites: vec![site("c.test", SiteState::Active, "10.0.0.9:80")] });
    assert_eq!(dir.poll().await.unwrap(), 2);
    assert!(dir.site("a.test").is_none() && dir.site("b.test").is_none());
    assert_eq!(dir.site("c.test").unwrap().upstreams, vec!["10.0.0.9:80"]);
    assert_eq!(cp.lock().seen, vec![None, Some(5), Some(7), None]);

    // A wrong token is an error and changes nothing.
    let bad = PollingDirectory::new(&base, "wrong").unwrap();
    assert!(matches!(bad.poll().await, Err(DirectoryError::Status(401))));
    assert!(bad.version().is_none());
    assert_eq!(dir.version(), Some(2));
}

/// Speaks just enough SMTP for one message; returns (rcpt, data) through the channel.
async fn fake_smtp() -> (std::net::SocketAddr, tokio::sync::oneshot::Receiver<(Vec<String>, String)>) {
    let (l, addr) = common::listener().await;
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let (sock, _) = l.accept().await.unwrap();
        let (r, mut w) = sock.into_split();
        let mut lines = BufReader::new(r).lines();
        w.write_all(b"220 fake ESMTP\r\n").await.unwrap();
        let (mut rcpt, mut data) = (Vec::new(), String::new());
        while let Some(line) = lines.next_line().await.unwrap() {
            let upper = line.to_ascii_uppercase();
            if upper.starts_with("EHLO") || upper.starts_with("HELO") {
                w.write_all(b"250-fake\r\n250 8BITMIME\r\n").await.unwrap();
            } else if upper.starts_with("MAIL FROM") {
                w.write_all(b"250 ok\r\n").await.unwrap();
            } else if upper.starts_with("RCPT TO") {
                rcpt.push(line.clone());
                w.write_all(b"250 ok\r\n").await.unwrap();
            } else if upper == "DATA" {
                w.write_all(b"354 go\r\n").await.unwrap();
                while let Some(l) = lines.next_line().await.unwrap() {
                    if l == "." {
                        break;
                    }
                    data.push_str(&l);
                    data.push('\n');
                }
                w.write_all(b"250 queued\r\n").await.unwrap();
                let _ = tx.send((rcpt, data));
                break;
            } else {
                w.write_all(b"250 ok\r\n").await.unwrap();
            }
        }
    });
    (addr, rx)
}

#[tokio::test]
async fn smtp_mailer_delivers_the_link() {
    let (addr, rx) = fake_smtp().await;
    let mailer = SmtpMailer::from_url(&format!("smtp://127.0.0.1:{}", addr.port()), "aho cloud <login@aho.test>").unwrap();
    let mail = MagicLinkMail {
        to: "ann@acme.com".into(),
        host: "shop.acme.com".into(),
        link: "https://shop.acme.com/.edge/magic?token=abc".into(),
        expires_in_secs: 900,
    };
    mailer.send_magic_link(&mail).await.unwrap();
    let (rcpt, data) = rx.await.unwrap();
    assert_eq!(rcpt, vec!["RCPT TO:<ann@acme.com>"]);
    assert!(data.contains("Subject: Sign in to shop.acme.com"), "{data}");
    assert!(data.contains("https://shop.acme.com/.edge/magic?token=abc"), "{data}");
    assert!(data.contains("15 minutes"));

    assert!(SmtpMailer::from_url("smtp://127.0.0.1:1", "not an address").is_err());
}
