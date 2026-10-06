//! URL / route canonicalization and segment matching (knowledge.md "Layer 4: App map" link steps
//! 2 and 4, Knowledge MCP `route_info`, triage stage 2a).
//!
//! The indexer builds endpoint/page keys and links client calls with these; the server
//! canonicalizes `route_info` input and matches it against the stored keys, so both sides must
//! compute them identically.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::domain::error::DomainError;

/// Go 1.22 `ServeMux` end anchor: `/items/{$}` matches exactly `/items/`.
const GO_END_ANCHOR: &str = "{$}";

/// A URL or route path as extracted from source (`AppFact::{RouteGroup, RouteDecl, PageDecl,
/// HttpCall}`): literal text and holes from interpolation or concatenation.
#[derive(Clone, PartialEq, Eq, Hash, Debug, Default, Serialize, Deserialize)]
pub struct UrlTemplate {
    pub parts: Vec<UrlPart>,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum UrlPart {
    Lit(String),
    /// A hole; the variable or expression name when known (`baseURL`, `id`).
    Var(Option<String>),
}

/// One canonical path segment.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum RouteSegment {
    /// Literal text, compared case-sensitively.
    Lit(String),
    /// `{}`: a path parameter, wildcard, or hole; matches exactly one segment.
    Param,
}

/// Canonical path: `/`-separated segments, no empty segments (so no trailing `/`), parameters as
/// `{}`. Displays as `/api/connections/{}`; the root is `/`. Serde uses the display string.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct RoutePattern {
    segments: Vec<RouteSegment>,
}

impl RoutePattern {
    /// Canonical form of a declared route or page path (link step 2), after the caller has
    /// prepended resolved group prefixes. Parameter segments become `{}`: `:id`, `:id?`, `{id}`,
    /// `{id:[0-9]+}`, `{path...}`, `*`, `*filepath`, `[id]`, `[...slug]`, and any segment holding a
    /// hole or a `{…}` part (`{name}.json`). `/` inside `{…}` does not split. Empty segments
    /// (`//`, trailing `/`) and Go's `{$}` anchor are dropped. `?` and `#` are literal text here.
    pub fn from_route(path: &UrlTemplate) -> Self {
        canonicalize(Mode::Route, path.parts.iter().map(Part::from))
    }

    /// Canonical form of a client call URL (link step 4): like [`Self::from_route`], plus the
    /// scheme and authority (`https://host:port`, `//host`, also when the host is a hole), leading
    /// holes (the base-URL variable), and everything from the first `?` or `#` (query, fragment)
    /// are dropped. A relative path (`users/1`) is read from the root.
    pub fn from_client_url(url: &UrlTemplate) -> Self {
        canonicalize(Mode::Url, url.parts.iter().map(Part::from))
    }

    /// Canonical form of a path or full URL given as text (`route_info` input, URLs found in
    /// ticket text): surrounding whitespace trimmed, then as [`Self::from_client_url`].
    pub fn from_url(url: &str) -> Self {
        canonicalize(Mode::Url, std::iter::once(Part::Lit(url.trim())))
    }

    pub fn segments(&self) -> &[RouteSegment] {
        &self.segments
    }
}

impl fmt::Display for RoutePattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.segments.is_empty() {
            return f.write_str("/");
        }
        for segment in &self.segments {
            f.write_str("/")?;
            match segment {
                RouteSegment::Lit(text) => f.write_str(text)?,
                RouteSegment::Param => f.write_str("{}")?,
            }
        }
        Ok(())
    }
}

impl fmt::Debug for RoutePattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RoutePattern({self})")
    }
}

/// Parses the canonical display form only: leading `/`, no empty segment, parameters exactly `{}`.
impl FromStr for RoutePattern {
    type Err = DomainError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let rest = s.strip_prefix('/').ok_or_else(|| DomainError::invalid("route path must start with `/`"))?;
        if rest.is_empty() {
            return Ok(Self::default());
        }
        let mut segments = Vec::new();
        for segment in rest.split('/') {
            match segment {
                "" => return Err(DomainError::invalid("route path has an empty segment or a trailing `/`")),
                "{}" => segments.push(RouteSegment::Param),
                _ if segment == GO_END_ANCHOR || is_param_syntax(segment) => {
                    return Err(DomainError::invalid(format!("route segment `{segment}` is not canonical")));
                }
                _ => segments.push(RouteSegment::Lit(segment.to_owned())),
            }
        }
        Ok(Self { segments })
    }
}

