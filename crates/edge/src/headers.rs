//! Request/response header hygiene, cookies, hosts and path normalization.
//!
//! The edge is the trust boundary: anything a client could use to impersonate the edge
//! (principal header, `x-edge-*`, forwarding headers, edge cookies) is removed before a request
//! reaches the app, whatever the route mode.

use std::net::IpAddr;

use http::header::{self, HeaderMap, HeaderName, HeaderValue};
use http::{Method, Uri, Version};

/// Session cookie; `__Host-` pins it to this exact host, `Secure`, `Path=/`.
pub(crate) const SESSION_COOKIE: &str = "__Host-edge_sid";
/// Double-submit CSRF token for edge forms.
pub(crate) const CSRF_COOKIE: &str = "__Host-edge_csrf";
/// Binds an OIDC `state` to the browser that started the login.
pub(crate) const OIDC_COOKIE: &str = "__Host-edge_oidc";
const EDGE_COOKIE_PREFIX: &str = "__Host-edge_";

const HOP_BY_HOP: [&str; 9] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// Client-supplied claims about the connection; the edge sets its own.
const SPOOFABLE: [&str; 7] = [
    "forwarded",
    "x-forwarded-for",
    "x-forwarded-proto",
    "x-forwarded-host",
    "x-forwarded-port",
    "x-real-ip",
    // Answered by the edge's own HTTP server; never re-sent upstream.
    "expect",
];

/// The request's target host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Host {
    /// Lowercase hostname without port or trailing dot: the directory key.
    pub name: String,
    /// `name[:port]` as the client addressed it.
    pub authority: String,
}

/// Host from the URI authority (h2 `:authority`, absolute-form h1) or the `Host` header.
pub(crate) fn host_of(uri: &Uri, headers: &HeaderMap) -> Option<Host> {
    let raw = match uri.authority() {
        Some(a) => a.as_str().to_owned(),
        None => headers.get(header::HOST)?.to_str().ok()?.to_owned(),
    };
    let raw = raw.rsplit('@').next().unwrap_or(&raw).trim().to_ascii_lowercase();
    if raw.starts_with('[') {
        return None;
    }
    let (name, port) = match raw.split_once(':') {
        Some((n, p)) if !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => (n, Some(p)),
        Some(_) => return None,
        None => (raw.as_str(), None),
    };
    let name = name.trim_end_matches('.');
    if name.is_empty() || name.len() > 253 || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.' || b == b'_') {
        return None;
    }
    let authority = match port {
        Some(p) => format!("{name}:{p}"),
        None => name.to_owned(),
    };
    Some(Host { name: name.to_owned(), authority })
}

/// Canonical form of a request path, used both for route matching and as the path forwarded to
/// the app, so the edge and the app never disagree about which route a request hit.
///
/// Percent-encoded unreserved characters are decoded (`%61` → `a`, `%2e` → `.`), remaining
/// escapes are upper-cased, repeated slashes are merged and dot segments are resolved.
/// Reserved escapes such as `%2F` stay encoded.
pub(crate) fn normalize_path(raw: &str) -> String {
    let decoded = decode_unreserved(raw);
    let trailing = decoded.ends_with('/') || decoded.ends_with("/.") || decoded.ends_with("/..");
    let mut out: Vec<&str> = Vec::new();
    for seg in decoded.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    let mut path = String::with_capacity(decoded.len() + 1);
    for seg in &out {
        path.push('/');
        path.push_str(seg);
    }
    if path.is_empty() || trailing {
        path.push('/');
    }
    path
}

fn decode_unreserved(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = String::with_capacity(raw.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            let c = (h << 4) | l;
            if c.is_ascii_alphanumeric() || matches!(c, b'-' | b'.' | b'_' | b'~') {
                out.push(c as char);
            } else {
                out.push('%');
                out.push(bytes[i + 1].to_ascii_uppercase() as char);
                out.push(bytes[i + 2].to_ascii_uppercase() as char);
            }
            i += 3;
            continue;
        }
        // Paths from hyper are ASCII; anything else is copied through byte-wise as UTF-8.
        let ch_len = utf8_len(bytes[i]);
        out.push_str(raw.get(i..i + ch_len).unwrap_or(""));
        i += ch_len.max(1);
    }
    out
}

