//! The edge as a reverse proxy: header hygiene, route modes, bearer tokens, webhook
//! verifiers, site states, the port-80 listener, WebSockets, upstream failover and TLS.

// Test code: helpers shared across tests panic on unexpected results by design.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use std::sync::Arc;

use cloud_api::internal::SiteState;
use cloud_contract::ledger::Stage;
use cloud_contract::manifest::Auth;
use cloud_edge::verify::verify_principal;
use cloud_edge::webhook::{github_signature, hmac_hex, stripe_signature};
use cloud_edge::{Edge, Listener, tls_server_config};
use common::{ACME_KEY_AUTH, ACME_TOKEN, Echo, Harness, SERVICE_TOKEN, location, unix_now};
use futures::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

const MODES: &str = r#"
default = "edge"

[[route]]
path = "/open/**"
mode = "passthrough"

[[route]]
path = "/pub/**"
mode = "public"

[[route]]
path = "/maybe/**"
mode = "optional"

[[route]]
path = "/api/**"
mode = "bearer"

[[route]]
path = "/ws"
mode = "bearer"

[[route]]
path = "/hooks/stripe"
mode = "public"
verify = { kind = "stripe", secret = "STRIPE_WEBHOOK_SECRET" }

[[route]]
path = "/hooks/github"
mode = "public"
verify = { kind = "github", secret = "GITHUB_WEBHOOK_SECRET" }

[[route]]
path = "/hooks/custom"
mode = "public"
verify = { kind = "hmac", header = "X-Signature", secret = "CUSTOM_SECRET" }
"#;

async fn harness() -> Harness {
    let h = Harness::start().await;
    let mut site = h.site("app.test", Auth::Password, MODES);
    site.password_hash = Some(common::password_hash("hunter2"));
    for (k, v) in [("STRIPE_WEBHOOK_SECRET", "whsec_test"), ("GITHUB_WEBHOOK_SECRET", "gh_secret"), ("CUSTOM_SECRET", "custom_secret")] {
        site.verifier_secrets.insert(k.into(), v.into());
    }
    h.add(site);
    h
}

fn forged(rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    rb.header("x-principal-jwt", "eyJhbGciOiJub25lIn0.eyJzdWIiOiJhZG1pbiJ9.")
        .header("x-edge-user", "admin")
        .header("X-Edge-Roles", "owner")
        .header("x-forwarded-for", "6.6.6.6")
        .header("x-forwarded-host", "evil.test")
        .header("x-forwarded-proto", "https")
        .header("forwarded", "for=6.6.6.6")
        .header("x-real-ip", "6.6.6.6")
        .header("cookie", "app=1; __Host-edge_sid=stolen")
}

fn assert_clean(e: &Echo) {
    for h in ["x-edge-user", "x-edge-roles", "forwarded", "x-real-ip"] {
        assert!(!e.has(h), "{h} leaked to the app");
    }
    assert_eq!(e.headers("x-forwarded-for"), vec!["127.0.0.1"]);
    assert_eq!(e.header("x-forwarded-host").as_deref(), Some("app.test"));
    assert_eq!(e.header("x-forwarded-proto").as_deref(), Some("http"));
    assert_eq!(e.header("host").as_deref(), Some("app.test"));
    assert_eq!(e.header("cookie").as_deref(), Some("app=1"), "edge cookies are edge credentials");
}

#[tokio::test]
async fn forged_principal_and_edge_headers_never_reach_the_app() {
    let h = harness().await;
    for path in ["/open/x", "/pub/x", "/maybe/x"] {
        let e = Echo::from(forged(h.get("app.test", path)).send().await.unwrap()).await;
        assert!(!e.has("x-principal-jwt"), "{path}: forged principal reached the app");
        assert_clean(&e);
    }
    // Where the edge does attach a principal, it is its own, never the client's.
    let resp = forged(h.get("app.test", "/api/x")).bearer_auth(SERVICE_TOKEN).send().await.unwrap();
    let e = Echo::from(resp).await;
    assert_clean(&e);
    let jwt = e.headers("x-principal-jwt");
    assert_eq!(jwt.len(), 1);
    let jwks = h.jwks("app.test").await;
    let claims = verify_principal(&jwt[0], &jwks, "app", Stage::Live, unix_now()).unwrap();
    assert!(claims.sub.starts_with("svc:"));
}