impl Serialize for RoutePattern {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for RoutePattern {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Endpoint key `METHOD /path` (link step 2; `AppNode.key`, e.g. `GET /api/connections/{}`).
/// The method is uppercase; an unknown method or one accepting any method displays as `ANY`.
/// Serde uses the display string.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RouteKey {
    method: Option<String>,
    path: RoutePattern,
}

impl RouteKey {
    /// `method` as extracted (`GET`, `get`, `:post`, gin `Any`). Absent, `any`, or not an HTTP
    /// token (a variable name such as `m.Method`) → `ANY`.
    pub fn new(method: Option<&str>, path: RoutePattern) -> Self {
        Self { method: method.and_then(canonical_method), path }
    }

    /// Uppercase method; `None` = `ANY`.
    pub fn method(&self) -> Option<&str> {
        self.method.as_deref()
    }

    pub fn path(&self) -> &RoutePattern {
        &self.path
    }
}

impl fmt::Display for RouteKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.method.as_deref().unwrap_or("ANY"), self.path)
    }
}

impl fmt::Debug for RouteKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RouteKey({self})")
    }
}

/// Parses the canonical display form only (uppercase method or `ANY`, one space, canonical path).
impl FromStr for RouteKey {
    type Err = DomainError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (method, path) = s.split_once(' ').ok_or_else(|| DomainError::invalid("route key must be `METHOD /path`"))?;
        let method = match method {
            "ANY" => None,
            _ if is_canonical_method(method) => Some(method.to_owned()),
            _ => return Err(DomainError::invalid(format!("route key method `{method}` is not canonical"))),
        };
        Ok(Self { method, path: path.parse()? })
    }
}

impl Serialize for RouteKey {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for RouteKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = <std::borrow::Cow<'de, str>>::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// Result of [`segment_match`].
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
pub struct SegmentMatch {
    /// Positions where both sides are equal literals.
    pub literal: u32,
    /// Segment count (equal on both sides).
    pub segments: u32,
}

impl SegmentMatch {
    /// Matched literal segments / total (link step 4: edges need ≥ 0.5). Root against root is 1.
    pub fn confidence(&self) -> f32 {
        if self.segments == 0 { 1.0 } else { self.literal as f32 / self.segments as f32 }
    }
}

/// Segment-wise match of a canonical URL against a canonical route: same segment count, literal
/// segments equal, `{}` on either side matches any one segment.
pub fn segment_match(url: &RoutePattern, route: &RoutePattern) -> Option<SegmentMatch> {
    if url.segments.len() != route.segments.len() {
        return None;
    }
    let segments = u32::try_from(url.segments.len()).ok()?;
    let mut literal = 0;
    for (u, r) in url.segments.iter().zip(&route.segments) {
        if let (RouteSegment::Lit(a), RouteSegment::Lit(b)) = (u, r) {
            if a != b {
                return None;
            }
            literal += 1;
        }
    }
    Some(SegmentMatch { literal, segments })
}

/// How a request's method relates to a route's method; ordered worst to best.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MethodMatch {
    /// Both known and different: still a candidate, ranked below the others.
    Different,
    /// The request method is unknown or the route accepts any method.
    Unspecified,
    Equal,
}

/// Result of [`route_match`]. `Ord`: greater is better — more literal segments first, then
/// method (`Equal` > `Unspecified` > `Different`); candidates of one request share `segments`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
pub struct RouteMatch {
    pub path: SegmentMatch,
    pub method: MethodMatch,
}