fn utf8_len(first: u8) -> usize {
    match first {
        0x00..=0x7f => 1,
        0xc0..=0xdf => 2,
        0xe0..=0xef => 3,
        _ => 4,
    }
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// The `Upgrade` protocol when this is an HTTP/1.1 upgrade request (WebSocket).
pub(crate) fn upgrade_protocol(method: &Method, version: Version, headers: &HeaderMap) -> Option<HeaderValue> {
    if version != Version::HTTP_11 || method != Method::GET {
        return None;
    }
    let wants = connection_tokens(headers).iter().any(|t| t == "upgrade");
    if !wants {
        return None;
    }
    headers.get(header::UPGRADE).cloned()
}

fn connection_tokens(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|t| !t.is_empty())
        .collect()
}

/// Removes everything a client must not be able to say to the app through the edge.
pub(crate) fn strip_inbound(headers: &mut HeaderMap) {
    remove_hop_by_hop(headers);
    for name in SPOOFABLE {
        headers.remove(name);
    }
    headers.remove(cloud_contract::principal::HEADER);
    let edge: Vec<HeaderName> = headers.keys().filter(|k| k.as_str().starts_with("x-edge-")).cloned().collect();
    for name in edge {
        headers.remove(name);
    }
    strip_edge_cookies(headers);
}

/// Hop-by-hop headers of an upstream response (not for `101 Switching Protocols`).
pub(crate) fn strip_response(headers: &mut HeaderMap) {
    remove_hop_by_hop(headers);
}

fn remove_hop_by_hop(headers: &mut HeaderMap) {
    for token in connection_tokens(headers) {
        if let Ok(name) = HeaderName::from_bytes(token.as_bytes()) {
            headers.remove(name);
        }
    }
    for name in HOP_BY_HOP {
        headers.remove(name);
    }
}

/// Sets the forwarding headers the app may trust (the client's copies are already gone).
pub(crate) fn set_forwarded(headers: &mut HeaderMap, peer: IpAddr, scheme: &'static str, host: &Host) {
    if let Ok(v) = HeaderValue::from_str(&peer.to_string()) {
        headers.insert("x-forwarded-for", v);
    }
    headers.insert("x-forwarded-proto", HeaderValue::from_static(scheme));
    if let Ok(v) = HeaderValue::from_str(&host.authority) {
        headers.insert("x-forwarded-host", v.clone());
        headers.insert(header::HOST, v);
    }
}

/// First cookie called `name` across every `Cookie` header (h2 splits them).
pub(crate) fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(n, _)| *n == name)
        .map(|(_, v)| v.trim().to_owned())
}

/// Rebuilds `Cookie` without the edge's own cookies (they are edge credentials).
fn strip_edge_cookies(headers: &mut HeaderMap) {
    if !headers.contains_key(header::COOKIE) {
        return;
    }
    let kept: Vec<String> = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .map(str::trim)
        .filter(|p| !p.is_empty() && !p.starts_with(EDGE_COOKIE_PREFIX))
        .map(str::to_owned)
        .collect();
    headers.remove(header::COOKIE);
    if !kept.is_empty()
        && let Ok(v) = HeaderValue::from_str(&kept.join("; "))
    {
        headers.insert(header::COOKIE, v);
    }
}

/// `Set-Cookie` for an edge cookie: host-only, secure, not readable by scripts.
pub(crate) fn set_cookie(name: &str, value: &str, max_age: u64, same_site: &str) -> HeaderValue {
    let s = format!("{name}={value}; Path=/; Secure; HttpOnly; SameSite={same_site}; Max-Age={max_age}");
    HeaderValue::from_str(&s).unwrap_or_else(|_| HeaderValue::from_static("__Host-edge_invalid=; Max-Age=0"))
}

/// Expires an edge cookie.
pub(crate) fn clear_cookie(name: &str) -> HeaderValue {
    set_cookie(name, "", 0, "Lax")
}

/// Query parameters, percent-decoded; the first occurrence of a name wins.
pub(crate) fn query_params(uri: &Uri) -> Vec<(String, String)> {
    uri.query().map(|q| url::form_urlencoded::parse(q.as_bytes()).into_owned().collect()).unwrap_or_default()
}

