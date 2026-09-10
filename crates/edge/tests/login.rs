//! Edge login end to end: password, magic link, OIDC (custom issuer and GitHub), reviewer
//! allow-lists, `/.edge/me`, `/.edge/logout`, and the principal JWT the app receives.

// Test code: helpers shared across tests panic on unexpected results by design.
#![allow(clippy::unwrap_used, clippy::expect_used)]

mod common;

use cloud_contract::ledger::Stage;
use cloud_contract::manifest::{Auth, OidcProvider};
use cloud_contract::principal::{TTL_SECS, audience, issuer};
use cloud_edge::verify::verify_principal;
use cloud_edge::{ClientCredentials, GithubEndpoints, OidcSettings, Settings};
use common::idp::{CLIENT_ID, CLIENT_SECRET, FakeGithub, FakeIdp};
use common::{Echo, Harness, Jar, form_field, location, password_login, set_cookie, set_cookie_line, unix_now};

async fn password_site(h: &Harness, host: &str, stage: Stage) {
    let mut s = h.site(host, Auth::Password, "");
    s.stage = stage;
    s.password_hash = Some(common::password_hash("hunter2"));
    h.add(s);
}

#[tokio::test]
async fn password_login_yields_a_principal_verifiable_with_the_served_jwks() {
    let h = Harness::start().await;
    password_site(&h, "pw.test", Stage::Staging).await;

    let resp = h.get("pw.test", "/dash").send().await.unwrap();
    assert_eq!(location(&resp), "/.edge/login?rd=%2Fdash");
    let mut jar = Jar::default();
    let page = h.get("pw.test", "/.edge/login?rd=/dash").send().await.unwrap();
    assert_eq!(page.status(), 200);
    assert_eq!(page.headers()["x-frame-options"], "DENY");
    let csrf_line = set_cookie_line(&page, "__Host-edge_csrf").unwrap();
    assert!(csrf_line.contains("Secure") && csrf_line.contains("HttpOnly") && csrf_line.contains("SameSite=Strict"));
    jar.take(&page);
    let csrf = form_field(&page.text().await.unwrap(), "csrf");

    let wrong = h
        .post("pw.test", "/.edge/login")
        .header("cookie", jar.header())
        .form(&[("csrf", csrf.as_str()), ("rd", "/dash"), ("password", "hunter3")])
        .send()
        .await
        .unwrap();
    assert_eq!(wrong.status(), 401);
    assert!(set_cookie(&wrong, "__Host-edge_sid").is_none());

    let ok = h
        .post("pw.test", "/.edge/login")
        .header("cookie", jar.header())
        .form(&[("csrf", csrf.as_str()), ("rd", "/dash"), ("password", "hunter2")])
        .send()
        .await
        .unwrap();
    assert_eq!(ok.status(), 303);
    assert_eq!(location(&ok), "/dash");
    let sid_line = set_cookie_line(&ok, "__Host-edge_sid").unwrap();
    for attr in ["Path=/", "Secure", "HttpOnly", "SameSite=Lax"] {
        assert!(sid_line.contains(attr), "{sid_line} lacks {attr}");
    }
    jar.take(&ok);

    let resp = h.get("pw.test", "/dash").header("cookie", jar.header()).header("x-principal-jwt", "forged").send().await.unwrap();
    let e = Echo::from(resp).await;
    let tokens = e.headers("x-principal-jwt");
    assert_eq!(tokens.len(), 1, "exactly the edge's principal");
    let jwks = h.jwks("pw.test").await;
    let now = unix_now();
    let claims = verify_principal(&tokens[0], &jwks, "pw", Stage::Staging, now).unwrap();
    assert_eq!(claims.iss, issuer("pw"));
    assert_eq!(claims.aud, audience("pw", Stage::Staging));
    assert_eq!(claims.amr, vec!["pwd"]);
    assert_eq!(claims.exp - claims.iat, TTL_SECS);
    assert!(verify_principal(&tokens[0], &jwks, "pw", Stage::Live, now).is_err(), "audience pins the stage");
    assert!(!e.header("cookie").unwrap_or_default().contains("edge_sid"));

    let me = h.get("pw.test", "/.edge/me").header("cookie", jar.header()).send().await.unwrap();
    assert_eq!(me.status(), 200);
    let me: serde_json::Value = me.json().await.unwrap();
    assert_eq!(me["sub"], claims.sub);
    assert_eq!(me["sid"], claims.sid);

    let out = h.post("pw.test", "/.edge/logout").header("cookie", jar.header()).send().await.unwrap();
    assert_eq!(out.status(), 303);
    assert!(set_cookie_line(&out, "__Host-edge_sid").unwrap().contains("Max-Age=0"));
    // The old cookie is revoked server-side, not just cleared in the browser.
    let again = h.get("pw.test", "/dash").header("cookie", jar.header()).send().await.unwrap();
    assert_eq!(again.status(), 302);
    assert_eq!(h.get("pw.test", "/.edge/me").header("cookie", jar.header()).send().await.unwrap().status(), 401);
}

