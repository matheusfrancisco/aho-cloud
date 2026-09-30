//! `/.edge/*`: the paths the edge owns on every hostname. Login (password, magic link, OIDC),
//! `/.edge/me`, `/.edge/logout`, `/.edge/jwks.json`. The app never sees these paths.
//!
//! Every form POST needs the double-submit CSRF token (`__Host-edge_csrf`, SameSite=Strict)
//! and, when the browser sends one, a same-origin `Origin`/`Sec-Fetch-Site`.

use std::sync::Arc;

use cloud_api::internal::Site;
use cloud_contract::manifest::Auth;
use cloud_contract::principal::{JWKS_PATH, LOGOUT_PATH, ME_PATH};
use http::header::{self, HeaderMap, HeaderValue};
use http::{Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Limited};
use hyper::body::Incoming;
use subtle::ConstantTimeEq;

use crate::headers::{self, CSRF_COOKIE, Host, OIDC_COOKIE, SESSION_COOKIE};
use crate::mail::MagicLinkMail;
use crate::oidc::Provider;
use crate::pages::{self, Body};
use crate::server::{Inner, internal_error, now};
use crate::session::{self, Identity, MagicToken, PendingLogin, Session, random_token};

pub(crate) const LOGIN_PATH: &str = "/.edge/login";
pub(crate) const CALLBACK_PATH: &str = "/.edge/callback";
pub(crate) const MAGIC_PATH: &str = "/.edge/magic";
/// Magic links work once, for 15 minutes.
pub(crate) const MAGIC_TTL_SECS: u64 = 15 * 60;
const OIDC_TTL_SECS: u64 = 10 * 60;
const CSRF_TTL_SECS: u64 = 3600;
const FORM_LIMIT: usize = 16 * 1024;

pub(crate) async fn handle(inner: &Inner, site: &Arc<Site>, host: &Host, req: Request<Incoming>, path: &str) -> Response<Body> {
    let method = req.method().clone();
    match (path, method) {
        (JWKS_PATH, Method::GET | Method::HEAD) => jwks(inner, site),
        (ME_PATH, Method::GET) => me(inner, site, host, req.headers()),
        (LOGOUT_PATH, Method::GET | Method::POST) => logout(inner, &req),
        (LOGIN_PATH, Method::GET) => login_page(inner, site, host, &req).await,
        (LOGIN_PATH, Method::POST) => login_submit(inner, site, host, req).await,
        (CALLBACK_PATH, Method::GET) => callback(inner, site, host, &req).await,
        (MAGIC_PATH, Method::GET) => magic_page(host, &req),
        (MAGIC_PATH, Method::POST) => magic_submit(inner, site, host, req).await,
        _ => pages::text(StatusCode::NOT_FOUND, "not found\n"),
    }
}

/// `edge` mode without a session.
pub(crate) fn redirect_to_login(path: &str, query: Option<&str>) -> Response<Body> {
    let rd = match query {
        Some(q) => format!("{path}?{q}"),
        None => path.to_owned(),
    };
    let rd: String = url::form_urlencoded::byte_serialize(rd.as_bytes()).collect();
    pages::redirect(StatusCode::FOUND, &format!("{LOGIN_PATH}?rd={rd}"))
}

/// Authenticated, but not on the reviewer list of this preview/staging site.
pub(crate) fn not_invited() -> Response<Body> {
    pages::message(
        StatusCode::FORBIDDEN,
        "Not invited",
        "This environment is limited to invited reviewers. Sign in with an invited, verified email address.",
    )
}

/// Only same-site relative paths, never back into `/.edge/`.
fn safe_rd(rd: Option<&str>) -> String {
    match rd {
        Some(r)
            if r.starts_with('/')
                && !r.starts_with("//")
                && !r.starts_with("/.edge")
                && !r.chars().any(|c| c.is_control() || c == '\\') =>
        {
            r.to_owned()
        }
        _ => "/".to_owned(),
    }
}

fn redirect_uri(host: &Host) -> String {
    format!("{}{CALLBACK_PATH}", host.origin())
}

fn session_of(inner: &Inner, site: &Site, host: &Host, headers: &HeaderMap) -> Option<Session> {
    headers::cookie(headers, SESSION_COOKIE).and_then(|sid| inner.session(&sid, site, host))
}

