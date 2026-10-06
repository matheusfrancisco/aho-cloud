//! Clojure / ClojureScript: re-frame (detection only), HTTP request maps, reitit route trees.

use mage_core::domain::{Language, RepoPath};
use mage_core::ports::knowledge::{AppFact, AppFactRecord, FrameworkId, HandlerRef, SymbolDef, UrlTemplate};
use tree_sitter::Node;

use super::detect::{CLJ_MANIFESTS, has, mentions};
use super::url::{clj_head, clj_string, clj_template, clj_values, concat};
use super::{AppExtractor, ManifestFile, enclosing_ord, line_of, record, snippet, text, walk_tree};

const CLJS: &[Language] = &[Language::ClojureScript, Language::Cljc];
const CLJ_ALL: &[Language] = &[Language::Clojure, Language::ClojureScript, Language::Cljc];

pub(super) struct ReFrame;
pub(super) struct CljsHttpMaps;
pub(super) struct Reitit;

impl AppExtractor for ReFrame {
    fn id(&self) -> FrameworkId {
        FrameworkId::ReFrame
    }
    fn languages(&self) -> &'static [Language] {
        CLJS
    }
    fn detect(&self, _root: Option<&RepoPath>, manifests: &[ManifestFile<'_>]) -> bool {
        mentions(manifests, CLJ_MANIFESTS, &["re-frame"])
    }
    /// Registrations and dispatches come from the symbol layer; nothing to extract here.
    fn extract(&self, _path: &RepoPath, _tree: &tree_sitter::Tree, _src: &[u8], _symbols: &[SymbolDef], _out: &mut Vec<AppFactRecord>) {}
}

/// Key/value pairs of a map literal (an odd trailing key is dropped).
fn pairs<'t>(map: Node<'t>) -> Vec<(Node<'t>, Node<'t>)> {
    clj_values(map).chunks_exact(2).map(|kv| (kv[0], kv[1])).collect()
}

fn is_key(node: Node<'_>, src: &[u8], key: &str) -> bool {
    node.kind() == "kwd_lit" && text(node, src) == key
}

/// Keyword name without its marker / namespace (`:get` → `get`).
fn kwd_name<'s>(node: Node<'_>, src: &'s [u8]) -> Option<&'s str> {
    (node.kind() == "kwd_lit").then(|| node.child_by_field_name("name").map(|n| text(n, src))).flatten()
}

// ---------------------------------------------------------------------------------------------
// HTTP request maps

impl AppExtractor for CljsHttpMaps {
    fn id(&self) -> FrameworkId {
        FrameworkId::CljsHttpMaps
    }
    fn languages(&self) -> &'static [Language] {
        CLJS
    }
    fn detect(&self, _root: Option<&RepoPath>, manifests: &[ManifestFile<'_>]) -> bool {
        has(manifests, "shadow-cljs.edn") || mentions(manifests, &["deps.edn", "project.clj"], &["clojurescript", "shadow-cljs"])
    }
    fn extract(&self, _path: &RepoPath, tree: &tree_sitter::Tree, src: &[u8], symbols: &[SymbolDef], out: &mut Vec<AppFactRecord>) {
        walk_tree(tree.root_node(), |map| {
            if map.kind() != "map_lit" {
                return;
            }
            let mut url = None;
            let mut method = None;
            for (key, value) in pairs(map) {
                if is_key(key, src, ":uri") || is_key(key, src, ":url") {
                    if value.kind() == "str_lit" || clj_head(value, src) == Some("str") {
                        url = Some(clj_template(value, src));
                    }
                } else if is_key(key, src, ":method") {
                    method = match value.kind() {
                        "kwd_lit" => kwd_name(value, src).map(str::to_ascii_uppercase),
                        _ => clj_string(value, src).map(|m| m.to_ascii_uppercase()),
                    };
                }
            }
            let Some(url) = url else { return };
            let fact = AppFact::HttpCall { method, url, client: client_key(map, src) };
            out.push(record(self.id(), fact, line_of(map), symbols));
        });
    }
}

/// The key whose value `map` is in its parent map (`":http-xhrio"`), else `"map"`.
fn client_key(map: Node<'_>, src: &[u8]) -> String {
    map.parent()
        .filter(|p| p.kind() == "map_lit")
        .and_then(|parent| pairs(parent).into_iter().find(|(_, v)| *v == map))
        .map_or_else(|| "map".to_owned(), |(k, _)| text(k, src).to_owned())
}

// ---------------------------------------------------------------------------------------------
// reitit

const REITIT_METHODS: &[&str] = &["get", "post", "put", "patch", "delete", "head", "options", "connect", "trace"];

impl AppExtractor for Reitit {
    fn id(&self) -> FrameworkId {
        FrameworkId::Reitit
    }
    fn languages(&self) -> &'static [Language] {
        CLJ_ALL
    }
    fn detect(&self, _root: Option<&RepoPath>, manifests: &[ManifestFile<'_>]) -> bool {
        mentions(manifests, CLJ_MANIFESTS, &["metosin/reitit"])
    }
    fn extract(&self, _path: &RepoPath, tree: &tree_sitter::Tree, src: &[u8], symbols: &[SymbolDef], out: &mut Vec<AppFactRecord>) {
        let mut reitit = ReititWalk { src, symbols, out };
        reitit.visit(tree.root_node());
    }
}