#[tokio::test]
async fn login_forms_refuse_missing_csrf_and_cross_site_posts() {
    let h = Harness::start().await;
    password_site(&h, "pw.test", Stage::Live).await;
    let page = h.get("pw.test", "/.edge/login").send().await.unwrap();
    let mut jar = Jar::default();
    jar.take(&page);
    let csrf = form_field(&page.text().await.unwrap(), "csrf");

    let no_cookie = h.post("pw.test", "/.edge/login").form(&[("csrf", csrf.as_str()), ("password", "hunter2")]);
    assert_eq!(no_cookie.send().await.unwrap().status(), 403);
    let wrong_token = h
        .post("pw.test", "/.edge/login")
        .header("cookie", jar.header())
        .form(&[("csrf", "x".repeat(43).as_str()), ("password", "hunter2")]);
    assert_eq!(wrong_token.send().await.unwrap().status(), 403);
    let cross_site = h
        .post("pw.test", "/.edge/login")
        .header("cookie", jar.header())
        .header("origin", "https://evil.test")
        .form(&[("csrf", csrf.as_str()), ("password", "hunter2")]);
    assert_eq!(cross_site.send().await.unwrap().status(), 403);
    // Open redirects are neutralized.
    let ok = h.post("pw.test", "/.edge/login").header("cookie", jar.header()).form(&[
        ("csrf", csrf.as_str()),
        ("password", "hunter2"),
        ("rd", "//evil.test/x"),
    ]);
    let ok = ok.send().await.unwrap();
    assert_eq!(ok.status(), 303);
    assert_eq!(location(&ok), "/");
}

#[tokio::test]
async fn sessions_are_bound_to_their_host() {
    let h = Harness::start().await;
    password_site(&h, "one.test", Stage::Live).await;
    password_site(&h, "two.test", Stage::Live).await;
    let jar = password_login(&h, "one.test", "hunter2").await;
    assert_eq!(h.get("one.test", "/").header("cookie", jar.header()).send().await.unwrap().status(), 200);
    let other = h.get("two.test", "/").header("cookie", jar.header()).send().await.unwrap();
    assert_eq!(other.status(), 302, "a session never crosses tenants");
    assert_eq!(h.get("two.test", "/.edge/me").header("cookie", jar.header()).send().await.unwrap().status(), 401);

    // Same hostname, re-bound to another project: the old session does not carry over.
    let mut moved = h.site("one.test", Auth::Password, "");
    moved.project = "newowner".into();
    moved.password_hash = Some(common::password_hash("hunter2"));
    h.add(moved);
    assert_eq!(h.get("one.test", "/").header("cookie", jar.header()).send().await.unwrap().status(), 302);
}

