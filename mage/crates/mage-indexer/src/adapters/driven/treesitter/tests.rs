//! Parser behavior tests: golden `ParsedBlob` fixtures per language, the literal-context
//! contract consumed by the error-site classifier (plus an end-to-end check through
//! `app::index::errors::classify`), limits and status handling.

use std::path::Path;
use std::time::Duration;

use mage_core::domain::{BlobOid, Language, RepoPath};
use mage_core::ports::knowledge::{
    AppFact, ArgSummary, BlobInput, Construct, ErrorSiteKind, FrameworkId, Hole, LitPart, LiteralSite, LogLevel, NameRef, ParseLimits, ParsedBlob,
    RefKind, StageStatus, SymbolKind,
};

use super::{PARSER_REV, TreeSitterConfig, TreeSitterParser};
use crate::app::index::errors::classify;
use crate::ports::parser::{ParseError, Parser};

fn limits() -> ParseLimits {
    ParseLimits { max_bytes: 1 << 20, timeout: Duration::from_secs(10), max_symbols: 10_000, max_refs: 50_000, max_literals: 10_000 }
}

fn blob(path: &str, lang: Language, src: &str) -> BlobInput {
    BlobInput { oid: BlobOid::from_bytes([0; 20]), path: RepoPath::new(path).unwrap(), lang, bytes: src.as_bytes().to_vec() }
}

fn parser() -> TreeSitterParser {
    TreeSitterParser::new(TreeSitterConfig { parse_threads: 2 }).unwrap()
}

async fn parse_with(path: &str, lang: Language, src: &str, limits: ParseLimits) -> Result<ParsedBlob, ParseError> {
    parser().parse(vec![blob(path, lang, src)], limits).await.pop().unwrap()
}

async fn parse(path: &str, lang: Language, src: &str) -> ParsedBlob {
    parse_with(path, lang, src, limits()).await.unwrap()
}

/// Parts rendered as text, holes as `{interp:expr}` / `{concat:expr}`.
fn render(site: &LiteralSite) -> String {
    site.parts
        .iter()
        .map(|p| match p {
            LitPart::Text(t) => t.clone(),
            LitPart::Hole(Hole::Interp { expr }) => format!("{{interp:{expr}}}"),
            LitPart::Hole(Hole::Concat { expr }) => format!("{{concat:{expr}}}"),
        })
        .collect()
}

/// The site whose rendered parts start with `prefix`.
fn site<'a>(parsed: &'a ParsedBlob, prefix: &str) -> &'a LiteralSite {
    parsed
        .literals
        .iter()
        .find(|s| render(s).starts_with(prefix))
        .unwrap_or_else(|| panic!("no literal starting with {prefix:?}; have {:?}", parsed.literals.iter().map(render).collect::<Vec<_>>()))
}

fn text(s: &str) -> LitPart {
    LitPart::Text(s.to_string())
}

fn interp(e: &str) -> LitPart {
    LitPart::Hole(Hole::Interp { expr: e.to_string() })
}

fn concat(e: &str) -> LitPart {
    LitPart::Hole(Hole::Concat { expr: e.to_string() })
}

fn ident(s: &str) -> ArgSummary {
    ArgSummary::Ident(s.to_string())
}

fn symbol_ord(parsed: &ParsedBlob, name: &str) -> Option<u32> {
    parsed.symbols.iter().find(|s| s.name == name).map(|s| s.ord)
}

fn refs_of(parsed: &ParsedBlob, kind: RefKind) -> Vec<(&str, Option<&str>)> {
    parsed.refs.iter().filter(|r| r.kind == kind).map(|r| (r.name.as_str(), r.qualifier.as_deref())).collect()
}

// ---------------------------------------------------------------------------------------------
// Golden fixtures

const FIXTURES: &[(&str, Language)] = &[
    ("go/server.go", Language::Go),
    ("typescript/api.ts", Language::TypeScript),
    ("tsx/Banner.tsx", Language::Tsx),
    ("javascript/server.js", Language::JavaScript),
    ("python/service.py", Language::Python),
    ("rust/lib.rs", Language::Rust),
    ("clojure/handler.clj", Language::Clojure),
    ("cljs/events.cljs", Language::ClojureScript),
];