#[tokio::test]
async fn edge_mode_without_session_redirects_to_login_and_never_hits_the_app() {
    let h = harness().await;
    let resp = h.get("app.test", "/dash/board?tab=1&x=%2F").send().await.unwrap();
    assert_eq!(resp.status(), 302);
    assert_eq!(location(&resp), "/.edge/login?rd=%2Fdash%2Fboard%3Ftab%3D1%26x%3D%252F");
    // A forged session cookie is no session.
    let resp = h.get("app.test", "/dash").header("cookie", "__Host-edge_sid=forged").send().await.unwrap();
    assert_eq!(resp.status(), 302);
    assert_eq!(h.hits(), 0);
}

#[tokio::test]
async fn optional_mode_forwards_anonymous_and_attaches_principal_with_session() {
    let h = harness().await;
    let e = Echo::from(h.get("app.test", "/maybe/x").send().await.unwrap()).await;
    assert!(!e.has("x-principal-jwt"));
    let jar = common::password_login(&h, "app.test", "hunter2").await;
    let e = Echo::from(h.get("app.test", "/maybe/x").header("cookie", jar.header()).send().await.unwrap()).await;
    let jwks = h.jwks("app.test").await;
    let claims = verify_principal(&e.header("x-principal-jwt").unwrap(), &jwks, "app", Stage::Live, unix_now()).unwrap();
    assert_eq!(claims.amr, vec!["pwd"]);
}

#[tokio::test]
async fn bearer_accepts_known_tokens_only() {
    let h = harness().await;
    let resp = h.get("app.test", "/api/items").bearer_auth(SERVICE_TOKEN).send().await.unwrap();
    let e = Echo::from(resp).await;
    assert!(!e.has("authorization"), "the edge consumed the service token");
    let jwks = h.jwks("app.test").await;
    let claims = verify_principal(&e.header("x-principal-jwt").unwrap(), &jwks, "app", Stage::Live, unix_now()).unwrap();
    let hash = blake3::hash(SERVICE_TOKEN.as_bytes()).to_hex().to_string();
    assert_eq!(claims.sub, format!("svc:{}", &hash[..12]));
    assert_eq!(claims.amr, vec!["svc"]);

    for auth in [Some("Bearer wrong-token"), Some(&format!("Basic {SERVICE_TOKEN}") as &str), Some("Bearer "), None] {
        let mut rb = h.get("app.test", "/api/items");
        if let Some(a) = auth {
            rb = rb.header("authorization", a);
        }
        let resp = rb.send().await.unwrap();
        assert_eq!(resp.status(), 401, "{auth:?}");
        assert_eq!(resp.headers()["www-authenticate"], "Bearer");
    }
    // The hash itself is not a token.
    let resp = h.get("app.test", "/api/items").bearer_auth(&hash).send().await.unwrap();
    assert_eq!(resp.status(), 401);
    assert_eq!(h.hits(), 1);
}