fn jwks(inner: &Inner, site: &Site) -> Response<Body> {
    match inner.keys.jwks(&site.project, site.stage) {
        Ok(jwks) => {
            let mut r = pages::json(StatusCode::OK, &jwks);
            r.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("public, max-age=300"));
            r
        }
        Err(e) => {
            tracing::error!(project = %site.project, error = %e, "cannot load signing key");
            internal_error()
        }
    }
}

fn me(inner: &Inner, site: &Site, host: &Host, headers: &HeaderMap) -> Response<Body> {
    let Some(s) = session_of(inner, site, host, headers) else {
        return pages::json(StatusCode::UNAUTHORIZED, &serde_json::json!({ "error": "unauthenticated" }));
    };
    if !session::allowed(site, &s.identity) {
        return pages::json(StatusCode::FORBIDDEN, &serde_json::json!({ "error": "not_invited" }));
    }
    pages::json(StatusCode::OK, &session::claims(site, &s.identity, &s.id, now()))
}

fn logout(inner: &Inner, req: &Request<Incoming>) -> Response<Body> {
    if let Some(sid) = headers::cookie(req.headers(), SESSION_COOKIE) {
        inner.sessions.remove(&sid);
    }
    let params = headers::query_params(req.uri());
    let mut r = pages::redirect(StatusCode::SEE_OTHER, &safe_rd(headers::param(&params, "rd")));
    r.headers_mut().append(header::SET_COOKIE, headers::clear_cookie(SESSION_COOKIE));
    r
}

/// The CSRF token for this browser, and the cookie to set when it had none.
fn csrf_token(headers: &HeaderMap) -> (String, Option<HeaderValue>) {
    match headers::cookie(headers, CSRF_COOKIE).filter(|t| t.len() >= 32) {
        Some(t) => (t, None),
        None => {
            let t = random_token();
            let cookie = headers::set_cookie(CSRF_COOKIE, &t, CSRF_TTL_SECS, "Strict");
            (t, Some(cookie))
        }
    }
}

fn csrf_ok(headers: &HeaderMap, submitted: Option<&str>) -> bool {
    match (headers::cookie(headers, CSRF_COOKIE), submitted) {
        (Some(c), Some(s)) if !c.is_empty() => bool::from(c.as_bytes().ct_eq(s.as_bytes())),
        _ => false,
    }
}

/// Refuses cross-site form posts when the browser tells us where they came from.
fn same_origin(headers: &HeaderMap, host: &Host) -> bool {
    if let Some(site) = headers.get("sec-fetch-site").and_then(|v| v.to_str().ok())
        && !matches!(site, "same-origin" | "none")
    {
        return false;
    }
    match headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) {
        None => true,
        Some(origin) => origin.eq_ignore_ascii_case(&host.origin()),
    }
}

fn form_page(status: StatusCode, title: &str, body: String, cookie: Option<HeaderValue>) -> Response<Body> {
    let mut r = pages::html(status, title, &body);
    if let Some(c) = cookie {
        r.headers_mut().append(header::SET_COOKIE, c);
    }
    r
}

async fn read_form(req: Request<Incoming>) -> Result<(HeaderMap, Vec<(String, String)>), Box<Response<Body>>> {
    let (parts, body) = req.into_parts();
    let bytes = match Limited::new(body, FORM_LIMIT).collect().await {
        Ok(c) => c.to_bytes(),
        Err(_) => return Err(Box::new(pages::text(StatusCode::PAYLOAD_TOO_LARGE, "form too large\n"))),
    };
    Ok((parts.headers, url::form_urlencoded::parse(&bytes).into_owned().collect()))
}

async fn login_page(inner: &Inner, site: &Site, host: &Host, req: &Request<Incoming>) -> Response<Body> {
    let params = headers::query_params(req.uri());
    let rd = safe_rd(headers::param(&params, "rd"));
    if session_of(inner, site, host, req.headers()).is_some() {
        return pages::redirect(StatusCode::FOUND, &rd);
    }
    match &site.auth {
        Auth::Password => {
            let (csrf, cookie) = csrf_token(req.headers());
            form_page(StatusCode::OK, "Sign in", pages::password_form(&host.name, &rd, &csrf, None), cookie)
        }
        Auth::MagicLink { .. } => {
            let (csrf, cookie) = csrf_token(req.headers());
            form_page(StatusCode::OK, "Sign in", pages::magic_form(&host.name, &rd, &csrf, None), cookie)
        }
        Auth::Oidc { .. } => oidc_start(inner, site, host, rd).await,
        Auth::None | Auth::AppOwned => no_login(),
    }
}