/// Looks up a parameter in decoded pairs.
pub(crate) fn param<'a>(pairs: &'a [(String, String)], name: &str) -> Option<&'a str> {
    pairs.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(HeaderName::from_bytes(k.as_bytes()).unwrap(), HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn host_prefers_authority_and_drops_port_case_and_dot() {
        let uri: Uri = "/x".parse().unwrap();
        let h = host_of(&uri, &headers(&[("host", "App.Example.COM.:8443")])).unwrap();
        assert_eq!(h.name, "app.example.com");
        assert_eq!(h.authority, "app.example.com:8443");
        let abs: Uri = "https://other.example/x".parse().unwrap();
        assert_eq!(host_of(&abs, &headers(&[("host", "app.example.com")])).unwrap().name, "other.example");
    }

    #[test]
    fn host_rejects_garbage() {
        let uri: Uri = "/".parse().unwrap();
        for bad in ["", "a b", "[::1]:80", "host:port", "exa$mple.com"] {
            assert!(host_of(&uri, &headers(&[("host", bad)])).is_none(), "{bad}");
        }
        assert!(host_of(&uri, &HeaderMap::new()).is_none());
    }

    #[test]
    fn path_normalization_defeats_route_bypass_tricks() {
        assert_eq!(normalize_path("/%61dmin/x"), "/admin/x");
        assert_eq!(normalize_path("//admin//x"), "/admin/x");
        assert_eq!(normalize_path("/public/../admin"), "/admin");
        assert_eq!(normalize_path("/public/%2e%2e/admin"), "/admin");
        assert_eq!(normalize_path("/a/./b/"), "/a/b/");
        assert_eq!(normalize_path("/../../etc"), "/etc");
        assert_eq!(normalize_path(""), "/");
        assert_eq!(normalize_path("/a/.."), "/");
        // Reserved escapes keep their meaning.
        assert_eq!(normalize_path("/files/a%2fb"), "/files/a%2Fb");
        assert_eq!(normalize_path("/%zz"), "/%zz");
    }

    #[test]
    fn strip_inbound_removes_impersonation_and_hop_by_hop() {
        let mut h = headers(&[
            ("x-principal-jwt", "forged"),
            ("x-edge-user", "admin"),
            ("X-Edge-Anything", "1"),
            ("x-forwarded-for", "1.2.3.4"),
            ("forwarded", "for=1.2.3.4"),
            ("connection", "keep-alive, x-secret"),
            ("x-secret", "hop"),
            ("te", "trailers"),
            ("transfer-encoding", "chunked"),
            ("upgrade", "websocket"),
            ("cookie", "a=1; __Host-edge_sid=s3cret"),
            ("cookie", "__Host-edge_csrf=t; b=2"),
            ("accept", "text/html"),
        ]);
        strip_inbound(&mut h);
        let mut names: Vec<&str> = h.keys().map(|k| k.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["accept", "cookie"]);
        assert_eq!(h.get("cookie").unwrap(), "a=1; b=2");
    }

    #[test]
    fn only_edge_cookies_removed_header_dropped_when_empty() {
        let mut h = headers(&[("cookie", "__Host-edge_sid=x")]);
        strip_inbound(&mut h);
        assert!(h.get("cookie").is_none());
    }

    #[test]
    fn cookie_lookup_spans_split_headers() {
        let h = headers(&[("cookie", "a=1"), ("cookie", "__Host-edge_sid=abc; c=3")]);
        assert_eq!(cookie(&h, SESSION_COOKIE).as_deref(), Some("abc"));
        assert_eq!(cookie(&h, "missing"), None);
    }

    #[test]
    fn upgrade_needs_get_http11_and_connection_token() {
        let h = headers(&[("connection", "Upgrade"), ("upgrade", "websocket")]);
        assert!(upgrade_protocol(&Method::GET, Version::HTTP_11, &h).is_some());
        assert!(upgrade_protocol(&Method::POST, Version::HTTP_11, &h).is_none());
        assert!(upgrade_protocol(&Method::GET, Version::HTTP_2, &h).is_none());
        let no_conn = headers(&[("upgrade", "websocket")]);
        assert!(upgrade_protocol(&Method::GET, Version::HTTP_11, &no_conn).is_none());
    }
}