/// `<dir>/<stem>.expected.json` next to each fixture; `UPDATE_GOLDEN=1` rewrites them.
#[tokio::test]
async fn golden_fixtures_match_expected_parsed_blobs() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/treesitter");
    let update = std::env::var_os("UPDATE_GOLDEN").is_some();
    let mut failures = Vec::new();
    for (rel, lang) in FIXTURES {
        let source = std::fs::read_to_string(dir.join(rel)).unwrap();
        let parsed = parse(rel, *lang, &source).await;
        assert_eq!(parsed.status, StageStatus::Ok, "{rel} has syntax errors");
        let actual = serde_json::to_string_pretty(&parsed).unwrap() + "\n";
        let source_path = dir.join(rel);
        let stem = source_path.file_stem().and_then(|s| s.to_str()).unwrap();
        let expected_path = source_path.with_file_name(format!("{stem}.expected.json"));
        if update {
            std::fs::write(&expected_path, &actual).unwrap();
            continue;
        }
        let expected = std::fs::read_to_string(&expected_path).unwrap_or_default();
        if expected != actual {
            let line = expected.lines().zip(actual.lines()).position(|(e, a)| e != a).unwrap_or_else(|| expected.lines().count().min(actual.lines().count()));
            let window = |s: &str| s.lines().skip(line.saturating_sub(3)).take(7).collect::<Vec<_>>().join("\n");
            failures.push(format!(
                "{} differs at line {}:\n--- expected\n{}\n+++ actual\n{}",
                expected_path.display(),
                line + 1,
                window(&expected),
                window(&actual)
            ));
        }
    }
    assert!(failures.is_empty(), "golden mismatch (rerun with UPDATE_GOLDEN=1 to accept):\n\n{}", failures.join("\n\n"));
}

// ---------------------------------------------------------------------------------------------
// Literal-context contract

#[tokio::test]
async fn go_literal_contexts() {
    let src = r#"package api

func handle(w http.ResponseWriter, c *gin.Context, name string, err error) error {
	http.Error(w, "session expired", http.StatusUnauthorized)
	c.JSON(http.StatusNotFound, gin.H{"message": "item is gone"})
	panic(fmt.Sprintf("bad state %d", 3))
	log.Printf("entry " + name + " missing\t(%s)", "x")
	return fmt.Errorf("failed fetching connection %s: %w", name, err)
}
"#;
    let p = parse("api/handle.go", Language::Go, src).await;
    let handle = symbol_ord(&p, "handle");

    let s = site(&p, "failed fetching connection");
    assert_eq!(s.parts, vec![text("failed fetching connection %s: %w")]);
    assert_eq!(s.context.callee.as_deref(), Some("fmt.Errorf"));
    assert_eq!(s.context.arg_index, Some(0));
    assert_eq!(s.context.siblings, vec![ident("name"), ident("err")]);
    assert_eq!(s.context.construct, Construct::Call);
    assert_eq!(s.enclosing, handle);
    assert_eq!(s.pos, (8, 20));

    let s = site(&p, "session expired");
    assert_eq!(s.context.callee.as_deref(), Some("http.Error"));
    assert_eq!(s.context.arg_index, Some(1));
    assert_eq!(s.context.siblings, vec![ident("w"), ident("http.StatusUnauthorized")]);

    let s = site(&p, "item is gone");
    assert_eq!(s.context.callee.as_deref(), Some("c.JSON"));
    assert_eq!(s.context.map_key.as_deref(), Some("message"));
    assert_eq!(s.context.arg_index, Some(1));
    assert_eq!(s.context.siblings, vec![ident("http.StatusNotFound")]);
    assert_eq!(s.context.construct, Construct::Call);
    let key = p.literals.iter().find(|l| render(l) == "message").unwrap();
    assert_eq!(key.context.callee, None, "map keys are not call arguments");

    // fmt.Sprintf is transparent: the context is panic's.
    let s = site(&p, "bad state");
    assert_eq!(s.parts, vec![text("bad state %d")]);
    assert_eq!(s.context.callee.as_deref(), Some("panic"));
    assert_eq!(s.context.construct, Construct::Panic);

    // Concatenation flattened into one site, escapes decoded.
    let s = site(&p, "entry ");
    assert_eq!(s.parts, vec![text("entry "), concat("name"), text(" missing\t(%s)")]);
    assert_eq!(s.context.callee.as_deref(), Some("log.Printf"));
    assert_eq!(s.context.siblings, vec![ArgSummary::Str]);
    assert_eq!(p.literals.iter().filter(|l| render(l).contains("missing")).count(), 1);
}

#[tokio::test]
async fn ts_literal_contexts() {
    let src = r#"export function load(id: string) {
  throw new Error(`Connection ${id} not found`);
}
export const handler = (req, res) => {
  res.status(404).json({ message: "user not found" });
  t("errors.connection_not_found");
  console.error("failed " + id + ' in \u0041PI');
  return Promise.reject(new TypeError("not implemented"));
};
"#;
    let p = parse("web/api.ts", Language::TypeScript, src).await;

    let s = site(&p, "Connection ");
    assert_eq!(s.parts, vec![text("Connection "), interp("id"), text(" not found")]);
    assert_eq!(s.context.construct, Construct::Throw);
    assert_eq!(s.context.exception_type.as_deref(), Some("Error"));
    assert_eq!(s.context.callee.as_deref(), Some("Error"));
    assert_eq!(s.context.arg_index, Some(0));
    assert_eq!(s.enclosing, symbol_ord(&p, "load"));

    let s = site(&p, "user not found");
    assert_eq!(s.context.callee.as_deref(), Some("res.status(404).json"));
    assert_eq!(s.context.map_key.as_deref(), Some("message"));
    assert_eq!(s.context.arg_index, Some(0));
    assert_eq!(s.context.construct, Construct::Call);
    assert_eq!(s.enclosing, symbol_ord(&p, "handler"));

    let s = site(&p, "errors.connection_not_found");
    assert_eq!(s.context.callee.as_deref(), Some("t"));
    assert_eq!(s.context.arg_index, Some(0));

    let s = site(&p, "failed ");
    assert_eq!(s.parts, vec![text("failed "), concat("id"), text(" in API")]);
    assert_eq!(s.context.callee.as_deref(), Some("console.error"));

    let s = site(&p, "not implemented");
    assert_eq!(s.context.callee.as_deref(), Some("TypeError"));
    assert_eq!(s.context.exception_type.as_deref(), Some("TypeError"));
    assert_eq!(s.context.construct, Construct::Call);
}