async fn request_magic_link(h: &Harness, host: &str, email: &str) -> reqwest::Response {
    let page = h.get(host, "/.edge/login?rd=/inbox").send().await.unwrap();
    let mut jar = Jar::default();
    jar.take(&page);
    let csrf = form_field(&page.text().await.unwrap(), "csrf");
    h.post(host, "/.edge/login")
        .header("cookie", jar.header())
        .form(&[("csrf", csrf.as_str()), ("rd", "/inbox"), ("email", email)])
        .send()
        .await
        .unwrap()
}

/// Opens the emailed link in a fresh browser and confirms it.
async fn use_magic_link(h: &Harness, host: &str, link: &str) -> (reqwest::Response, Jar) {
    let path = link.split_once(host).unwrap().1;
    let page = h.get(host, path).send().await.unwrap();
    assert_eq!(page.status(), 200);
    let mut jar = Jar::default();
    jar.take(&page);
    let html = page.text().await.unwrap();
    let (csrf, token) = (form_field(&html, "csrf"), form_field(&html, "token"));
    let resp = h
        .post(host, "/.edge/magic")
        .header("cookie", jar.header())
        .form(&[("csrf", csrf.as_str()), ("token", token.as_str())])
        .send()
        .await
        .unwrap();
    jar.take(&resp);
    (resp, jar)
}

#[tokio::test]
async fn magic_link_signs_in_once() {
    let h = Harness::start().await;
    h.add(h.site("ml.test", Auth::MagicLink { allowed_domains: vec!["acme.com".into()] }, ""));

    let refused = request_magic_link(&h, "ml.test", "eve@evil.com").await;
    assert_eq!(refused.status(), 403);
    assert!(h.mails.0.lock().is_empty(), "no mail for a disallowed domain");

    let sent = request_magic_link(&h, "ml.test", "  Ann@ACME.com ").await;
    assert_eq!(sent.status(), 200);
    let mail = h.mails.0.lock().pop().unwrap();
    assert_eq!(mail.to, "ann@acme.com");
    assert!(mail.link.starts_with("https://ml.test/.edge/magic?token="));
    assert!(mail.text().contains(&mail.link));

    // Opening the link (e.g. by a mail scanner) does not consume it; confirming does.
    let (resp, jar) = use_magic_link(&h, "ml.test", &mail.link).await;
    assert_eq!(resp.status(), 303);
    assert_eq!(location(&resp), "/inbox");
    let e = Echo::from(h.get("ml.test", "/inbox").header("cookie", jar.header()).send().await.unwrap()).await;
    let jwks = h.jwks("ml.test").await;
    let claims = verify_principal(&e.header("x-principal-jwt").unwrap(), &jwks, "ml", Stage::Live, unix_now()).unwrap();
    assert_eq!(claims.sub, "email:ann@acme.com");
    assert_eq!(claims.email.as_deref(), Some("ann@acme.com"));
    assert!(claims.email_verified);
    assert_eq!(claims.amr, vec!["otp"]);

    let (reuse, jar2) = use_magic_link(&h, "ml.test", &mail.link).await;
    assert_eq!(reuse.status(), 400, "a magic link works once");
    assert!(jar2.get("__Host-edge_sid").is_none());
}