fn no_login() -> Response<Body> {
    pages::message(StatusCode::NOT_FOUND, "No edge login", "This site does not use an edge login.")
}

async fn login_submit(inner: &Inner, site: &Site, host: &Host, req: Request<Incoming>) -> Response<Body> {
    if !same_origin(req.headers(), host) {
        return pages::message(StatusCode::FORBIDDEN, "Refused", "Cross-site form submissions are not accepted.");
    }
    let (hdrs, form) = match read_form(req).await {
        Ok(f) => f,
        Err(r) => return *r,
    };
    if !csrf_ok(&hdrs, headers::param(&form, "csrf")) {
        return pages::message(StatusCode::FORBIDDEN, "Form expired", "Reload the sign-in page and try again.");
    }
    let rd = safe_rd(headers::param(&form, "rd"));
    let csrf = headers::param(&form, "csrf").unwrap_or_default().to_owned();
    match &site.auth {
        Auth::Password => {
            let Some(hash) = site.password_hash.clone() else {
                tracing::error!(site = %host.name, "password auth without a password hash");
                return pages::message(StatusCode::SERVICE_UNAVAILABLE, "Unavailable", "Sign-in is not set up yet.");
            };
            let password = headers::param(&form, "password").unwrap_or_default().to_owned();
            if !verify_password(hash, password).await {
                let body = pages::password_form(&host.name, &rd, &csrf, Some("Wrong password."));
                return form_page(StatusCode::UNAUTHORIZED, "Sign in", body, None);
            }
            let identity =
                Identity { sub: "pwd:shared".into(), email: None, email_verified: false, groups: Vec::new(), amr: vec!["pwd".into()] };
            finish_login(inner, site, host, identity, &rd)
        }
        Auth::MagicLink { allowed_domains } => {
            let form_error = |status, msg: &str| form_page(status, "Sign in", pages::magic_form(&host.name, &rd, &csrf, Some(msg)), None);
            let Some(email) = normalize_email(headers::param(&form, "email").unwrap_or_default()) else {
                return form_error(StatusCode::BAD_REQUEST, "Enter a valid email address.");
            };
            let domain = email.rsplit('@').next().unwrap_or_default();
            if !allowed_domains.is_empty() && !allowed_domains.iter().any(|d| d.trim().eq_ignore_ascii_case(domain)) {
                return form_error(StatusCode::FORBIDDEN, &format!("Only {} addresses can sign in.", allowed_domains.join(", ")));
            }
            let probe =
                Identity { sub: String::new(), email: Some(email.clone()), email_verified: true, groups: Vec::new(), amr: Vec::new() };
            if !session::allowed(site, &probe) {
                return not_invited();
            }
            let Some(mailer) = &inner.mailer else {
                tracing::error!(site = %host.name, "magic-link auth but no mailer configured");
                return pages::message(StatusCode::SERVICE_UNAVAILABLE, "Unavailable", "Email sign-in is not available.");
            };
            let token = random_token();
            let t = now();
            let record = MagicToken { host: host.name.clone(), project: site.project.clone(), stage: site.stage, email: email.clone(), rd };
            if inner.magic.insert(&token, record, t + MAGIC_TTL_SECS, t).is_err() {
                return busy();
            }
            let mail = MagicLinkMail {
                to: email.clone(),
                host: host.name.clone(),
                link: format!("{}{MAGIC_PATH}?token={token}", host.origin()),
                expires_in_secs: MAGIC_TTL_SECS,
            };
            if let Err(e) = mailer.send_magic_link(&mail).await {
                inner.magic.remove(&token);
                tracing::warn!(site = %host.name, error = %e, "magic link not sent");
                return pages::message(StatusCode::BAD_GATEWAY, "Not sent", "We could not send the email. Try again.");
            }
            pages::html(StatusCode::OK, "Check your email", &pages::magic_sent(&email))
        }
        Auth::Oidc { .. } | Auth::None | Auth::AppOwned => no_login(),
    }
}

async fn verify_password(phc: String, password: String) -> bool {
    use argon2::password_hash::{PasswordHash, PasswordVerifier};
    tokio::task::spawn_blocking(move || {
        PasswordHash::new(&phc).map(|h| argon2::Argon2::default().verify_password(password.as_bytes(), &h).is_ok()).unwrap_or(false)
    })
    .await
    .unwrap_or(false)
}