#[tokio::test]
async fn stripe_signatures_good_bad_expired_and_oversized() {
    let h = harness().await;
    let body = r#"{"id":"evt_1","type":"invoice.paid"}"#;
    let post = |sig: String, body: String| h.post("app.test", "/hooks/stripe").header("stripe-signature", sig).body(body).send();
    let now = unix_now();
    let resp = post(stripe_signature("whsec_test", now, body.as_bytes()), body.into()).await.unwrap();
    let e = Echo::from(resp).await;
    assert_eq!(e.body(), body, "verified body reaches the app byte for byte");

    let wrong_secret = post(stripe_signature("whsec_other", now, body.as_bytes()), body.into()).await.unwrap();
    assert_eq!(wrong_secret.status(), 401);
    let tampered = post(stripe_signature("whsec_test", now, body.as_bytes()), body.replace("paid", "void")).await.unwrap();
    assert_eq!(tampered.status(), 401);
    let expired = post(stripe_signature("whsec_test", now - 301, body.as_bytes()), body.into()).await.unwrap();
    assert_eq!(expired.status(), 401);
    assert!(expired.text().await.unwrap().contains("tolerance"));
    let unsigned = h.post("app.test", "/hooks/stripe").body(body).send().await.unwrap();
    assert_eq!(unsigned.status(), 401);

    // Refused on the declared length before any body byte is read. Sent raw: an HTTP client still
    // uploading 1 MiB when the edge answers early may see a connection reset instead of the 413.
    let head = format!(
        "POST /hooks/stripe HTTP/1.1\r\nhost: app.test\r\nstripe-signature: {}\r\ncontent-length: {}\r\n\r\n",
        stripe_signature("whsec_test", now, b"x"),
        (1 << 20) + 1
    );
    assert_eq!(status_line(h.addr, &head).await, "HTTP/1.1 413 Payload Too Large");
    assert_eq!(h.hits(), 1, "only the good delivery reached the app");
}

/// Sends `head` on a fresh connection and returns the response's status line.
async fn status_line(addr: std::net::SocketAddr, head: &str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(head.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while !buf.ends_with(b"\r\n") && s.read(&mut byte).await.unwrap() == 1 {
        buf.push(byte[0]);
    }
    String::from_utf8_lossy(&buf).trim_end().to_owned()
}

#[tokio::test]
async fn github_and_hmac_signatures() {
    let h = harness().await;
    let body = r#"{"action":"opened"}"#;
    let gh = |sig: String| h.post("app.test", "/hooks/github").header("x-hub-signature-256", sig).body(body).send();
    assert_eq!(gh(github_signature("gh_secret", body.as_bytes())).await.unwrap().status(), 200);
    assert_eq!(gh(github_signature("nope", body.as_bytes())).await.unwrap().status(), 401);
    assert_eq!(gh(hmac_hex("gh_secret", body.as_bytes())).await.unwrap().status(), 401, "prefix required");

    let custom = |sig: String| h.post("app.test", "/hooks/custom").header("x-signature", sig).body(body).send();
    assert_eq!(custom(hmac_hex("custom_secret", body.as_bytes())).await.unwrap().status(), 200);
    assert_eq!(custom(hmac_hex("custom_secret", b"other")).await.unwrap().status(), 401);
    assert_eq!(custom("not-hex".into()).await.unwrap().status(), 401);
    assert_eq!(h.hits(), 2);
}

#[tokio::test]
async fn site_states_and_unknown_hosts_never_reach_an_app() {
    let h = harness().await;
    for (host, state) in [("gone.test", SiteState::Tombstoned), ("sleep.test", SiteState::Sleeping), ("maint.test", SiteState::Maintenance)]
    {
        let mut s = h.site(host, Auth::None, "default = \"passthrough\"");
        s.state = state;
        h.add(s);
    }
    let gone = h.get("gone.test", "/").send().await.unwrap();
    assert_eq!(gone.status(), 410);
    assert!(gone.text().await.unwrap().contains("gone"));
    let jwks = h.get("gone.test", "/.edge/jwks.json").send().await.unwrap();
    assert_eq!(jwks.status(), 410, "tombstoned hosts serve only the tombstone");

    let sleep = h.get("sleep.test", "/x").send().await.unwrap();
    assert_eq!(sleep.status(), 503);
    assert!(sleep.headers().contains_key("retry-after"));
    assert!(sleep.text().await.unwrap().contains("paused"));
    let maint = h.get("maint.test", "/x").send().await.unwrap();
    assert_eq!(maint.status(), 503);
    assert!(maint.text().await.unwrap().contains("maintenance"));

    assert_eq!(h.get("unknown.test", "/").send().await.unwrap().status(), 421);
    assert_eq!(h.get("APP.TEST.", "/open/x").send().await.unwrap().status(), 200, "host matching is case/dot-insensitive");
    assert_eq!(h.hits(), 1);
}

#[tokio::test]
async fn port_80_answers_acme_and_redirects_everything_else() {
    let h = harness().await;
    let mut gone = h.site("gone.test", Auth::None, "");
    gone.state = SiteState::Tombstoned;
    h.add(gone);

    let resp = h.get80("app.test", &format!("/.well-known/acme-challenge/{ACME_TOKEN}")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), ACME_KEY_AUTH);
    // Challenges are answered before the host lookup: certificates are issued before sites go live.
    let resp = h.get80("not-yet.test", &format!("/.well-known/acme-challenge/{ACME_TOKEN}")).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    for bad in ["nope", "..%2F..%2Fetc", ""] {
        let resp = h.get80("app.test", &format!("/.well-known/acme-challenge/{bad}")).send().await.unwrap();
        assert_eq!(resp.status(), 404, "{bad}");
    }

    let resp = h.get80("app.test", "/dash?x=1").send().await.unwrap();
    assert_eq!(resp.status(), 308);
    assert_eq!(location(&resp), "https://app.test/dash?x=1");
    assert_eq!(h.get80("unknown.test", "/").send().await.unwrap().status(), 421);
    assert_eq!(h.get80("gone.test", "/").send().await.unwrap().status(), 410);
    assert_eq!(h.hits(), 0);
}

