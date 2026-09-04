//! RDAP expiry against an in-process server that behaves like the rdap.org bootstrap: it
//! redirects registered domains to the "registry" and 404s names nobody registered.
// Helpers outside `#[test]` fns are test code too.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::net::SocketAddr;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use axum::routing::get;
use cloud_domains::{Expiry, Rdap, RdapClient, RdapError};
use parking_lot::Mutex;

const FIXTURE: &str = include_str!("fixtures/rdap-acme.com.json");
/// `2027-01-22T05:00:00Z`, the fixture's expiration event.
const ACME_EXPIRES: u64 = 1_800_594_000;

#[derive(Clone, Default)]
struct Seen(Arc<Mutex<Vec<String>>>);

async fn bootstrap(State(seen): State<Seen>, Path(name): Path<String>) -> Response {
    seen.0.lock().push(name.clone());
    match name.as_str() {
        "acme.com" => Redirect::temporary("/registry/domain/acme.com").into_response(),
        "broken.com" => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        "noexpiry.com" => ([(header::CONTENT_TYPE, "application/rdap+json")], r#"{"events":[]}"#).into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn registry() -> Response {
    ([(header::CONTENT_TYPE, "application/rdap+json")], FIXTURE).into_response()
}

async fn start() -> (String, Seen) {
    let seen = Seen::default();
    let app =
        Router::new().route("/domain/{name}", get(bootstrap)).route("/registry/domain/acme.com", get(registry)).with_state(seen.clone());
    let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0))).await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}/"), seen)
}

#[tokio::test]
async fn expiry_walks_up_to_the_registered_domain_and_follows_the_bootstrap_redirect() {
    let (base, seen) = start().await;
    let rdap = RdapClient::new(&base);
    let got = rdap.expiry("API.Shop.acme.com.").await.unwrap();
    assert_eq!(got, Some(Expiry { domain: "acme.com".into(), expires_at: ACME_EXPIRES }));
    assert_eq!(*seen.0.lock(), vec!["api.shop.acme.com", "shop.acme.com", "acme.com"]);
}

#[tokio::test]
async fn unknown_names_are_none_and_never_query_a_bare_tld() {
    let (base, seen) = start().await;
    let rdap = RdapClient::new(&base);
    assert_eq!(rdap.expiry("www.unregistered.example").await.unwrap(), None);
    assert_eq!(*seen.0.lock(), vec!["www.unregistered.example", "unregistered.example"]);
    assert_eq!(rdap.expiry("noexpiry.com").await.unwrap(), None, "registered but no expiration event");
}

#[tokio::test]
async fn registry_errors_surface_instead_of_walking_past_them() {
    let (base, seen) = start().await;
    let rdap = RdapClient::new(&base);
    assert!(matches!(rdap.expiry("shop.broken.com").await, Err(RdapError::Http { domain, .. }) if domain == "broken.com"));
    assert_eq!(*seen.0.lock(), vec!["shop.broken.com", "broken.com"]);
}