#[tokio::test]
async fn tsx_attributes_and_render_refs() {
    let src = r#"export function Banner() {
  return <Alert><Trans i18nKey="errors.retry_later" /><Icons.Warning title={"careful now"} /><div /></Alert>;
}
"#;
    let p = parse("web/Banner.tsx", Language::Tsx, src).await;
    let s = site(&p, "errors.retry_later");
    assert_eq!(s.context.construct, Construct::Attribute);
    assert_eq!(s.context.attribute.as_deref(), Some("i18nKey"));
    assert_eq!(s.context.callee, None);
    let s = site(&p, "careful now");
    assert_eq!(s.context.attribute.as_deref(), Some("title"));
    assert_eq!(refs_of(&p, RefKind::Render), vec![("Alert", None), ("Trans", None), ("Warning", Some("Icons"))]);
}

#[tokio::test]
async fn python_literal_contexts() {
    let src = r#"def connect(n, m, host, name):
    raise ValueError("bad value")
    logger.error("retry %d/%d failed for %s", n, m, host)
    raise HTTPException(status_code=404, detail="item not found")
    s = f"hello {name!r} {{lit}}\n"
    t = "x {} of {}".format(name, n)
    u = "y %s here" % name
    v = "implicit " "join"
"#;
    let p = parse("svc/connect.py", Language::Python, src).await;

    let s = site(&p, "bad value");
    assert_eq!(s.context.construct, Construct::Raise);
    assert_eq!(s.context.exception_type.as_deref(), Some("ValueError"));
    assert_eq!(s.context.callee.as_deref(), Some("ValueError"));

    let s = site(&p, "retry");
    assert_eq!(s.parts, vec![text("retry %d/%d failed for %s")]);
    assert_eq!(s.context.callee.as_deref(), Some("logger.error"));
    assert_eq!(s.context.siblings, vec![ident("n"), ident("m"), ident("host")]);
    assert_eq!(s.context.construct, Construct::Call);

    let s = site(&p, "item not found");
    assert_eq!(s.context.callee.as_deref(), Some("HTTPException"));
    assert_eq!(s.context.map_key.as_deref(), Some("detail"));
    assert_eq!(s.context.arg_index, Some(1));
    assert_eq!(s.context.siblings, vec![ArgSummary::Int(404)]);
    assert_eq!(s.context.construct, Construct::Raise);
    assert_eq!(s.context.exception_type.as_deref(), Some("HTTPException"));

    let s = site(&p, "hello ");
    assert_eq!(s.parts, vec![text("hello "), interp("name"), text(" {lit}\n")]);

    let s = site(&p, "x {}");
    assert_eq!(s.parts, vec![text("x {} of {}")]);
    assert_eq!(s.context.callee, None);
    assert_eq!(s.context.construct, Construct::Other);

    assert_eq!(site(&p, "y %s").parts, vec![text("y %s here")]);
    assert_eq!(site(&p, "implicit").parts, vec![text("implicit join")]);
}