fn normalize_email(raw: &str) -> Option<String> {
    let e = raw.trim().to_ascii_lowercase();
    let (local, domain) = e.rsplit_once('@')?;
    let ok = !local.is_empty()
        && e.len() <= 254
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && domain.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
        && !local.bytes().any(|b| b.is_ascii_whitespace() || b.is_ascii_control() || b"<>()[],;:\"@\\".contains(&b));
    ok.then_some(e)
}

fn busy() -> Response<Body> {
    pages::message(StatusCode::SERVICE_UNAVAILABLE, "Busy", "Too many sign-ins in progress. Try again shortly.")
}

fn magic_page(host: &Host, req: &Request<Incoming>) -> Response<Body> {
    let params = headers::query_params(req.uri());
    let token = headers::param(&params, "token").unwrap_or_default();
    if token.is_empty() {
        return pages::message(StatusCode::BAD_REQUEST, "Invalid link", "This sign-in link is incomplete.");
    }
    let (csrf, cookie) = csrf_token(req.headers());
    form_page(StatusCode::OK, "Sign in", pages::magic_confirm(&host.name, token, &csrf), cookie)
}

async fn magic_submit(inner: &Inner, site: &Site, host: &Host, req: Request<Incoming>) -> Response<Body> {
    if !same_origin(req.headers(), host) {
        return pages::message(StatusCode::FORBIDDEN, "Refused", "Cross-site form submissions are not accepted.");
    }
    let (hdrs, form) = match read_form(req).await {
        Ok(f) => f,
        Err(r) => return *r,
    };
    if !csrf_ok(&hdrs, headers::param(&form, "csrf")) {
        return pages::message(StatusCode::FORBIDDEN, "Form expired", "Open the link from your email again.");
    }
    let token = headers::param(&form, "token").unwrap_or_default();
    let used = || pages::message(StatusCode::BAD_REQUEST, "Link expired", "This sign-in link was already used or has expired.");
    let Some(m) = inner.magic.take(token, now()) else { return used() };
    if m.host != host.name || m.project != site.project || m.stage != site.stage {
        return used();
    }
    let identity = Identity {
        sub: format!("email:{}", m.email),
        email: Some(m.email),
        email_verified: true,
        groups: Vec::new(),
        amr: vec!["otp".into()],
    };
    finish_login(inner, site, host, identity, &m.rd)
}

async fn oidc_start(inner: &Inner, site: &Site, host: &Host, rd: String) -> Response<Body> {
    let provider = match Provider::for_site(site, &inner.settings.oidc) {
        Some(Ok(p)) => p,
        Some(Err(e)) => {
            tracing::error!(site = %host.name, error = %e, "oidc provider unavailable");
            return pages::message(StatusCode::SERVICE_UNAVAILABLE, "Unavailable", "Sign-in is not available right now.");
        }
        None => return no_login(),
    };
    let state = random_token();
    let pending = PendingLogin {
        host: host.name.clone(),
        project: site.project.clone(),
        stage: site.stage,
        nonce: random_token(),
        verifier: random_token(),
        rd,
    };
    let url = match inner.oidc.authorize_url(&provider, &redirect_uri(host), &state, &pending.nonce, &pending.verifier).await {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!(site = %host.name, error = %e, "cannot start oidc login");
            return pages::message(StatusCode::BAD_GATEWAY, "Unavailable", "The identity provider is not reachable.");
        }
    };
    let t = now();
    if inner.pending.insert(&state, pending, t + OIDC_TTL_SECS, t).is_err() {
        return busy();
    }
    let mut r = pages::redirect(StatusCode::FOUND, &url);
    // Lax: the cookie must come back on the provider's top-level redirect to the callback.
    r.headers_mut().append(header::SET_COOKIE, headers::set_cookie(OIDC_COOKIE, &state, OIDC_TTL_SECS, "Lax"));
    r
}