struct ReititWalk<'a, 'o> {
    src: &'a [u8],
    symbols: &'a [SymbolDef],
    out: &'o mut Vec<AppFactRecord>,
}

impl ReititWalk<'_, '_> {
    fn emit(&mut self, fact: AppFact, line: u32) {
        self.out.push(record(FrameworkId::Reitit, fact, line, self.symbols));
    }

    fn visit(&mut self, node: Node<'_>) {
        if self.route_path(node).is_some() {
            self.route(node, &UrlTemplate::default(), &[]);
            return;
        }
        let mut cursor = node.walk();
        let children: Vec<Node<'_>> = node.named_children(&mut cursor).collect();
        for child in children {
            self.visit(child);
        }
    }

    /// `["/path" …]` / `["" …]` → (path string node, path).
    fn route_path<'t>(&self, node: Node<'t>) -> Option<(Node<'t>, String)> {
        if node.kind() != "vec_lit" {
            return None;
        }
        let first = *clj_values(node).first()?;
        let path = clj_string(first, self.src)?;
        (path.is_empty() || path.starts_with('/')).then_some((first, path))
    }

    /// One route vector below `prefix`; `inherited` = middleware of the enclosing routes' data.
    fn route(&mut self, node: Node<'_>, prefix: &UrlTemplate, inherited: &[String]) {
        let Some((path_node, _)) = self.route_path(node) else { return };
        let path = concat(prefix, &clj_template(path_node, self.src));
        let line = line_of(path_node);
        let elements = clj_values(node);
        let mut middleware = inherited.to_vec();
        for data in elements.iter().filter(|e| e.kind() == "map_lit") {
            middleware.extend(self.middleware(&pairs(*data)));
        }
        for element in elements.into_iter().skip(1) {
            match element.kind() {
                "map_lit" => self.data(element, &path, line, &middleware),
                "vec_lit" if self.route_path(element).is_some() => self.route(element, &path, &middleware),
                "vec_lit" => {
                    for child in clj_values(element) {
                        self.route(child, &path, &middleware);
                    }
                }
                _ => {}
            }
        }
    }

    /// Route data: `:view` → page; `:get`/`:post`/… (and a bare `:handler`) → route declarations.
    /// `shared_mw` = middleware of this route and its parents.
    fn data(&mut self, map: Node<'_>, path: &UrlTemplate, line: u32, shared_mw: &[String]) {
        let src = self.src;
        let entries = pairs(map);
        for (key, value) in &entries {
            // Plain keywords only (`:get`, not `::get` / `:ns/get`).
            let plain = (key.kind() == "kwd_lit").then(|| text(*key, src).strip_prefix(':')).flatten();
            let Some(name) = plain.filter(|n| !n.starts_with(':') && !n.contains('/')) else {
                continue;
            };
            match name {
                "view" => {
                    let component = snippet(*value, src);
                    self.emit(AppFact::PageDecl { path: path.clone(), component }, line);
                }
                "handler" => {
                    let handler = self.handler(*value);
                    let fact = AppFact::RouteDecl { method: None, path: path.clone(), group: None, handler, middleware: shared_mw.to_vec() };
                    self.emit(fact, line);
                }
                m if REITIT_METHODS.contains(&m) => {
                    let (handler, mut middleware) = if value.kind() == "map_lit" {
                        let method_entries = pairs(*value);
                        let Some((_, h)) = method_entries.iter().find(|(k, _)| is_key(*k, src, ":handler")) else { continue };
                        (self.handler(*h), self.middleware(&method_entries))
                    } else {
                        (self.handler(*value), Vec::new())
                    };
                    let mut all = shared_mw.to_vec();
                    all.append(&mut middleware);
                    let fact = AppFact::RouteDecl {
                        method: Some(m.to_ascii_uppercase()),
                        path: path.clone(),
                        group: None,
                        handler,
                        middleware: all,
                    };
                    self.emit(fact, line);
                }
                _ => {}
            }
        }
    }

    /// Elements of a `:middleware [...]` entry.
    fn middleware(&self, entries: &[(Node<'_>, Node<'_>)]) -> Vec<String> {
        entries
            .iter()
            .find(|(k, _)| is_key(*k, self.src, ":middleware"))
            .map(|(_, v)| if v.kind() == "vec_lit" { clj_values(*v).into_iter().map(|m| snippet(m, self.src)).collect() } else { vec![snippet(*v, self.src)] })
            .unwrap_or_default()
    }

    /// `ns/f` → Named { f, ns }; `f` → Named { f }; `(fn …)` → Inline.
    fn handler(&self, node: Node<'_>) -> HandlerRef {
        let src = self.src;
        match node.kind() {
            "sym_lit" => {
                let name = node.child_by_field_name("name").map_or_else(|| text(node, src).to_owned(), |n| text(n, src).to_owned());
                let qualifier = node.child_by_field_name("namespace").map(|n| text(n, src).to_owned());
                HandlerRef::Named { name, qualifier }
            }
            "list_lit" if matches!(clj_head(node, src), Some("fn" | "fn*")) => {
                let line = line_of(node);
                HandlerRef::Inline { enclosing: enclosing_ord(self.symbols, line), line }
            }
            _ => HandlerRef::Named { name: snippet(node, src), qualifier: None },
        }
    }
}