#[tokio::test]
async fn rust_literal_contexts() {
    let src = r#"#[derive(thiserror::Error)]
enum E {
    #[error("invalid token: {0}")]
    Invalid(String),
}
fn renew(res: Result<u8, E>, y: u8) -> anyhow::Result<()> {
    bail!("lease {} expired", y);
    tracing::error!("lost lease {}", StatusCode::NOT_FOUND);
    let v = res.expect("value missing");
    let b = Body { message: "lease conflict".to_string(), code: 409 };
    panic!("boom \"now\"\n");
}
"#;
    let p = parse("src/lease.rs", Language::Rust, src).await;

    let s = site(&p, "invalid token");
    assert_eq!(s.context.construct, Construct::Attribute);
    assert_eq!(s.context.attribute.as_deref(), Some("error"));
    assert_eq!(s.context.callee, None);
    assert_eq!(s.context.arg_index, Some(0));
    assert_eq!(s.enclosing, symbol_ord(&p, "E"));

    let s = site(&p, "lease {}");
    assert_eq!(s.context.callee.as_deref(), Some("bail!"));
    assert_eq!(s.context.arg_index, Some(0));
    assert_eq!(s.context.siblings, vec![ident("y")]);
    assert_eq!(s.context.construct, Construct::Call);

    let s = site(&p, "lost lease");
    assert_eq!(s.context.callee.as_deref(), Some("tracing::error!"));
    assert_eq!(s.context.siblings, vec![ident("StatusCode::NOT_FOUND")]);

    let s = site(&p, "value missing");
    assert_eq!(s.context.callee.as_deref(), Some("res.expect"));
    assert_eq!(s.context.arg_index, Some(0));

    let s = site(&p, "lease conflict");
    assert_eq!(s.context.map_key.as_deref(), Some("message"));
    assert_eq!(s.context.siblings, vec![ArgSummary::Int(409)]);
    assert_eq!(s.context.construct, Construct::MapValue);

    let s = site(&p, "boom");
    assert_eq!(s.parts, vec![text("boom \"now\"\n")]);
    assert_eq!(s.context.callee.as_deref(), Some("panic!"));
    assert_eq!(s.context.construct, Construct::Panic);
}

#[tokio::test]
async fn cljs_literal_contexts() {
    let src = r#"(ns webapp.plugins
  (:require [re-frame.core :as rf]
            [taoensso.timbre :as log]))

(defn enable! [plugin-name]
  (throw (ex-info (str "Plugin " plugin-name " is not enabled") {})))

(defn notify []
  (log/error "sync failed badly")
  (rf/dispatch [:show-snackbar {:level :error :text "Failed to connect"}]))

(defn ring-handler [req] {:status 404 :body {:message "user not found"}})

(defn boom [] (throw (js/Error. "boom happened")) (format "id %s missing" 1))
"#;
    let p = parse("src/webapp/plugins.cljs", Language::ClojureScript, src).await;

    let s = site(&p, "Plugin ");
    assert_eq!(s.parts, vec![text("Plugin "), concat("plugin-name"), text(" is not enabled")]);
    assert_eq!(s.context.callee.as_deref(), Some("ex-info"));
    assert_eq!(s.context.arg_index, Some(0));
    assert_eq!(s.context.construct, Construct::Throw);
    assert_eq!(s.context.exception_type.as_deref(), Some("ex-info"));
    assert_eq!(s.context.siblings, vec![ArgSummary::Other]);
    assert_eq!(s.enclosing, symbol_ord(&p, "enable!"));
    assert_eq!(p.literals.iter().filter(|l| render(l).contains("is not enabled")).count(), 1);

    let s = site(&p, "sync failed badly");
    assert_eq!(s.context.callee.as_deref(), Some("log/error"));
    assert_eq!(s.context.arg_index, Some(0));
    assert_eq!(s.context.construct, Construct::Call);

    let s = site(&p, "Failed to connect");
    assert_eq!(s.context.callee.as_deref(), Some("rf/dispatch"));
    assert_eq!(s.context.map_key.as_deref(), Some(":text"));
    assert_eq!(s.context.arg_index, Some(0));
    assert!(s.context.siblings.contains(&ident(":error")), "{:?}", s.context.siblings);

    let s = site(&p, "user not found");
    assert_eq!(s.context.callee, None);
    assert_eq!(s.context.map_key.as_deref(), Some(":message"));
    assert_eq!(s.context.siblings, vec![ArgSummary::Int(404)]);
    assert_eq!(s.context.construct, Construct::MapValue);

    let s = site(&p, "boom happened");
    assert_eq!(s.context.callee.as_deref(), Some("js/Error."));
    assert_eq!(s.context.exception_type.as_deref(), Some("Error"));
    assert_eq!(s.context.construct, Construct::Throw);

    // `format` is transparent: no call context outside it.
    let s = site(&p, "id %s");
    assert_eq!(s.parts, vec![text("id %s missing")]);
    assert_eq!(s.context.callee, None);
}

// ---------------------------------------------------------------------------------------------
// Symbols and refs

#[tokio::test]
async fn cljs_namespaces_registrations_and_dispatches() {
    let src = r#"(ns webapp.connections.events
  (:require [re-frame.core :as rf]
            [webapp.plugins.core :as plugins]
            [reagent.core :refer [atom]]))

(rf/reg-event-fx ::fetch (fn [_ _] {:fx [[:dispatch [::loaded]]] :dispatch [:other/evt]}))
(rf/reg-sub :connections/list (fn [db] db))

(defn view
  "Renders."
  []
  (rf/dispatch [::plugins/enable 1])
  @(rf/subscribe [::list])
  (plugins/enabled? 1))

