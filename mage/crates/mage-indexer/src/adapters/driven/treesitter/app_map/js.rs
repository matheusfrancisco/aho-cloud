//! JavaScript / TypeScript: `fetch`, axios, React Router, Next.js file routes.

use std::collections::HashMap;

use mage_core::domain::{Language, RepoPath};
use mage_core::ports::knowledge::{AppFact, AppFactRecord, FrameworkId, HandlerRef, SymbolDef, UrlTemplate};
use tree_sitter::Node;

use super::url::{concat, join_route, js_is_stringish, js_string, js_template, leading_lit, lit_template, var_template};
use super::{AppExtractor, ManifestFile, detect, line_of, named_children, record, text, walk_tree};

const JS: &[Language] = &[Language::JavaScript, Language::TypeScript, Language::Tsx];

pub(super) struct JsFetch;
pub(super) struct Axios;
pub(super) struct ReactRouter;
pub(super) struct NextRoutes;

/// Value node of property `key` in an object literal (`key: v` → `v`; shorthand `{ key }` → the
/// identifier itself); `None` when absent.
fn prop<'t>(object: Node<'t>, key: &str, src: &[u8]) -> Option<Node<'t>> {
    if object.kind() != "object" {
        return None;
    }
    named_children(object).into_iter().find_map(|member| match member.kind() {
        "pair" => {
            let k = member.child_by_field_name("key")?;
            let name = match k.kind() {
                "string" => js_string(k, src)?,
                _ => text(k, src).to_owned(),
            };
            if name == key { member.child_by_field_name("value") } else { None }
        }
        "shorthand_property_identifier" if text(member, src) == key => Some(member),
        _ => None,
    })
}