#[tokio::test]
async fn preview_allowed_emails_are_enforced() {
    let h = Harness::start().await;
    let mut s = h.site("rev.test", Auth::MagicLink { allowed_domains: vec![] }, "");
    s.stage = Stage::Preview;
    s.allowed_emails = vec!["rev@acme.com".into()];
    h.add(s.clone());

    assert_eq!(request_magic_link(&h, "rev.test", "stranger@acme.com").await.status(), 403);
    assert!(h.mails.0.lock().is_empty());
    assert_eq!(request_magic_link(&h, "rev.test", "rev@acme.com").await.status(), 200);
    let link = h.mails.0.lock().pop().unwrap().link;
    let (resp, jar) = use_magic_link(&h, "rev.test", &link).await;
    assert_eq!(resp.status(), 303);
    assert_eq!(h.get("rev.test", "/").header("cookie", jar.header()).send().await.unwrap().status(), 200);

    // The list is checked per request: dropping the reviewer locks them out immediately.
    s.allowed_emails = vec!["someone-else@acme.com".into()];
    h.add(s);
    assert_eq!(h.get("rev.test", "/").header("cookie", jar.header()).send().await.unwrap().status(), 403);

    // A shared password carries no email, so it cannot satisfy a reviewer list.
    let mut pw = h.site("pwrev.test", Auth::Password, "");
    pw.stage = Stage::Staging;
    pw.password_hash = Some(common::password_hash("hunter2"));
    pw.allowed_emails = vec!["rev@acme.com".into()];
    h.add(pw);
    let page = h.get("pwrev.test", "/.edge/login").send().await.unwrap();
    let mut jar = Jar::default();
    jar.take(&page);
    let csrf = form_field(&page.text().await.unwrap(), "csrf");
    let resp = h
        .post("pwrev.test", "/.edge/login")
        .header("cookie", jar.header())
        .form(&[("csrf", csrf.as_str()), ("password", "hunter2")])
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 403);
    assert!(set_cookie(&resp, "__Host-edge_sid").is_none());
}

/// Starts an OIDC login at the edge and lets the IdP "consent"; returns the callback path,
/// the browser jar and the authorize URL the edge built.
async fn oidc_to_callback(h: &Harness, host: &str) -> (String, Jar, url::Url) {
    let start = h.get(host, "/.edge/login?rd=/home").send().await.unwrap();
    assert_eq!(start.status(), 302);
    let mut jar = Jar::default();
    jar.take(&start);
    let authorize = url::Url::parse(&location(&start)).unwrap();
    let consent = h.http.get(authorize.clone()).send().await.unwrap();
    assert_eq!(consent.status(), 303, "IdP accepted the authorization request");
    let back = location(&consent);
    let path = back.split_once(host).unwrap().1.to_owned();
    (path, jar, authorize)
}

fn param(url: &url::Url, name: &str) -> String {
    url.query_pairs().find(|(k, _)| k == name).map(|(_, v)| v.into_owned()).unwrap()
}

async fn oidc_harness() -> (Harness, FakeIdp) {
    let idp = FakeIdp::start().await;
    let h = Harness::start().await;
    let mut s =
        h.site("sso.test", Auth::Oidc { provider: OidcProvider::Custom { issuer: idp.issuer.clone(), client_id: CLIENT_ID.into() } }, "");
    s.oidc_client_secret = Some(CLIENT_SECRET.into());
    h.add(s);
    (h, idp)
}

#[tokio::test]
async fn oidc_login_with_pkce_state_and_nonce() {
    let (h, idp) = oidc_harness().await;
    let (callback, mut jar, authorize) = oidc_to_callback(&h, "sso.test").await;
    assert_eq!(param(&authorize, "redirect_uri"), "https://sso.test/.edge/callback");
    assert_eq!(param(&authorize, "scope"), "openid email profile");
    assert_eq!(jar.get("__Host-edge_oidc"), Some(param(&authorize, "state").as_str()));

    let done = h.get("sso.test", &callback).header("cookie", jar.header()).send().await.unwrap();
    assert_eq!(done.status(), 303);
    assert_eq!(location(&done), "/home");
    jar.take(&done);
    assert!(jar.get("__Host-edge_oidc").is_none(), "state cookie cleared");
    let token_req = idp.state.lock().token_requests.pop().unwrap();
    assert_eq!(token_req["code_verifier"].len(), 43);

    let e = Echo::from(h.get("sso.test", "/home").header("cookie", jar.header()).send().await.unwrap()).await;
    let jwks = h.jwks("sso.test").await;
    let claims = verify_principal(&e.header("x-principal-jwt").unwrap(), &jwks, "sso", Stage::Live, unix_now()).unwrap();
    assert_eq!(claims.sub, "oidc:user-123");
    assert_eq!(claims.email.as_deref(), Some("ann@acme.com"));
    assert!(claims.email_verified);
    assert_eq!(claims.groups, vec!["eng"]);
    assert_eq!(claims.amr, vec!["oidc"]);

    // The completed state cannot be replayed.
    let replay = h.get("sso.test", &callback).header("cookie", "__Host-edge_oidc=".to_owned() + &param(&authorize, "state"));
    assert_eq!(replay.send().await.unwrap().status(), 400);
}