(defn- helper [] (atom nil))
"#;
    let p = parse("src/webapp/connections/events.cljs", Language::ClojureScript, src).await;
    let symbols: Vec<(&str, &str, SymbolKind, bool)> =
        p.symbols.iter().map(|s| (s.name.as_str(), s.qualified.as_str(), s.kind, s.exported)).collect();
    assert_eq!(
        symbols,
        vec![
            ("webapp.connections.events", "webapp.connections.events", SymbolKind::Module, true),
            (":webapp.connections.events/fetch", ":webapp.connections.events/fetch", SymbolKind::Registered, true),
            (":connections/list", ":connections/list", SymbolKind::Registered, true),
            ("view", "webapp.connections.events/view", SymbolKind::Function, true),
            ("helper", "webapp.connections.events/helper", SymbolKind::Function, false),
        ]
    );
    let view = &p.symbols[3];
    assert_eq!(view.doc.as_deref(), Some("Renders."));
    assert_eq!(view.name_pos, (9, 7));
    assert_eq!((view.span.start(), view.span.end()), (9, 14));
    assert_eq!(view.signature.as_deref(), Some("(defn view"));

    assert_eq!(
        refs_of(&p, RefKind::Import),
        vec![("re-frame.core", Some("rf")), ("webapp.plugins.core", Some("plugins")), ("reagent.core", None)]
    );
    assert_eq!(
        refs_of(&p, RefKind::Dispatch),
        vec![
            (":webapp.connections.events/loaded", None),
            (":other/evt", None),
            (":webapp.plugins.core/enable", None),
            (":webapp.connections.events/list", None),
        ]
    );
    let calls = refs_of(&p, RefKind::Call);
    assert!(calls.contains(&("dispatch", Some("re-frame.core"))), "{calls:?}");
    assert!(calls.contains(&("enabled?", Some("webapp.plugins.core"))), "{calls:?}");
    assert!(calls.contains(&("atom", Some("reagent.core"))), "{calls:?}");
    assert!(!calls.iter().any(|(n, _)| *n == "defn" || *n == "reg-sub"), "top-level heads are not refs: {calls:?}");
    let enabled: &NameRef = p.refs.iter().find(|r| r.name == "enabled?").unwrap();
    assert_eq!(enabled.enclosing, Some(view.ord));
}

#[tokio::test]
async fn go_symbols_and_refs() {
    let src = r#"// Package api serves.
package api

import (
	"fmt"
	h "net/http"
)

// Server serves.
// Twice.
type Server struct{}

type Store interface{ Get() }

type ID = string

const A, B = 1, 2

func (s *Server) Connect(name string) error { return fmt.Errorf("x %s", name) }

func helper(w h.ResponseWriter) {}
"#;
    let p = parse("api/server.go", Language::Go, src).await;
    let symbols: Vec<(&str, &str, SymbolKind, Option<u32>, bool)> =
        p.symbols.iter().map(|s| (s.name.as_str(), s.qualified.as_str(), s.kind, s.container, s.exported)).collect();
    assert_eq!(
        symbols,
        vec![
            ("api", "api", SymbolKind::Module, None, true),
            ("Server", "Server", SymbolKind::Struct, None, true),
            ("Store", "Store", SymbolKind::Interface, None, true),
            ("ID", "ID", SymbolKind::TypeAlias, None, true),
            ("A", "A", SymbolKind::Constant, None, true),
            ("B", "B", SymbolKind::Constant, None, true),
            ("Connect", "Server.Connect", SymbolKind::Method, Some(1), true),
            ("helper", "helper", SymbolKind::Function, None, false),
        ]
    );
    let server = &p.symbols[1];
    assert_eq!(server.doc.as_deref(), Some("Server serves.\nTwice."));
    assert_eq!(server.signature.as_deref(), Some("type Server struct{}"));
    assert_eq!((server.span.start(), server.span.end()), (11, 11));
    assert_eq!(refs_of(&p, RefKind::Import), vec![("fmt", Some("fmt")), ("net/http", Some("h"))]);
    assert!(refs_of(&p, RefKind::Call).contains(&("Errorf", Some("fmt"))));
    let types = refs_of(&p, RefKind::Type);
    assert!(types.contains(&("ResponseWriter", Some("h"))), "{types:?}");
    assert!(types.contains(&("Server", None)), "receiver type is a ref: {types:?}");
    assert!(!types.iter().any(|(n, _)| *n == "string" || *n == "error"), "predeclared types are not refs: {types:?}");
    assert_eq!(types.iter().filter(|(n, _)| *n == "Server").count(), 1, "definition names are not refs");
}

#[tokio::test]
async fn go_test_functions_in_test_files() {
    let src = "package api\n\nfunc TestConnect(t *testing.T) {}\nfunc helper() {}\n";
    let p = parse("api/server_test.go", Language::Go, src).await;
    assert_eq!(p.symbols[1].kind, SymbolKind::Test);
    assert_eq!(p.symbols[2].kind, SymbolKind::Function);
    let p = parse("api/server.go", Language::Go, src).await;
    assert_eq!(p.symbols[1].kind, SymbolKind::Function);
}