/// [`segment_match`] of the paths plus the method relation (`route_info` "best = most literal
/// segments, then method equality"; link step 4 "prefer equal method").
pub fn route_match(request: &RouteKey, route: &RouteKey) -> Option<RouteMatch> {
    let path = segment_match(&request.path, &route.path)?;
    let method = match (request.method(), route.method()) {
        (Some(a), Some(b)) if a == b => MethodMatch::Equal,
        (Some(_), Some(_)) => MethodMatch::Different,
        _ => MethodMatch::Unspecified,
    };
    Some(RouteMatch { path, method })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Declared route: no scheme/host, holes are parameters, `?`/`#` are literal.
    Route,
    /// Client URL or URL text: scheme/host, leading holes, query and fragment dropped.
    Url,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    /// URL mode before any literal text: holes here are the base URL and are dropped.
    Start,
    /// URL mode inside `scheme://authority`: skipped up to the first `/`.
    Authority,
    Path,
    /// URL mode after `?`/`#`: everything else is ignored.
    Done,
}

#[derive(Clone, Copy)]
enum Part<'a> {
    Lit(&'a str),
    Var,
}

impl<'a> From<&'a UrlPart> for Part<'a> {
    fn from(part: &'a UrlPart) -> Self {
        match part {
            UrlPart::Lit(text) => Part::Lit(text),
            UrlPart::Var(_) => Part::Var,
        }
    }
}

fn canonicalize<'a>(mode: Mode, parts: impl Iterator<Item = Part<'a>>) -> RoutePattern {
    let mut canon = Canon {
        mode,
        state: match mode {
            Mode::Route => State::Path,
            Mode::Url => State::Start,
        },
        segments: Vec::new(),
        current: String::new(),
        hole: false,
        depth: 0,
    };
    for part in parts {
        match part {
            Part::Lit(text) => canon.text(text),
            Part::Var => canon.var(),
        }
        if canon.state == State::Done {
            break;
        }
    }
    canon.finish()
}

struct Canon {
    mode: Mode,
    state: State,
    segments: Vec<RouteSegment>,
    /// Literal text of the segment being read.
    current: String,
    /// The segment being read contains a hole.
    hole: bool,
    /// `{…}` nesting depth inside the segment being read.
    depth: u32,
}

impl Canon {
    fn var(&mut self) {
        if self.state == State::Path {
            self.hole = true;
        }
    }

    fn text(&mut self, mut text: &str) {
        if self.state == State::Start {
            if text.is_empty() {
                return;
            }
            match strip_scheme(text) {
                Some(rest) => {
                    self.state = State::Authority;
                    text = rest;
                }
                None => self.state = State::Path,
            }
        }
        if self.state == State::Authority {
            let Some(end) = text.find(['/', '?', '#']) else {
                return;
            };
            text = &text[end..];
            if !text.starts_with('/') {
                self.state = State::Done;
                return;
            }
            self.state = State::Path;
        }
        if self.state != State::Path {
            return;
        }
        for c in text.chars() {
            match c {
                '/' if self.depth == 0 => self.end_segment(),
                '?' | '#' if self.depth == 0 && self.mode == Mode::Url => {
                    self.end_segment();
                    self.state = State::Done;
                    return;
                }
                _ => {
                    match c {
                        '{' => self.depth += 1,
                        '}' => self.depth = self.depth.saturating_sub(1),
                        _ => {}
                    }
                    self.current.push(c);
                }
            }
        }
    }

    fn end_segment(&mut self) {
        let hole = std::mem::replace(&mut self.hole, false);
        self.depth = 0;
        if hole || (is_param_syntax(&self.current) && self.current != GO_END_ANCHOR) {
            self.current.clear();
            self.segments.push(RouteSegment::Param);
        } else if self.current.is_empty() || self.current == GO_END_ANCHOR {
            self.current.clear();
        } else {
            self.segments.push(RouteSegment::Lit(std::mem::take(&mut self.current)));
        }
    }

    fn finish(mut self) -> RoutePattern {
        if self.state == State::Path {
            self.end_segment();
        }
        RoutePattern { segments: self.segments }
    }
}