async fn callback(inner: &Inner, site: &Site, host: &Host, req: &Request<Incoming>) -> Response<Body> {
    let params = headers::query_params(req.uri());
    let failed = |status, text: &str| {
        let mut r = pages::message(status, "Sign-in failed", text);
        r.headers_mut().append(header::SET_COOKIE, headers::clear_cookie(OIDC_COOKIE));
        r
    };
    if let Some(err) = headers::param(&params, "error") {
        return failed(StatusCode::UNAUTHORIZED, &format!("The identity provider said: {err}"));
    }
    let (Some(state), Some(code)) = (headers::param(&params, "state"), headers::param(&params, "code")) else {
        return failed(StatusCode::BAD_REQUEST, "The identity provider's answer is incomplete.");
    };
    let bound = headers::cookie(req.headers(), OIDC_COOKIE).is_some_and(|c| bool::from(c.as_bytes().ct_eq(state.as_bytes())));
    if !bound {
        return failed(StatusCode::BAD_REQUEST, "This sign-in was started in another browser or tab. Start again.");
    }
    let Some(pending) = inner.pending.take(state, now()) else {
        return failed(StatusCode::BAD_REQUEST, "This sign-in expired or was already completed. Start again.");
    };
    if pending.host != host.name || pending.project != site.project || pending.stage != site.stage {
        return failed(StatusCode::BAD_REQUEST, "This sign-in belongs to another site.");
    }
    let provider = match Provider::for_site(site, &inner.settings.oidc) {
        Some(Ok(p)) => p,
        _ => return failed(StatusCode::SERVICE_UNAVAILABLE, "Sign-in is not available right now."),
    };
    let identity = match inner.oidc.complete(&provider, &redirect_uri(host), code, &pending.verifier, &pending.nonce, now()).await {
        Ok(i) => i,
        Err(e) => {
            tracing::warn!(site = %host.name, error = %e, "oidc login refused");
            return failed(StatusCode::UNAUTHORIZED, "The identity provider's answer did not verify.");
        }
    };
    let mut r = finish_login(inner, site, host, identity, &pending.rd);
    r.headers_mut().append(header::SET_COOKIE, headers::clear_cookie(OIDC_COOKIE));
    r
}

fn finish_login(inner: &Inner, site: &Site, host: &Host, identity: Identity, rd: &str) -> Response<Body> {
    if !session::allowed(site, &identity) {
        return not_invited();
    }
    let secret = random_token();
    let ttl = inner.settings.session_ttl_secs;
    let t = now();
    let s = Session {
        id: random_token()[..22].to_owned(),
        host: host.name.clone(),
        project: site.project.clone(),
        stage: site.stage,
        identity,
    };
    tracing::info!(site = %host.name, sub = %s.identity.sub, sid = %s.id, "edge login");
    if inner.sessions.insert(&secret, s, t + ttl, t).is_err() {
        return busy();
    }
    let mut r = pages::redirect(StatusCode::SEE_OTHER, &safe_rd(Some(rd)));
    r.headers_mut().append(header::SET_COOKIE, headers::set_cookie(SESSION_COOKIE, &secret, ttl, "Lax"));
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rd_only_allows_local_paths_outside_the_edge() {
        assert_eq!(safe_rd(Some("/dash?x=1")), "/dash?x=1");
        for bad in ["https://evil.com", "//evil.com", "/\\evil.com", "/.edge/logout", "", "/a\nb", "evil"] {
            assert_eq!(safe_rd(Some(bad)), "/", "{bad:?}");
        }
        assert_eq!(safe_rd(None), "/");
    }

    #[test]
    fn email_normalization() {
        assert_eq!(normalize_email("  Ann@Acme.COM ").as_deref(), Some("ann@acme.com"));
        for bad in ["ann", "@acme.com", "ann@acme", "a b@acme.com", "ann@-acme..com.", "<a>@acme.com", "a@b@c.com"] {
            assert!(normalize_email(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn origin_checks() {
        let host = Host { name: "app.test".into(), authority: "app.test:8443".into(), scheme: "https" };
        let h = |pairs: &[(&'static str, &str)]| {
            let mut m = HeaderMap::new();
            for (k, v) in pairs {
                m.insert(*k, HeaderValue::from_str(v).unwrap());
            }
            m
        };
        assert!(same_origin(&h(&[]), &host));
        assert!(same_origin(&h(&[("origin", "https://app.test:8443"), ("sec-fetch-site", "same-origin")]), &host));
        assert!(!same_origin(&h(&[("origin", "https://evil.test")]), &host));
        assert!(!same_origin(&h(&[("origin", "null")]), &host));
        // The scheme is part of the origin: an http page cannot post to the https site.
        assert!(!same_origin(&h(&[("origin", "http://app.test:8443")]), &host));
        assert!(!same_origin(&h(&[("sec-fetch-site", "cross-site")]), &host));
    }
}