#[tokio::test]
async fn python_symbols() {
    let src = r#"import os.path as osp
from .x import y

MAX_RETRY = 3
_cache = {}

class Service:
    """Service doc."""

    @property
    def name(self):
        """Name doc."""
        return "x"

def _helper():
    pass
"#;
    let p = parse("svc/service.py", Language::Python, src).await;
    let symbols: Vec<(&str, &str, SymbolKind, bool)> = p.symbols.iter().map(|s| (s.name.as_str(), s.qualified.as_str(), s.kind, s.exported)).collect();
    assert_eq!(
        symbols,
        vec![
            ("MAX_RETRY", "MAX_RETRY", SymbolKind::Constant, true),
            ("_cache", "_cache", SymbolKind::Variable, false),
            ("Service", "Service", SymbolKind::Class, true),
            ("name", "Service.name", SymbolKind::Method, true),
            ("_helper", "_helper", SymbolKind::Function, false),
        ]
    );
    let method = &p.symbols[3];
    assert_eq!(method.doc.as_deref(), Some("Name doc."));
    assert_eq!(method.signature.as_deref(), Some("@property\n    def name(self):"));
    assert_eq!(method.span.start(), 10, "span starts at the decorator");
    assert_eq!(method.container, Some(2));
    assert_eq!(p.symbols[2].doc.as_deref(), Some("Service doc."));
    assert_eq!(refs_of(&p, RefKind::Import), vec![("os.path", Some("osp")), (".x", None)]);
}

#[tokio::test]
async fn rust_symbols() {
    let src = r#"use std::collections::HashMap as Map;

/// Lease doc.
#[derive(Debug)]
pub struct Lease { id: u64 }

impl Lease {
    /// Renews.
    pub fn renew(&self) {}
    fn private(&self) {}
}

pub(crate) trait Store { fn get(&self); }

#[macro_export]
macro_rules! m { () => {} }

#[tokio::test]
async fn renews() {}
"#;
    let p = parse("src/lease.rs", Language::Rust, src).await;
    let symbols: Vec<(&str, &str, SymbolKind, Option<u32>, bool)> =
        p.symbols.iter().map(|s| (s.name.as_str(), s.qualified.as_str(), s.kind, s.container, s.exported)).collect();
    assert_eq!(
        symbols,
        vec![
            ("Lease", "Lease", SymbolKind::Struct, None, true),
            ("renew", "Lease::renew", SymbolKind::Method, Some(0), true),
            ("private", "Lease::private", SymbolKind::Method, Some(0), false),
            ("Store", "Store", SymbolKind::Trait, None, false),
            ("get", "Store::get", SymbolKind::Method, Some(3), false),
            ("m", "m", SymbolKind::Macro, None, true),
            ("renews", "renews", SymbolKind::Test, None, false),
        ]
    );
    assert_eq!(p.symbols[0].doc.as_deref(), Some("Lease doc."));
    assert_eq!(p.symbols[1].doc.as_deref(), Some("Renews."));
    assert_eq!(refs_of(&p, RefKind::Import), vec![("std::collections::HashMap", Some("Map"))]);
}

#[tokio::test]
async fn ts_symbols_and_imports() {
    let src = r#"import React from "react";
import * as api from "./api";
import { a } from "./a";

/** Loads. */
export async function load() {}
export const Comp = () => null;
class Hidden { run() {} }
interface I {}
type T = string;
enum E { A }
"#;
    let p = parse("web/x.ts", Language::TypeScript, src).await;
    let symbols: Vec<(&str, &str, SymbolKind, bool)> = p.symbols.iter().map(|s| (s.name.as_str(), s.qualified.as_str(), s.kind, s.exported)).collect();
    assert_eq!(
        symbols,
        vec![
            ("load", "load", SymbolKind::Function, true),
            ("Comp", "Comp", SymbolKind::Function, true),
            ("Hidden", "Hidden", SymbolKind::Class, false),
            ("run", "Hidden.run", SymbolKind::Method, false),
            ("I", "I", SymbolKind::Interface, false),
            ("T", "T", SymbolKind::TypeAlias, false),
            ("E", "E", SymbolKind::Enum, false),
        ]
    );
    assert_eq!(p.symbols[0].doc.as_deref(), Some("Loads."));
    assert_eq!(p.symbols[0].signature.as_deref(), Some("export async function load() {}"));
    assert_eq!(refs_of(&p, RefKind::Import), vec![("react", Some("React")), ("./api", Some("api")), ("./a", None)]);
}

// ---------------------------------------------------------------------------------------------
// Status, limits, ordering

#[tokio::test]
async fn syntax_errors_give_partial_status_and_keep_other_facts() {
    let src = "package api\n\nfunc broken( {\n\nfunc Good() { log.Print(\"still here\") }\n";
    let p = parse("api/broken.go", Language::Go, src).await;
    match p.status {
        StageStatus::Partial { error_nodes } => assert!(error_nodes >= 1),
        other => panic!("expected Partial, got {other:?}"),
    }
    assert!(p.symbols.iter().any(|s| s.name == "api"));
}