fn call_args(call: Node<'_>) -> Vec<Node<'_>> {
    call.child_by_field_name("arguments").filter(|a| a.kind() == "arguments").map(named_children).unwrap_or_default()
}

/// Uppercased literal method; `None` for a non-literal expression.
fn method_literal(node: Node<'_>, src: &[u8]) -> Option<String> {
    js_string(node, src).map(|m| m.to_ascii_uppercase())
}

/// Method of an options object: literal → upper, non-literal → None, absent → GET.
fn options_method(options: Option<Node<'_>>, src: &[u8]) -> Option<String> {
    match options.and_then(|o| prop(o, "method", src)) {
        Some(m) => method_literal(m, src),
        None => Some("GET".to_owned()),
    }
}

// ---------------------------------------------------------------------------------------------
// fetch

impl AppExtractor for JsFetch {
    fn id(&self) -> FrameworkId {
        FrameworkId::JsFetch
    }
    fn languages(&self) -> &'static [Language] {
        JS
    }
    fn detect(&self, _root: Option<&RepoPath>, manifests: &[ManifestFile<'_>]) -> bool {
        detect::has(manifests, "package.json")
    }
    fn extract(&self, _path: &RepoPath, tree: &tree_sitter::Tree, src: &[u8], symbols: &[SymbolDef], out: &mut Vec<AppFactRecord>) {
        walk_tree(tree.root_node(), |node| {
            if node.kind() != "call_expression" {
                return;
            }
            let Some(func) = node.child_by_field_name("function") else { return };
            let is_fetch = match func.kind() {
                "identifier" => text(func, src) == "fetch",
                "member_expression" => {
                    matches!(text(func, src), "window.fetch" | "globalThis.fetch" | "self.fetch")
                }
                _ => false,
            };
            if !is_fetch {
                return;
            }
            let args = call_args(node);
            let Some(url) = args.first() else { return };
            let fact = AppFact::HttpCall {
                method: options_method(args.get(1).copied(), src),
                url: js_template(*url, src),
                client: "fetch".to_owned(),
            };
            out.push(record(self.id(), fact, line_of(node), symbols));
        });
    }
}

// ---------------------------------------------------------------------------------------------
// axios

const AXIOS_VERBS: &[&str] = &["get", "post", "put", "patch", "delete", "head", "options"];

impl AppExtractor for Axios {
    fn id(&self) -> FrameworkId {
        FrameworkId::Axios
    }
    fn languages(&self) -> &'static [Language] {
        JS
    }
    fn detect(&self, _root: Option<&RepoPath>, manifests: &[ManifestFile<'_>]) -> bool {
        detect::npm_dep(manifests, "axios")
    }
    fn extract(&self, _path: &RepoPath, tree: &tree_sitter::Tree, src: &[u8], symbols: &[SymbolDef], out: &mut Vec<AppFactRecord>) {
        let instances = axios_instances(tree.root_node(), src);
        walk_tree(tree.root_node(), |node| {
            if node.kind() != "call_expression" {
                return;
            }
            let Some(func) = node.child_by_field_name("function") else { return };
            let args = call_args(node);
            let (client, verb) = match func.kind() {
                "identifier" => (text(func, src), None),
                "member_expression" => {
                    let (Some(object), Some(property)) = (func.child_by_field_name("object"), func.child_by_field_name("property")) else {
                        return;
                    };
                    (text(object, src), Some(text(property, src)))
                }
                _ => return,
            };
            let base = match client {
                "axios" => None,
                name => match instances.get(name) {
                    Some(base) => Some(base),
                    None => return,
                },
            };
            let first = args.first().copied();
            let (method, url) = match verb {
                Some(v) if AXIOS_VERBS.contains(&v) => match first {
                    Some(url) => (Some(v.to_ascii_uppercase()), Some(js_template(url, src))),
                    None => return,
                },
                Some("request") => match first {
                    Some(config) => (options_method(Some(config), src), prop(config, "url", src).map(|u| js_template(u, src))),
                    None => return,
                },
                Some(_) => return,
                None => match first {
                    Some(config) if config.kind() == "object" => {
                        (options_method(Some(config), src), prop(config, "url", src).map(|u| js_template(u, src)))
                    }
                    Some(url) => (options_method(args.get(1).copied(), src), Some(js_template(url, src))),
                    None => return,
                },
            };
            let Some(url) = url else { return };
            let url = match base {
                Some(base) if !is_absolute(&url) => concat(base, &url),
                _ => url,
            };
            let fact = AppFact::HttpCall { method, url, client: client.to_owned() };
            out.push(record(self.id(), fact, line_of(node), symbols));
        });
    }
}

fn is_absolute(url: &UrlTemplate) -> bool {
    leading_lit(url).is_some_and(|s| s.starts_with("http://") || s.starts_with("https://") || s.starts_with("//"))
}

/// `const api = axios.create({ baseURL })` / `this.api = axios.create(…)` → receiver text → base URL template.
fn axios_instances(root: Node<'_>, src: &[u8]) -> HashMap<String, UrlTemplate> {
    let mut instances = HashMap::new();
    walk_tree(root, |node| {
        let (target, value) = match node.kind() {
            "variable_declarator" => (node.child_by_field_name("name"), node.child_by_field_name("value")),
            "assignment_expression" => (node.child_by_field_name("left"), node.child_by_field_name("right")),
            _ => return,
        };
        let (Some(target), Some(value)) = (target, value) else { return };
        if value.kind() != "call_expression" || value.child_by_field_name("function").is_none_or(|f| text(f, src) != "axios.create") {
            return;
        }
        let base = match call_args(value).first().and_then(|config| prop(*config, "baseURL", src)) {
            Some(b) if js_is_stringish(b, src) => js_template(b, src),
            Some(_) => var_template("baseURL"),
            None => UrlTemplate::default(),
        };
        instances.insert(text(target, src).to_owned(), base);
    });
    instances
}

// ---------------------------------------------------------------------------------------------
// React Router

const ROUTER_FACTORIES: &[&str] = &["createBrowserRouter", "createHashRouter", "createMemoryRouter", "useRoutes"];

impl AppExtractor for ReactRouter {
    fn id(&self) -> FrameworkId {
        FrameworkId::ReactRouter
    }
    fn languages(&self) -> &'static [Language] {
        JS
    }
    fn detect(&self, _root: Option<&RepoPath>, manifests: &[ManifestFile<'_>]) -> bool {
        detect::npm_dep(manifests, "react-router") || detect::npm_dep(manifests, "react-router-dom")
    }
    fn extract(&self, _path: &RepoPath, tree: &tree_sitter::Tree, src: &[u8], symbols: &[SymbolDef], out: &mut Vec<AppFactRecord>) {
        let arrays = array_decls(tree.root_node(), src);
        let mut rr = Routes { src, symbols, out, arrays };
        rr.visit(tree.root_node(), None);
    }
}

/// `const name = [ … ]` declarations (route object arrays passed by name).
fn array_decls<'t>(root: Node<'t>, src: &[u8]) -> HashMap<String, Node<'t>> {
    let mut arrays = HashMap::new();
    walk_tree(root, |node| {
        if node.kind() != "variable_declarator" {
            return;
        }
        if let (Some(name), Some(value)) = (node.child_by_field_name("name"), node.child_by_field_name("value")) {
            let value = unwrap_ts(value);
            if name.kind() == "identifier" && value.kind() == "array" {
                arrays.insert(text(name, src).to_owned(), value);
            }
        }
    });
    arrays
}

