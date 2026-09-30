//! Edge-generated responses: status pages and login forms. Every interpolated value is
//! HTML-escaped; pages carry a CSP that allows no scripts, no framing and same-origin forms only.

use http::header::{self, HeaderValue};
use http::{Response, StatusCode};
use http_body_util::{BodyExt, Empty, Full};
use hyper::body::Bytes;

/// Body type of every response the edge returns.
pub(crate) type Body = http_body_util::combinators::BoxBody<Bytes, hyper::Error>;

pub(crate) fn full(bytes: impl Into<Bytes>) -> Body {
    Full::new(bytes.into()).map_err(|never| match never {}).boxed()
}

pub(crate) fn empty() -> Body {
    Empty::new().map_err(|never| match never {}).boxed()
}

pub(crate) fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

fn layout(title: &str, body: &str) -> String {
    format!(
        "<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\">\
         <meta name=\"viewport\" content=\"width=device-width,initial-scale=1\">\
         <title>{t}</title><style>body{{font:16px/1.5 system-ui,sans-serif;max-width:28rem;\
         margin:12vh auto;padding:0 1rem;color:#222}}input,button{{font:inherit;padding:.5rem;\
         width:100%;box-sizing:border-box;margin:.25rem 0}}.err{{color:#b00020}}</style></head>\
         <body><h1>{t}</h1>{body}</body></html>",
        t = escape(title),
    )
}

/// An HTML page with the edge's hardening headers.
pub(crate) fn html(status: StatusCode, title: &str, body: &str) -> Response<Body> {
    let mut resp = Response::new(full(layout(title, body)));
    *resp.status_mut() = status;
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/html; charset=utf-8"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(
        header::CONTENT_SECURITY_POLICY,
        HeaderValue::from_static("default-src 'none'; style-src 'unsafe-inline'; form-action 'self'; frame-ancestors 'none'"),
    );
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    // `same-origin`, not `no-referrer`: under `no-referrer` browsers send `Origin: null` on the
    // page's own form posts, which the cross-site check must refuse. Nothing leaks cross-origin
    // either way (magic-link tokens live in these URLs).
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("same-origin"));
    resp
}

/// A short message page.
pub(crate) fn message(status: StatusCode, title: &str, text: &str) -> Response<Body> {
    html(status, title, &format!("<p>{}</p>", escape(text)))
}

pub(crate) fn json(status: StatusCode, value: &impl serde::Serialize) -> Response<Body> {
    let body = serde_json::to_vec(value).unwrap_or_default();
    let mut resp = Response::new(full(body));
    *resp.status_mut() = status;
    resp.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("application/json"));
    resp.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

pub(crate) fn text(status: StatusCode, body: impl Into<Bytes>) -> Response<Body> {
    let mut resp = Response::new(full(body));
    *resp.status_mut() = status;
    resp.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
    resp.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

/// Redirect with `status` (302/303/308) to `location`; an unencodable target becomes `/`.
pub(crate) fn redirect(status: StatusCode, location: &str) -> Response<Body> {
    let mut resp = Response::new(empty());
    *resp.status_mut() = status;
    let loc = HeaderValue::from_str(location).unwrap_or_else(|_| HeaderValue::from_static("/"));
    resp.headers_mut().insert(header::LOCATION, loc);
    resp.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    resp
}

pub(crate) fn misdirected() -> Response<Body> {
    text(StatusCode::MISDIRECTED_REQUEST, "421 Misdirected Request: this edge does not serve that host\n")
}

pub(crate) fn tombstone(host: &str) -> Response<Body> {
    message(StatusCode::GONE, "This site is gone", &format!("{host} is no longer served here. If you own it, point it somewhere new."))
}

pub(crate) fn paused(host: &str) -> Response<Body> {
    let mut r = message(
        StatusCode::SERVICE_UNAVAILABLE,
        "This site is paused",
        &format!("{host} is asleep: its owner's budget paused it. It will be back when they resume it."),
    );
    r.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from_static("3600"));
    r
}

pub(crate) fn maintenance(host: &str) -> Response<Body> {
    let mut r = message(
        StatusCode::SERVICE_UNAVAILABLE,
        "Down for maintenance",
        &format!("{host} is undergoing maintenance and will be back shortly."),
    );
    r.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from_static("120"));
    r
}

pub(crate) fn bad_gateway() -> Response<Body> {
    message(StatusCode::BAD_GATEWAY, "Bad gateway", "No healthy instance of this site answered. Try again shortly.")
}

fn error_line(error: Option<&str>) -> String {
    error.map(|e| format!("<p class=\"err\">{}</p>", escape(e))).unwrap_or_default()
}

pub(crate) fn password_form(host: &str, rd: &str, csrf: &str, error: Option<&str>) -> String {
    format!(
        "<p>{host} is password protected.</p>{err}<form method=\"post\" action=\"/.edge/login\">\
         <input type=\"hidden\" name=\"csrf\" value=\"{csrf}\"><input type=\"hidden\" name=\"rd\" value=\"{rd}\">\
         <input type=\"password\" name=\"password\" placeholder=\"Password\" autocomplete=\"current-password\" required autofocus>\
         <button>Continue</button></form>",
        host = escape(host),
        err = error_line(error),
        csrf = escape(csrf),
        rd = escape(rd),
    )
}

pub(crate) fn magic_form(host: &str, rd: &str, csrf: &str, error: Option<&str>) -> String {
    format!(
        "<p>Sign in to {host} with a link sent to your email.</p>{err}<form method=\"post\" action=\"/.edge/login\">\
         <input type=\"hidden\" name=\"csrf\" value=\"{csrf}\"><input type=\"hidden\" name=\"rd\" value=\"{rd}\">\
         <input type=\"email\" name=\"email\" placeholder=\"you@example.com\" autocomplete=\"email\" required autofocus>\
         <button>Email me a link</button></form>",
        host = escape(host),
        err = error_line(error),
        csrf = escape(csrf),
        rd = escape(rd),
    )
}

pub(crate) fn magic_sent(email: &str) -> String {
    format!("<p>We sent a sign-in link to <b>{}</b>. It works once, for 15 minutes.</p>", escape(email))
}

pub(crate) fn magic_confirm(host: &str, token: &str, csrf: &str) -> String {
    format!(
        "<p>Continue signing in to {host}?</p><form method=\"post\" action=\"/.edge/magic\">\
         <input type=\"hidden\" name=\"csrf\" value=\"{csrf}\"><input type=\"hidden\" name=\"token\" value=\"{token}\">\
         <button>Sign in</button></form>",
        host = escape(host),
        csrf = escape(csrf),
        token = escape(token),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escaping_neutralizes_markup_in_every_field() {
        let page = password_form("h<script>", "/\"><script>alert(1)</script>", "c'x", Some("<b>"));
        assert!(!page.contains("<script>"));
        assert!(page.contains("value=\"/&quot;&gt;&lt;script&gt;"), "rd cannot close its attribute");
        assert!(page.contains("&lt;b&gt;"));
        assert!(page.contains("c&#39;x"));
    }

    #[test]
    fn login_pages_let_browsers_send_origin_on_their_own_form_posts() {
        // Under `no-referrer` Chromium sends `Origin: null` with the login form's POST, and the
        // cross-site check refuses it: nobody could sign in (found running the local stack).
        let page = html(StatusCode::OK, "Sign in", "");
        let policy = page.headers().get(header::REFERRER_POLICY).and_then(|v| v.to_str().ok());
        assert!(matches!(policy, Some("same-origin" | "strict-origin" | "strict-origin-when-cross-origin")), "{policy:?}");
    }
}