#[tokio::test]
async fn size_cap_is_too_large() {
    let limits = ParseLimits { max_bytes: 8, ..limits() };
    assert_eq!(parse_with("a.go", Language::Go, "package api\n", limits).await, Err(ParseError::TooLarge));
}

#[tokio::test]
async fn expired_deadline_is_timeout() {
    let src = "package api\n".to_string() + &"func f() { g(\"some text\") }\n".repeat(20_000);
    let limits = ParseLimits { timeout: Duration::ZERO, ..limits() };
    assert_eq!(parse_with("a.go", Language::Go, &src, limits).await, Err(ParseError::Timeout));
}

#[tokio::test]
async fn unsupported_language() {
    assert_eq!(parse_with("A.java", Language::Java, "class A {}", limits()).await, Err(ParseError::Unsupported(Language::Java)));
}

#[tokio::test]
async fn caps_keep_lowest_ords_and_clamp_enclosing() {
    let src = "package api\n\nfunc A() { f(\"first one\") }\nfunc B() { f(\"second one\") }\n";
    let capped = ParseLimits { max_symbols: 2, max_refs: 1, max_literals: 1, ..limits() };
    let p = parse_with("a.go", Language::Go, src, capped).await.unwrap();
    assert_eq!(p.symbols.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), vec!["api", "A"]);
    assert_eq!(p.refs.len(), 1);
    assert_eq!(p.literals.iter().map(render).collect::<Vec<_>>(), vec!["first one"]);
    assert_eq!(p.literals[0].ord, 0);

    // A literal inside a dropped symbol is not attributed to it.
    let capped = ParseLimits { max_symbols: 2, ..limits() };
    let p = parse_with("a.go", Language::Go, src, capped).await.unwrap();
    assert_eq!(site(&p, "second").enclosing, None);
}

#[tokio::test]
async fn batch_results_keep_input_order() {
    let p = parser();
    let blobs = vec![
        blob("a.go", Language::Go, "package a\n"),
        blob("b.java", Language::Java, "class B {}"),
        blob("c.py", Language::Python, "def c(): pass\n"),
        blob("d.rs", Language::Rust, "fn d() {}\n"),
    ];
    let results = p.parse(blobs, limits()).await;
    assert_eq!(results.len(), 4);
    assert_eq!(results[0].as_ref().unwrap().symbols[0].name, "a");
    assert_eq!(results[1], Err(ParseError::Unsupported(Language::Java)));
    assert_eq!(results[2].as_ref().unwrap().symbols[0].name, "c");
    assert_eq!(results[3].as_ref().unwrap().symbols[0].name, "d");
}

#[tokio::test]
async fn deterministic_output() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/treesitter");
    let src = std::fs::read_to_string(dir.join("cljs/events.cljs")).unwrap();
    let first = parse("events.cljs", Language::ClojureScript, &src).await;
    let second = parse("events.cljs", Language::ClojureScript, &src).await;
    assert_eq!(first, second);
}

#[test]
fn revs_and_languages() {
    let p = parser();
    assert_eq!(p.rev(), PARSER_REV);
    assert_eq!(p.app_rev(), super::app_map::APP_MAP_REV);
    assert_eq!(p.languages().len(), 9);
    assert!(p.languages().contains(&Language::Cljc));
}

// ---------------------------------------------------------------------------------------------
// App map wiring

#[tokio::test]
async fn frameworks_and_app_facts_run_through_the_registry() {
    let p = parser();
    let gomod = blob("svc/go.mod", Language::Other, "module example.com/svc\n\nrequire github.com/gin-gonic/gin v1.9.1\n");
    let scopes = p.detect_frameworks(vec![gomod]).await.unwrap();
    assert!(scopes.iter().any(|s| s.framework == FrameworkId::Gin && s.root.as_ref().map(RepoPath::as_str) == Some("svc")), "{scopes:?}");

    let src = "package svc\n\nfunc routes(r *gin.Engine) {\n\tr.GET(\"/items/:id\", getItem)\n}\n";
    let mut facts = p.app_facts(vec![(blob("svc/routes.go", Language::Go, src), vec![FrameworkId::Gin])], limits()).await;
    let facts = facts.pop().unwrap().unwrap();
    let route = facts.iter().find(|f| matches!(f.fact, AppFact::RouteDecl { .. })).unwrap_or_else(|| panic!("no route in {facts:?}"));
    assert_eq!(route.framework, FrameworkId::Gin);
    assert_eq!(route.line, 4);
    assert_eq!(route.enclosing, Some(1), "enclosing = ord of `routes`");

    let too_large = ParseLimits { max_bytes: 4, ..limits() };
    let mut facts = p.app_facts(vec![(blob("svc/routes.go", Language::Go, src), vec![FrameworkId::Gin])], too_large).await;
    assert_eq!(facts.pop().unwrap(), Err(ParseError::TooLarge));
}