/// Strips TS `x as T` / `x satisfies T` / `(x)` wrappers.
fn unwrap_ts(node: Node<'_>) -> Node<'_> {
    match node.kind() {
        "as_expression" | "satisfies_expression" | "parenthesized_expression" | "non_null_expression" => {
            named_children(node).first().map_or(node, |inner| unwrap_ts(*inner))
        }
        _ => node,
    }
}

/// Name of a JSX element (`<C/>` → `C`, `<A.B>…</A.B>` → `A.B`).
fn jsx_name(node: Node<'_>, src: &[u8]) -> Option<String> {
    let node = unwrap_ts(node);
    let name = match node.kind() {
        "jsx_self_closing_element" => node.child_by_field_name("name")?,
        "jsx_element" => node.child_by_field_name("open_tag")?.child_by_field_name("name")?,
        _ => return None,
    };
    Some(text(name, src).to_owned())
}

/// Component reference `C` / `A.B`.
fn component_ref(node: Node<'_>, src: &[u8]) -> Option<String> {
    let node = unwrap_ts(node);
    matches!(node.kind(), "identifier" | "member_expression").then(|| text(node, src).to_owned())
}

struct Routes<'a, 't, 'o> {
    src: &'a [u8],
    symbols: &'a [SymbolDef],
    out: &'o mut Vec<AppFactRecord>,
    arrays: HashMap<String, Node<'t>>,
}

/// Route attributes common to JSX and object routes.
#[derive(Default)]
struct RouteSpec {
    path: Option<UrlTemplate>,
    index: bool,
    component: Option<String>,
}

impl<'t> Routes<'_, 't, '_> {
    fn visit(&mut self, node: Node<'t>, parent: Option<&UrlTemplate>) {
        match node.kind() {
            "jsx_element" => {
                let open = node.child_by_field_name("open_tag");
                match open.filter(|o| self.is_route(*o)) {
                    Some(open) => {
                        let own = self.route(open, self.jsx_spec(open), parent);
                        let close = node.child_by_field_name("close_tag");
                        for child in named_children(node) {
                            if Some(child) != Some(open) && Some(child) != close {
                                self.visit(child, own.as_ref());
                            }
                        }
                    }
                    None => self.children(node, parent),
                }
            }
            "jsx_self_closing_element" if self.is_route(node) => {
                self.route(node, self.jsx_spec(node), parent);
            }
            "call_expression" => {
                let factory = node.child_by_field_name("function").is_some_and(|f| ROUTER_FACTORIES.contains(&text(f, self.src)));
                if factory {
                    if let Some(routes) = call_args(node).first().and_then(|a| self.array(*a)) {
                        self.objects(routes, None);
                    }
                }
                self.children(node, parent);
            }
            _ => self.children(node, parent),
        }
    }

    fn children(&mut self, node: Node<'t>, parent: Option<&UrlTemplate>) {
        for child in named_children(node) {
            self.visit(child, parent);
        }
    }

    fn is_route(&self, tag: Node<'_>) -> bool {
        tag.child_by_field_name("name").is_some_and(|n| text(n, self.src) == "Route")
    }