#[tokio::test]
async fn oidc_refuses_bad_state_and_bad_nonce() {
    let (h, idp) = oidc_harness().await;

    // The callback from a browser that did not start the login (login CSRF).
    let (callback, _jar, _) = oidc_to_callback(&h, "sso.test").await;
    let resp = h.get("sso.test", &callback).send().await.unwrap();
    assert_eq!(resp.status(), 400);
    // A state cookie that does not match the returned state.
    let (callback, _jar, _) = oidc_to_callback(&h, "sso.test").await;
    let resp = h.get("sso.test", &callback).header("cookie", "__Host-edge_oidc=someone-elses-state").send().await.unwrap();
    assert_eq!(resp.status(), 400);
    // A state the edge never issued, even if cookie and query agree.
    let resp =
        h.get("sso.test", "/.edge/callback?code=code-1&state=made-up").header("cookie", "__Host-edge_oidc=made-up").send().await.unwrap();
    assert_eq!(resp.status(), 400);

    // An id_token whose nonce belongs to another login.
    idp.state.lock().nonce_override = Some("replayed-nonce".into());
    let (callback, jar, _) = oidc_to_callback(&h, "sso.test").await;
    let resp = h.get("sso.test", &callback).header("cookie", jar.header()).send().await.unwrap();
    assert_eq!(resp.status(), 401);
    assert!(set_cookie(&resp, "__Host-edge_sid").is_none());
    assert_eq!(h.hits(), 0);
}

#[tokio::test]
async fn github_login_uses_verified_primary_email() {
    let gh = FakeGithub::start().await;
    let settings = Settings {
        oidc: OidcSettings {
            github: Some(ClientCredentials { client_id: CLIENT_ID.into(), client_secret: Some(CLIENT_SECRET.into()) }),
            github_endpoints: GithubEndpoints {
                authorize: format!("{}/login/oauth/authorize", gh.base),
                token: format!("{}/login/oauth/access_token", gh.base),
                api: gh.base.clone(),
            },
            ..OidcSettings::default()
        },
        ..Settings::default()
    };
    let h = Harness::with_settings(settings).await;
    let mut s = h.site("gh.test", Auth::Oidc { provider: OidcProvider::Github }, "");
    s.stage = Stage::Preview;
    s.allowed_emails = vec!["octo@acme.com".into()];
    h.add(s);

    let (callback, mut jar, authorize) = oidc_to_callback(&h, "gh.test").await;
    assert_eq!(param(&authorize, "code_challenge_method"), "S256");
    let done = h.get("gh.test", &callback).header("cookie", jar.header()).send().await.unwrap();
    assert_eq!(done.status(), 303);
    jar.take(&done);
    let e = Echo::from(h.get("gh.test", "/home").header("cookie", jar.header()).send().await.unwrap()).await;
    let jwks = h.jwks("gh.test").await;
    let claims = verify_principal(&e.header("x-principal-jwt").unwrap(), &jwks, "gh", Stage::Preview, unix_now()).unwrap();
    assert_eq!(claims.sub, "github:4242");
    assert_eq!(claims.email.as_deref(), Some("octo@acme.com"));
    assert!(claims.email_verified);
    drop(gh);
}

#[tokio::test]
async fn google_without_edge_registration_is_unavailable_not_open() {
    let h = Harness::start().await;
    h.add(h.site("g.test", Auth::Oidc { provider: OidcProvider::Google }, ""));
    let resp = h.get("g.test", "/.edge/login").send().await.unwrap();
    assert_eq!(resp.status(), 503);
    assert_eq!(h.get("g.test", "/").send().await.unwrap().status(), 302);
    assert_eq!(h.hits(), 0);
}