// ---------------------------------------------------------------------------------------------
// End to end through the error-site classifier (`app::index::errors`)

struct Expect {
    kind: ErrorSiteKind,
    template: &'static str,
    http_status: Option<u16>,
    level: Option<LogLevel>,
    i18n_key: Option<&'static str>,
}

async fn classified(path: &str, lang: Language, src: &str, prefix: &str, want: Expect) {
    let p = parse(path, lang, src).await;
    let s = site(&p, prefix);
    let draft = classify(lang, s).unwrap_or_else(|| panic!("{path}: {prefix:?} not classified; context {:?}", s.context));
    assert_eq!(draft.kind, want.kind, "{path}: {prefix:?} {:?}", s.context);
    assert_eq!(draft.template.to_string(), want.template, "{path}: {prefix:?}");
    assert_eq!(draft.http_status, want.http_status, "{path}: {prefix:?} {:?}", s.context);
    assert_eq!(draft.level, want.level, "{path}: {prefix:?}");
    assert_eq!(draft.i18n_key.as_deref(), want.i18n_key, "{path}: {prefix:?}");
}

fn expect(kind: ErrorSiteKind, template: &'static str) -> Expect {
    Expect { kind, template, http_status: None, level: None, i18n_key: None }
}

#[tokio::test]
async fn classifier_end_to_end() {
    use ErrorSiteKind::*;
    let go = "package api\n\nfunc f(w http.ResponseWriter, c *gin.Context, name string, err error) error {\n\thttp.Error(w, \"session expired\", http.StatusUnauthorized)\n\tc.JSON(http.StatusNotFound, gin.H{\"message\": \"not found here\"})\n\treturn fmt.Errorf(\"failed fetching connection %s: %w\", name, err)\n}\n";
    classified("api/f.go", Language::Go, go, "failed fetching", expect(Throw, "failed fetching connection {}: {w}")).await;
    classified("api/f.go", Language::Go, go, "session expired", Expect { http_status: Some(401), ..expect(HttpResponse, "session expired") }).await;
    classified("api/f.go", Language::Go, go, "not found here", Expect { http_status: Some(404), ..expect(HttpResponse, "not found here") }).await;

    let ts = "export function load(id: string) {\n  throw new Error(`Connection ${id} not found`);\n}\nexport const h = (req, res) => {\n  res.status(404).json({message: \"user not found\"});\n};\n";
    classified("web/a.ts", Language::TypeScript, ts, "Connection", expect(Throw, "Connection {} not found")).await;
    classified("web/a.ts", Language::TypeScript, ts, "user not found", Expect { http_status: Some(404), ..expect(HttpResponse, "user not found") }).await;

    let tsx = "export function B() {\n  const { t } = useTranslation();\n  return <p>{t(\"errors.connection_not_found\")}</p>;\n}\n";
    classified("web/B.tsx", Language::Tsx, tsx, "errors.connection", Expect { i18n_key: Some("errors.connection_not_found"), ..expect(I18nUse, "") }).await;

    let py = "def f(n, m, host):\n    logger.error(\"retry %d/%d failed for %s\", n, m, host)\n    raise HTTPException(status_code=404, detail=\"item not found\")\n";
    classified("svc/f.py", Language::Python, py, "retry", Expect { level: Some(LogLevel::Error), ..expect(Log, "retry {int}/{int} failed for {}") }).await;
    classified("svc/f.py", Language::Python, py, "item not found", Expect { http_status: Some(404), ..expect(HttpResponse, "item not found") }).await;

    let rs = "#[derive(thiserror::Error)]\nenum E {\n    #[error(\"invalid token: {0}\")]\n    Invalid(String),\n}\nfn f(id: u64) {\n    tracing::warn!(\"lease lost for {}\", id);\n}\n";
    classified("src/e.rs", Language::Rust, rs, "invalid token", expect(Throw, "invalid token: {}")).await;
    classified("src/e.rs", Language::Rust, rs, "lease lost", Expect { level: Some(LogLevel::Warn), ..expect(Log, "lease lost for {}") }).await;

    let cljs = "(ns a.b (:require [re-frame.core :as rf] [taoensso.timbre :as log]))\n(defn f [plugin-name]\n  (log/error \"sync failed badly\")\n  (rf/dispatch [:show-snackbar {:level :error :text \"Failed to connect\"}])\n  (throw (ex-info (str \"Plugin \" plugin-name \" is not enabled\") {})))\n";
    classified("src/a/b.cljs", Language::ClojureScript, cljs, "Plugin", expect(Throw, "Plugin {} is not enabled")).await;
    classified("src/a/b.cljs", Language::ClojureScript, cljs, "sync failed", Expect { level: Some(LogLevel::Error), ..expect(Log, "sync failed badly") }).await;
    classified("src/a/b.cljs", Language::ClojureScript, cljs, "Failed to connect", expect(UiMessage, "Failed to connect")).await;
}