/// Remainder after `scheme://` (RFC 3986 scheme) or a protocol-relative `//`.
fn strip_scheme(text: &str) -> Option<&str> {
    if let Some(rest) = text.strip_prefix("//") {
        return Some(rest);
    }
    let (scheme, rest) = text.split_once("://")?;
    let mut chars = scheme.chars();
    let valid =
        chars.next().is_some_and(|c| c.is_ascii_alphabetic()) && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'));
    valid.then_some(rest)
}

/// Parameter / wildcard syntaxes of the supported routers (Express/gin/echo/chi/gorilla/Go 1.22/
/// Next/reitit): `:name`, `{…}` anywhere, `*` anywhere, `[…]`.
fn is_param_syntax(segment: &str) -> bool {
    segment.starts_with(':') || segment.contains(['{', '}', '*']) || (segment.starts_with('[') && segment.ends_with(']'))
}

fn canonical_method(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let raw = raw.strip_prefix(':').unwrap_or(raw);
    let token = !raw.is_empty() && raw.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    (token && !raw.eq_ignore_ascii_case("any")).then(|| raw.to_ascii_uppercase())
}

fn is_canonical_method(method: &str) -> bool {
    !method.is_empty() && method.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lit(text: &str) -> UrlPart {
        UrlPart::Lit(text.to_owned())
    }

    fn var(name: &str) -> UrlPart {
        UrlPart::Var(Some(name.to_owned()))
    }

    fn route(parts: Vec<UrlPart>) -> String {
        RoutePattern::from_route(&UrlTemplate { parts }).to_string()
    }

    fn client(parts: Vec<UrlPart>) -> String {
        RoutePattern::from_client_url(&UrlTemplate { parts }).to_string()
    }

    fn pat(s: &str) -> RoutePattern {
        s.parse().unwrap()
    }

    fn key(s: &str) -> RouteKey {
        s.parse().unwrap()
    }

    #[test]
    fn route_parameters_wildcards_and_slashes_canonicalize() {
        let cases = [
            ("/api/connections/:id", "/api/connections/{}"),
            ("/api/connections/{id}", "/api/connections/{}"),
            ("/items/{id:[0-9]+}", "/items/{}"),
            ("/items/{id:[0-9]{1,3}}/x", "/items/{}/x"),
            ("/files/{rest:.+/raw}", "/files/{}"),
            ("/files/{path...}", "/files/{}"),
            ("/files/{name}.json", "/files/{}"),
            ("/users/:id?", "/users/{}"),
            ("/static/*", "/static/{}"),
            ("/static/*filepath", "/static/{}"),
            ("/connections/[id]/edit", "/connections/{}/edit"),
            ("/docs/[...slug]", "/docs/{}"),
            ("/docs/[[...slug]]", "/docs/{}"),
            ("/api/connections/", "/api/connections"),
            ("/api//connections", "/api/connections"),
            ("/items/{$}", "/items"),
            ("api/plugins", "/api/plugins"),
            ("/ab?cd#x", "/ab?cd#x"),
            ("/", "/"),
            ("", "/"),
        ];
        for (input, expected) in cases {
            assert_eq!(route(vec![lit(input)]), expected, "{input}");
        }
    }

    #[test]
    fn route_holes_and_concatenated_prefixes() {
        assert_eq!(route(vec![lit("/api/"), var("version"), lit("/users")]), "/api/{}/users");
        assert_eq!(route(vec![lit("/api/v"), var("n"), lit("/users")]), "/api/{}/users");
        assert_eq!(route(vec![var("prefix"), lit("/users")]), "/{}/users");
        // Group prefix + route path, as the linker joins them.
        assert_eq!(route(vec![lit("/api"), lit("/connections/"), lit(":id")]), "/api/connections/{}");
        assert_eq!(route(vec![lit("/api/"), lit("/connections")]), "/api/connections");
    }

    #[test]
    fn client_urls_drop_scheme_host_base_variable_query_and_fragment() {
        let cases = [
            (vec![lit("https://app.hoop.dev/api/connections/"), var("id"), lit("?expand=true")], "/api/connections/{}"),
            (vec![var("API_URL"), lit("/connections/"), var("id"), lit("/edit")], "/connections/{}/edit"),
            (vec![var("base"), var("prefix"), lit("/x")], "/x"),
            (vec![lit("https://"), var("host"), lit(".example.com/api/x")], "/api/x"),
            (vec![lit("//cdn.example.com/a/b")], "/a/b"),
            (vec![lit("/api/plugins/"), var("name"), lit("#section")], "/api/plugins/{}"),
            (vec![lit("/api/search?q="), var("q"), lit("&page=2")], "/api/search"),
            (vec![lit("users/"), var("id")], "/users/{}"),
            (vec![lit("http://localhost:8009")], "/"),
            (vec![lit("http://localhost:8009?x=/y")], "/"),
            (vec![lit("/api/v"), var("v"), lit("/x/")], "/api/{}/x"),
            (vec![lit("/api/users/{id}")], "/api/users/{}"),
            (vec![lit("")], "/"),
        ];
        for (parts, expected) in cases {
            let shown = format!("{parts:?}");
            assert_eq!(client(parts), expected, "{shown}");
        }
    }

    #[test]
    fn url_text_canonicalizes_like_a_client_url() {
        let cases = [
            ("https://app.hoop.dev/connections/abc/edit?tab=1#top", "/connections/abc/edit"),
            ("/api/connections/pg-prod/", "/api/connections/pg-prod"),
            ("  /a/b \n", "/a/b"),
            ("https://app.hoop.dev", "/"),
            ("https://app.hoop.dev?next=/x", "/"),
            ("HTTP://Example.com:8080/A/b", "/A/b"),
            ("/connections/:id", "/connections/{}"),
            ("/redirect?to=https://x/y", "/redirect"),
        ];
        for (input, expected) in cases {
            assert_eq!(RoutePattern::from_url(input).to_string(), expected, "{input}");
        }
    }

    #[test]
    fn canonical_path_parse_round_trips_and_rejects_non_canonical() {
        for s in ["/", "/api/{}/x", "/a b/c", "/ab?cd"] {
            assert_eq!(pat(s).to_string(), s);
        }
        assert_eq!(pat("/api/{}").segments(), [RouteSegment::Lit("api".to_owned()), RouteSegment::Param]);
        for s in ["", "api/x", "/a/", "/a//b", "/a/:id", "/a/{id}", "/a/*", "/a/[id]", "/a/{$}"] {
            assert!(s.parse::<RoutePattern>().is_err(), "{s}");
        }
    }

    #[test]
    fn route_key_normalizes_method_and_round_trips() {
        let path = pat("/api/{}");
        assert_eq!(RouteKey::new(Some("get"), path.clone()).to_string(), "GET /api/{}");
        assert_eq!(RouteKey::new(Some(" :post "), path.clone()).to_string(), "POST /api/{}");
        assert_eq!(RouteKey::new(None, path.clone()).to_string(), "ANY /api/{}");
        assert_eq!(RouteKey::new(Some("Any"), path.clone()).to_string(), "ANY /api/{}");
        assert_eq!(RouteKey::new(Some("m.Method"), path.clone()).to_string(), "ANY /api/{}");
        assert_eq!(RouteKey::new(Some(""), path.clone()).method(), None);

        let parsed = key("PROPFIND /a/{}/b");
        assert_eq!(parsed.method(), Some("PROPFIND"));
        assert_eq!(parsed.path(), &pat("/a/{}/b"));
        assert_eq!(key("ANY /").method(), None);
        for s in ["get /a", "GET", "GET a", "GET /a/", "/a", "GET  /a", "GET /a/:id"] {
            assert!(s.parse::<RouteKey>().is_err(), "{s}");
        }

        let json = serde_json::to_string(&key("GET /api/connections/{}")).unwrap();
        assert_eq!(json, r#""GET /api/connections/{}""#);
        assert_eq!(serde_json::from_str::<RouteKey>(&json).unwrap(), key("GET /api/connections/{}"));
        assert!(serde_json::from_str::<RouteKey>(r#""GET /a/:id""#).is_err());
    }

    #[test]
    fn segment_match_table() {
        let cases = [
            ("/api/connections/abc", "/api/connections/{}", Some((2, 3))),
            ("/api/connections/abc", "/api/connections/abc", Some((3, 3))),
            ("/api/{}/list", "/api/connections/list", Some((2, 3))),
            ("/api/connections/{}", "/api/connections/{}", Some((2, 3))),
            ("/api/{}/{}", "/api/connections/{}", Some((1, 3))),
            ("/api/connections", "/api/connections/{}", None),
            ("/api/connections/abc/edit", "/api/connections/{}", None),
            ("/api/users/abc", "/api/connections/{}", None),
            ("/API/connections", "/api/connections", None),
            ("/", "/", Some((0, 0))),
            ("/x", "/", None),
        ];
        for (url, route, expected) in cases {
            let got = segment_match(&pat(url), &pat(route)).map(|m| (m.literal, m.segments));
            assert_eq!(got, expected, "{url} vs {route}");
        }
    }

    #[test]
    fn confidence_is_literal_share_with_root_as_full() {
        let m = |url: &str, route: &str| segment_match(&pat(url), &pat(route)).unwrap().confidence();
        assert!((m("/api/connections/abc", "/api/connections/{}") - 2.0 / 3.0).abs() < 1e-6);
        assert_eq!(m("/api/connections/abc", "/api/connections/abc"), 1.0);
        assert_eq!(m("/", "/"), 1.0);
        assert!(m("/api/{}/{}", "/api/connections/{}") < 0.5);
        assert_eq!(m("/a/{}", "/a/b"), 0.5);
    }

    #[test]
    fn client_call_links_to_grouped_endpoint() {
        // gin: g := r.Group("/api"); g.GET("/connections/:id", h)
        let endpoint =
            RouteKey::new(Some("GET"), RoutePattern::from_route(&UrlTemplate { parts: vec![lit("/api"), lit("/connections/:id")] }));
        assert_eq!(endpoint.to_string(), "GET /api/connections/{}");
        // axios.get(`${API_URL}/api/connections/${id}?expand=1`)
        let call = RouteKey::new(
            Some("get"),
            RoutePattern::from_client_url(&UrlTemplate {
                parts: vec![var("API_URL"), lit("/api/connections/"), var("id"), lit("?expand=1")],
            }),
        );
        let m = route_match(&call, &endpoint).unwrap();
        assert_eq!(m.method, MethodMatch::Equal);
        assert!(m.path.confidence() >= 0.5);
        // route_info with a pasted browser URL.
        let pasted = RouteKey::new(None, RoutePattern::from_url("https://app.hoop.dev/api/connections/pg-prod?x=1"));
        assert_eq!(route_match(&pasted, &endpoint).map(|m| (m.path.literal, m.method)), Some((2, MethodMatch::Unspecified)));
    }

    #[test]
    fn route_matches_rank_by_literal_segments_then_method() {
        let request = RouteKey::new(Some("GET"), RoutePattern::from_url("/api/connections/abc"));
        let routes = [
            "POST /api/connections/{}",
            "ANY /api/{}/{}",
            "GET /api/connections/{}",
            "GET /api/connections",
            "ANY /api/connections/{}",
            "GET /api/connections/abc",
        ];
        let mut ranked: Vec<(RouteMatch, &str)> = routes.iter().filter_map(|r| route_match(&request, &key(r)).map(|m| (m, *r))).collect();
        ranked.sort_by_key(|(m, _)| std::cmp::Reverse(*m));
        let order: Vec<&str> = ranked.iter().map(|(_, r)| *r).collect();
        assert_eq!(
            order,
            [
                "GET /api/connections/abc",
                "GET /api/connections/{}",
                "ANY /api/connections/{}",
                "POST /api/connections/{}",
                "ANY /api/{}/{}",
            ]
        );

        let any = RouteKey::new(None, RoutePattern::from_url("/api/connections/abc"));
        let m = route_match(&any, &key("POST /api/connections/{}")).unwrap();
        assert_eq!(m.method, MethodMatch::Unspecified);
    }
}