    /// An array literal, or an identifier naming a `const x = [ … ]`.
    fn array(&self, node: Node<'t>) -> Option<Node<'t>> {
        let node = unwrap_ts(node);
        match node.kind() {
            "array" => Some(node),
            "identifier" => self.arrays.get(text(node, self.src)).copied(),
            _ => None,
        }
    }

    fn jsx_spec(&self, tag: Node<'_>) -> RouteSpec {
        let mut spec = RouteSpec::default();
        let mut cursor = tag.walk();
        for attr in tag.children_by_field_name("attribute", &mut cursor) {
            let parts = named_children(attr);
            let Some(name) = parts.first() else { continue };
            let value = parts.get(1).map(|v| {
                if v.kind() == "jsx_expression" { named_children(*v).first().copied().unwrap_or(*v) } else { *v }
            });
            match (text(*name, self.src), value) {
                ("index", None) => spec.index = true,
                ("index", Some(v)) => spec.index = text(v, self.src) == "true",
                ("path", Some(v)) => spec.path = Some(js_template(v, self.src)),
                ("element", Some(v)) => spec.component = spec.component.take().or_else(|| jsx_name(v, self.src)),
                ("component" | "Component", Some(v)) => spec.component = spec.component.take().or_else(|| component_ref(v, self.src)),
                _ => {}
            }
        }
        spec
    }

    fn object_spec(&self, object: Node<'_>) -> RouteSpec {
        let src = self.src;
        RouteSpec {
            path: prop(object, "path", src).map(|p| js_template(p, src)),
            index: prop(object, "index", src).is_some_and(|v| text(v, src) == "true"),
            component: prop(object, "element", src)
                .and_then(|e| jsx_name(e, src))
                .or_else(|| prop(object, "Component", src).and_then(|c| component_ref(c, src)))
                .or_else(|| prop(object, "component", src).and_then(|c| component_ref(c, src))),
        }
    }

    fn objects(&mut self, array: Node<'t>, parent: Option<&UrlTemplate>) {
        for object in named_children(array) {
            let object = unwrap_ts(object);
            if object.kind() != "object" {
                continue;
            }
            let own = self.route(object, self.object_spec(object), parent);
            if let Some(children) = prop(object, "children", self.src).and_then(|c| self.array(c)) {
                self.objects(children, own.as_ref());
            }
        }
    }

    /// Emits the page of one route; returns the path its children are relative to.
    fn route(&mut self, at: Node<'_>, spec: RouteSpec, parent: Option<&UrlTemplate>) -> Option<UrlTemplate> {
        let own = match (&spec.path, spec.index) {
            (Some(path), false) => Some(join_route(parent, path)),
            (_, true) => Some(parent.cloned().unwrap_or_else(|| lit_template("/"))),
            (None, false) => None,
        };
        if let (Some(path), Some(component)) = (&own, spec.component) {
            let fact = AppFact::PageDecl { path: path.clone(), component };
            self.out.push(record(FrameworkId::ReactRouter, fact, line_of(at), self.symbols));
        }
        own.or_else(|| parent.cloned())
    }
}

// ---------------------------------------------------------------------------------------------
// Next.js

const NEXT_METHODS: &[&str] = &["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];

impl AppExtractor for NextRoutes {
    fn id(&self) -> FrameworkId {
        FrameworkId::NextRoutes
    }
    fn languages(&self) -> &'static [Language] {
        JS
    }
    fn detect(&self, _root: Option<&RepoPath>, manifests: &[ManifestFile<'_>]) -> bool {
        detect::npm_dep(manifests, "next")
    }
    fn extract(&self, path: &RepoPath, tree: &tree_sitter::Tree, src: &[u8], symbols: &[SymbolDef], out: &mut Vec<AppFactRecord>) {
        let Some(file) = next_file(path) else { return };
        let root = tree.root_node();
        let mut push = |fact: AppFact, line: u32| out.push(record(self.id(), fact, line, symbols));
        let (default_name, default_line) = default_export(root, src).unwrap_or_else(|| ("default".to_owned(), 1));
        match file {
            NextFile::Api(path) => {
                let handler = HandlerRef::Named { name: default_name, qualifier: None };
                push(AppFact::RouteDecl { method: None, path, group: None, handler, middleware: Vec::new() }, default_line);
            }
            NextFile::Page(path) => push(AppFact::PageDecl { path, component: default_name }, default_line),
            NextFile::RouteHandler(path) => {
                for (method, line) in exported_methods(root, src) {
                    let handler = HandlerRef::Named { name: method.clone(), qualifier: None };
                    let fact = AppFact::RouteDecl { method: Some(method), path: path.clone(), group: None, handler, middleware: Vec::new() };
                    push(fact, line);
                }
            }
        }
    }
}

enum NextFile {
    /// `pages/api/**`
    Api(UrlTemplate),
    /// `pages/**` or `app/**/page.*`
    Page(UrlTemplate),
    /// `app/**/route.*`
    RouteHandler(UrlTemplate),
}

/// Classifies a file by its path below the first `pages` / `app` segment.
fn next_file(path: &RepoPath) -> Option<NextFile> {
    let segments: Vec<&str> = path.segments().collect();
    let at = segments.iter().position(|s| *s == "pages" || *s == "app")?;
    let (file, dirs) = segments[at + 1..].split_last()?;
    let (stem, ext) = file.rsplit_once('.')?;
    if !matches!(ext, "js" | "jsx" | "ts" | "tsx") || stem.ends_with(".d") {
        return None;
    }
    let url = |segs: &[&str]| {
        let kept: Vec<&str> = segs.iter().copied().filter(|s| !(s.starts_with('(') && s.ends_with(')')) && !s.starts_with('@')).collect();
        lit_template(&format!("/{}", kept.join("/")))
    };
    if segments[at] == "pages" {
        if matches!(stem, "_app" | "_document" | "_error") {
            return None;
        }
        let mut segs = dirs.to_vec();
        if stem != "index" {
            segs.push(stem);
        }
        return Some(if dirs.first() == Some(&"api") { NextFile::Api(url(&segs)) } else { NextFile::Page(url(&segs)) });
    }
    match stem {
        "page" => Some(NextFile::Page(url(dirs))),
        "route" => Some(NextFile::RouteHandler(url(dirs))),
        _ => None,
    }
}

fn is_default_export(node: Node<'_>) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor).any(|c| !c.is_named() && c.kind() == "default")
}

/// (name, line) of the default export; name `"default"` for anonymous / wrapped values.
fn default_export(root: Node<'_>, src: &[u8]) -> Option<(String, u32)> {
    let export = named_children(root).into_iter().find(|n| n.kind() == "export_statement" && is_default_export(*n))?;
    let target = export.child_by_field_name("declaration").or_else(|| export.child_by_field_name("value"));
    let name = match target {
        Some(t) if t.kind() == "identifier" => text(t, src).to_owned(),
        Some(t) => t.child_by_field_name("name").map_or_else(|| "default".to_owned(), |n| text(n, src).to_owned()),
        None => "default".to_owned(),
    };
    Some((name, line_of(export)))
}

/// Exported `GET`/`POST`/… route handlers with their lines.
fn exported_methods(root: Node<'_>, src: &[u8]) -> Vec<(String, u32)> {
    let mut out = Vec::new();
    for export in named_children(root).into_iter().filter(|n| n.kind() == "export_statement") {
        let mut names: Vec<(Node<'_>, Node<'_>)> = Vec::new();
        if let Some(decl) = export.child_by_field_name("declaration") {
            match decl.kind() {
                "function_declaration" | "generator_function_declaration" => names.extend(decl.child_by_field_name("name").map(|n| (n, decl))),
                "lexical_declaration" | "variable_declaration" => {
                    for declarator in named_children(decl).into_iter().filter(|d| d.kind() == "variable_declarator") {
                        names.extend(declarator.child_by_field_name("name").map(|n| (n, declarator)));
                    }
                }
                _ => {}
            }
        }
        for clause in named_children(export).into_iter().filter(|n| n.kind() == "export_clause") {
            for spec in named_children(clause).into_iter().filter(|s| s.kind() == "export_specifier") {
                names.extend(spec.child_by_field_name("alias").or_else(|| spec.child_by_field_name("name")).map(|n| (n, spec)));
            }
        }
        for (name, at) in names {
            let name = text(name, src);
            if NEXT_METHODS.contains(&name) {
                out.push((name.to_owned(), line_of(at)));
            }
        }
    }
    out
}