/// Status code of a hand-written HTTP/1.1 request, so the target reaches the edge verbatim.
async fn raw_status(addr: std::net::SocketAddr, host: &str, target: &str) -> u16 {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    let req = format!("GET {target} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    tcp.write_all(req.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    tcp.read_to_end(&mut buf).await.unwrap();
    let head = String::from_utf8_lossy(&buf);
    head.split(' ').nth(1).unwrap().parse().unwrap()
}

#[tokio::test]
async fn path_tricks_do_not_escape_their_route() {
    let h = Harness::start().await;
    h.add(h.site("app.test", Auth::Password, "default = \"public\"\n[[route]]\npath = \"/admin/**\"\nmode = \"edge\"\n"));
    // reqwest (WHATWG URL parsing) would resolve dot segments itself; send the raw bytes.
    for trick in ["/%61dmin/x", "//admin/x", "/pub/../admin/x", "/pub/%2e%2e/admin/x", "/./admin/x", "/pub/%2E%2E/admin/y"] {
        assert_eq!(raw_status(h.addr, "app.test", trick).await, 302, "{trick} bypassed the edge route");
    }
    let e = Echo::from(h.get("app.test", "/pub/./a//b/%7Euser?q=%2F").send().await.unwrap()).await;
    assert_eq!(e.uri(), "/pub/a/b/~user?q=%2F", "the app sees the path the edge matched");
    assert_eq!(h.get("app.test", "/.edge/anything").send().await.unwrap().status(), 404);
    assert_eq!(h.hits(), 1);
}

#[tokio::test]
async fn websocket_upgrade_passes_through_with_principal() {
    let h = harness().await;
    let tcp = tokio::net::TcpStream::connect(h.addr).await.unwrap();
    let mut req = "ws://app.test/ws".into_client_request().unwrap();
    req.headers_mut().insert("authorization", format!("Bearer {SERVICE_TOKEN}").parse().unwrap());
    req.headers_mut().insert("x-principal-jwt", "forged".parse().unwrap());
    let (mut ws, resp) = tokio_tungstenite::client_async(req, tcp).await.unwrap();
    assert_eq!(resp.status(), 101);
    let hello = ws.next().await.unwrap().unwrap().into_text().unwrap();
    let hello: serde_json::Value = serde_json::from_str(&hello).unwrap();
    let jwt = hello["principal"].as_str().unwrap();
    let jwks = h.jwks("app.test").await;
    assert!(verify_principal(jwt, &jwks, "app", Stage::Live, unix_now()).is_ok(), "handshake carried the edge's principal");
    for msg in ["one", "two"] {
        ws.send(Message::text(msg)).await.unwrap();
        assert_eq!(ws.next().await.unwrap().unwrap().into_text().unwrap().as_str(), msg);
    }
    ws.close(None).await.unwrap();

    // Without the token the upgrade is refused at the edge.
    let tcp = tokio::net::TcpStream::connect(h.addr).await.unwrap();
    let Err(err) = tokio_tungstenite::client_async("ws://app.test/ws", tcp).await else {
        panic!("upgrade without a token must be refused");
    };
    assert!(err.to_string().contains("401"), "{err}");
}

#[tokio::test]
async fn upstream_failover_and_502_when_none_answer() {
    let h = Harness::start().await;
    let (dead, dead_addr) = common::listener().await;
    drop(dead);
    let mut site = h.site("app.test", Auth::None, "default = \"passthrough\"");
    site.upstreams = vec![dead_addr.to_string(), h.upstream.to_string()];
    h.add(site.clone());
    for _ in 0..4 {
        let resp = h.get("app.test", "/x").send().await.unwrap();
        assert_eq!(resp.status(), 200, "bodiless requests retry on the next replica");
    }
    // A streaming body cannot be replayed; with only a dead replica the answer is 502.
    site.upstreams = vec![dead_addr.to_string()];
    h.add(site.clone());
    assert_eq!(h.post("app.test", "/x").body("payload").send().await.unwrap().status(), 502);
    site.upstreams.clear();
    h.add(site);
    assert_eq!(h.get("app.test", "/x").send().await.unwrap().status(), 502);
}

#[derive(Debug)]
struct OneCert(Arc<rustls::sign::CertifiedKey>);

impl rustls::server::ResolvesServerCert for OneCert {
    fn resolve(&self, hello: rustls::server::ClientHello<'_>) -> Option<Arc<rustls::sign::CertifiedKey>> {
        (hello.server_name() == Some("app.test")).then(|| self.0.clone())
    }
}

#[tokio::test]
async fn tls_listener_serves_h2_by_sni_and_refuses_sni_host_mismatch() {
    let h = harness().await;
    let rcgen::CertifiedKey { cert, signing_key } = rcgen::generate_simple_self_signed(vec!["app.test".to_owned()]).unwrap();
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(signing_key.serialize_der().into());
    let signer = rustls::crypto::ring::sign::any_supported_type(&key).unwrap();
    let certified = Arc::new(rustls::sign::CertifiedKey::new(vec![cert.der().clone()], signer));
    let tls = tls_server_config(Arc::new(OneCert(certified))).unwrap();

    let (l, addr) = common::listener().await;
    let edge: Edge = h.edge.clone();
    let task = tokio::spawn(async move { edge.serve(l, Listener::Tls(tls)).await });

    let root = reqwest::Certificate::from_der(cert.der()).unwrap();
    let client = |h1: bool| {
        let mut b = reqwest::Client::builder().add_root_certificate(root.clone()).resolve("app.test", addr);
        if h1 {
            b = b.http1_only();
        }
        b.build().unwrap()
    };
    let url = format!("https://app.test:{}/open/x", addr.port());
    let resp = client(false).get(&url).header("x-principal-jwt", "forged").send().await.unwrap();
    assert_eq!(resp.version(), reqwest::Version::HTTP_2);
    let e = Echo::from(resp).await;
    assert_eq!(e.header("x-forwarded-proto").as_deref(), Some("https"));
    assert_eq!(e.header("host").as_deref(), Some(&*format!("app.test:{}", addr.port())));
    assert!(!e.has("x-principal-jwt"));

    let resp = client(true).get(&url).send().await.unwrap();
    assert_eq!(resp.version(), reqwest::Version::HTTP_11);
    assert_eq!(resp.status(), 200);
    let resp = client(true).get(&url).header("host", "other.test").send().await.unwrap();
    assert_eq!(resp.status(), 421, "SNI app.test, Host other.test");
    task.abort();
}
