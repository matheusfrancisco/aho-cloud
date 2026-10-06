//! Go HTTP routers: net/http, gin, chi, gorilla/mux, echo.
//!
//! One walker serves all five extractors. It tracks router variables of every framework (typed
//! parameters, constructors, groups, chi closure parameters) so a call on a known router is
//! attributed to that router's framework only; calls on unknown receivers are attributed to the
//! walking extractor when the call shape matches its API and the path argument looks like a path.
//! Emitted `RouteGroup` / `RouteDecl` facts follow the linker's grouping conventions:
//! - local group `api := r.Group("/api")` → `RouteGroup { var: "api", prefix: "/api", parent: "r" }`;
//! - router-typed parameter of `F` (or local constructor in `F`) → `RouteGroup { var: "r", prefix: [], parent: "F()" }`;
//! - pass site `Register(api)` → `RouteGroup { var: "Register()", prefix: [], parent: "api" }`;
//! - chi `r.Mount("/p", x())` → `RouteGroup { var: "x()", prefix: "/p", parent: "r" }`;
//! - chi `r.Route("/p", func(r chi.Router) {…})` → `RouteGroup { var: "r@<line>", prefix: "/p", parent: <outer> }`.

use std::collections::HashMap;

use mage_core::domain::{Language, RepoPath};
use mage_core::ports::knowledge::{AppFact, AppFactRecord, FrameworkId, HandlerRef, SymbolDef, UrlPart, UrlTemplate};
use tree_sitter::Node;

use super::url::{go_path_template, go_string};
use super::{AppExtractor, ManifestFile, detect, enclosing_ord, line_of, named_children, record, snippet, text};

const GO: &[Language] = &[Language::Go];

pub(super) struct GoNetHttp;
pub(super) struct Gin;
pub(super) struct Chi;
pub(super) struct GorillaMux;
pub(super) struct Echo;

impl AppExtractor for GoNetHttp {
    fn id(&self) -> FrameworkId {
        FrameworkId::GoNetHttp
    }
    fn languages(&self) -> &'static [Language] {
        GO
    }
    fn detect(&self, _root: Option<&RepoPath>, manifests: &[ManifestFile<'_>]) -> bool {
        detect::has(manifests, "go.mod")
    }
    fn extract(&self, _path: &RepoPath, tree: &tree_sitter::Tree, src: &[u8], symbols: &[SymbolDef], out: &mut Vec<AppFactRecord>) {
        walk(self.id(), tree, src, symbols, out);
    }
}

impl AppExtractor for Gin {
    fn id(&self) -> FrameworkId {
        FrameworkId::Gin
    }
    fn languages(&self) -> &'static [Language] {
        GO
    }
    fn detect(&self, _root: Option<&RepoPath>, manifests: &[ManifestFile<'_>]) -> bool {
        detect::go_requires(manifests, |m| m == "github.com/gin-gonic/gin")
    }
    fn extract(&self, _path: &RepoPath, tree: &tree_sitter::Tree, src: &[u8], symbols: &[SymbolDef], out: &mut Vec<AppFactRecord>) {
        walk(self.id(), tree, src, symbols, out);
    }
}

impl AppExtractor for Chi {
    fn id(&self) -> FrameworkId {
        FrameworkId::Chi
    }
    fn languages(&self) -> &'static [Language] {
        GO
    }
    fn detect(&self, _root: Option<&RepoPath>, manifests: &[ManifestFile<'_>]) -> bool {
        detect::go_requires(manifests, |m| detect::module_any_major(m, "github.com/go-chi/chi"))
    }
    fn extract(&self, _path: &RepoPath, tree: &tree_sitter::Tree, src: &[u8], symbols: &[SymbolDef], out: &mut Vec<AppFactRecord>) {
        walk(self.id(), tree, src, symbols, out);
    }
}

impl AppExtractor for GorillaMux {
    fn id(&self) -> FrameworkId {
        FrameworkId::GorillaMux
    }
    fn languages(&self) -> &'static [Language] {
        GO
    }
    fn detect(&self, _root: Option<&RepoPath>, manifests: &[ManifestFile<'_>]) -> bool {
        detect::go_requires(manifests, |m| m == "github.com/gorilla/mux")
    }
    fn extract(&self, _path: &RepoPath, tree: &tree_sitter::Tree, src: &[u8], symbols: &[SymbolDef], out: &mut Vec<AppFactRecord>) {
        walk(self.id(), tree, src, symbols, out);
    }
}

impl AppExtractor for Echo {
    fn id(&self) -> FrameworkId {
        FrameworkId::Echo
    }
    fn languages(&self) -> &'static [Language] {
        GO
    }
    fn detect(&self, _root: Option<&RepoPath>, manifests: &[ManifestFile<'_>]) -> bool {
        detect::go_requires(manifests, |m| detect::module_any_major(m, "github.com/labstack/echo"))
    }
    fn extract(&self, _path: &RepoPath, tree: &tree_sitter::Tree, src: &[u8], symbols: &[SymbolDef], out: &mut Vec<AppFactRecord>) {
        walk(self.id(), tree, src, symbols, out);
    }
}

fn walk(fw: FrameworkId, tree: &tree_sitter::Tree, src: &[u8], symbols: &[SymbolDef], out: &mut Vec<AppFactRecord>) {
    let mut walker = Walker { fw, src, symbols, out, scopes: vec![HashMap::new()], funcs: Vec::new() };
    walker.visit(tree.root_node());
}

/// Framework of a router-typed parameter (`*gin.RouterGroup`, `chi.Router`, …).
fn router_type(ty: &str) -> Option<FrameworkId> {
    let ty: String = ty.split_whitespace().collect();
    Some(match ty.as_str() {
        "*gin.RouterGroup" | "*gin.Engine" | "gin.IRouter" | "gin.IRoutes" => FrameworkId::Gin,
        "chi.Router" | "*chi.Mux" => FrameworkId::Chi,
        "*mux.Router" => FrameworkId::GorillaMux,
        "*echo.Echo" | "*echo.Group" => FrameworkId::Echo,
        "*http.ServeMux" => FrameworkId::GoNetHttp,
        _ => return None,
    })
}

/// Framework of a router constructor call `pkg.F()`.
fn constructor(pkg: &str, func: &str) -> Option<FrameworkId> {
    Some(match (pkg, func) {
        ("gin", "New" | "Default") => FrameworkId::Gin,
        ("chi", "NewRouter" | "NewMux") => FrameworkId::Chi,
        ("mux", "NewRouter") => FrameworkId::GorillaMux,
        ("http", "NewServeMux") => FrameworkId::GoNetHttp,
        ("echo", "New") => FrameworkId::Echo,
        _ => return None,
    })
}

const GIN_METHODS: &[&str] = &["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"];
const ECHO_METHODS: &[&str] = &["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS", "CONNECT", "TRACE"];
const CHI_METHODS: &[&str] = &["Get", "Post", "Put", "Patch", "Delete", "Head", "Options", "Connect", "Trace"];

/// A router variable in scope.
#[derive(Clone)]
struct Bound {
    /// Name used in facts (`r`, `api`, `r@12`).
    resolved: String,
    fw: FrameworkId,
}

struct Func {
    /// Simple name of a function / method declaration; `None` for a func literal.
    named: Option<String>,
    /// Parameter names in order (`""` for unnamed ones).
    params: Vec<String>,
}

/// A resolved call receiver.
struct Recv {
    name: String,
    known: Option<FrameworkId>,
    /// chi `r.With(mw…)` middleware.
    middleware: Vec<String>,
}

/// Route declaration shape of one call: argument positions and methods.
struct DeclSpec {
    methods: Vec<Option<String>>,
    path: usize,
    handler: usize,
    middleware: Vec<usize>,
}

impl DeclSpec {
    fn new(methods: Vec<Option<String>>, path: usize, handler: usize, middleware: impl IntoIterator<Item = usize>) -> Self {
        Self { methods, path, handler, middleware: middleware.into_iter().collect() }
    }
}

enum Shape {
    /// Not a router call.
    No,
    /// Handled; children still need a generic visit.
    Matched,
    /// Handled including the children.
    Visited,
}

struct Walker<'a, 'o> {
    fw: FrameworkId,
    src: &'a [u8],
    symbols: &'a [SymbolDef],
    out: &'o mut Vec<AppFactRecord>,
    scopes: Vec<HashMap<String, Bound>>,
    funcs: Vec<Func>,
}

impl<'a> Walker<'a, '_> {
    fn emit(&mut self, fact: AppFact, line: u32) {
        self.out.push(record(self.fw, fact, line, self.symbols));
    }

    fn emit_group(&mut self, fw: FrameworkId, var: String, prefix: UrlTemplate, parent: String, line: u32) {
        if fw == self.fw {
            self.emit(AppFact::RouteGroup { var, prefix, parent: Some(parent) }, line);
        }
    }

    fn lookup(&self, name: &str) -> Option<&Bound> {
        self.scopes.iter().rev().find_map(|s| s.get(name))
    }

    fn bind(&mut self, name: &str, bound: Bound) {
        if name == "_" || name.is_empty() {
            return;
        }
        if let Some(scope) = self.scopes.last_mut() {
            scope.insert(name.to_owned(), bound);
        }
    }

    fn named_fn(&self) -> Option<&str> {
        self.funcs.iter().rev().find_map(|f| f.named.as_deref())
    }

    fn text(&self, node: Node<'_>) -> &'a str {
        text(node, self.src)
    }

    fn visit(&mut self, node: Node<'_>) {
        match node.kind() {
            "function_declaration" | "method_declaration" => self.func_decl(node),
            "func_literal" => self.func_lit(node, None),
            "short_var_declaration" | "assignment_statement" => {
                self.bind_pairs(node.child_by_field_name("left").map(named_children).unwrap_or_default(), node.child_by_field_name("right"))
            }
            "var_spec" => {
                let mut cursor = node.walk();
                let names: Vec<Node<'_>> = node.children_by_field_name("name", &mut cursor).collect();
                self.bind_pairs(names, node.child_by_field_name("value"));
            }
            "call_expression" => self.call(node, None),
            _ => self.children(node),
        }
    }

    fn children(&mut self, node: Node<'_>) {
        for child in named_children(node) {
            self.visit(child);
        }
    }

    /// `a, b := x, y` style: a call on the right of an identifier may bind a router variable.
    fn bind_pairs(&mut self, lefts: Vec<Node<'_>>, right: Option<Node<'_>>) {
        let Some(right) = right else { return };
        let rights = named_children(right);
        if lefts.len() != rights.len() {
            self.visit(right);
            return;
        }
        for (left, value) in lefts.into_iter().zip(rights) {
            if left.kind() == "identifier" && value.kind() == "call_expression" {
                let name = self.text(left);
                self.call(value, Some(name));
            } else {
                self.visit(value);
            }
        }
    }

    /// Parameter names in order, and the router-typed ones with their framework.
    fn params(&self, list: Option<Node<'_>>) -> (Vec<String>, Vec<(String, FrameworkId)>) {
        let mut names = Vec::new();
        let mut routers = Vec::new();
        for decl in list.map(named_children).unwrap_or_default() {
            if !matches!(decl.kind(), "parameter_declaration" | "variadic_parameter_declaration") {
                continue;
            }
            let fw = if decl.kind() == "parameter_declaration" {
                decl.child_by_field_name("type").and_then(|t| router_type(self.text(t)))
            } else {
                None
            };
            let mut cursor = decl.walk();
            let decl_names: Vec<&str> = decl.children_by_field_name("name", &mut cursor).map(|n| self.text(n)).collect();
            if decl_names.is_empty() {
                names.push(String::new());
            }
            for name in decl_names {
                names.push(name.to_owned());
                if let Some(fw) = fw {
                    routers.push((name.to_owned(), fw));
                }
            }
        }
        (names, routers)
    }

    fn func_decl(&mut self, node: Node<'_>) {
        let name = node.child_by_field_name("name").map(|n| self.text(n).to_owned());
        let (params, routers) = self.params(node.child_by_field_name("parameters"));
        self.scopes.push(HashMap::new());
        self.funcs.push(Func { named: name.clone(), params });
        let line = line_of(node);
        for (param, fw) in routers {
            self.bind(&param, Bound { resolved: param.clone(), fw });
            if let Some(func) = &name {
                self.emit_group(fw, param, UrlTemplate::default(), format!("{func}()"), line);
            }
        }
        if let Some(body) = node.child_by_field_name("body") {
            self.visit(body);
        }
        self.funcs.pop();
        self.scopes.pop();
    }

    /// `closure` = (resolved name, framework) bound to the literal's first parameter (chi closures).
    fn func_lit(&mut self, node: Node<'_>, closure: Option<(String, FrameworkId)>) {
        let (params, routers) = self.params(node.child_by_field_name("parameters"));
        let first = params.first().cloned();
        self.scopes.push(HashMap::new());
        self.funcs.push(Func { named: None, params });
        for (param, fw) in routers {
            self.bind(&param, Bound { resolved: param.clone(), fw });
        }
        if let (Some((resolved, fw)), Some(first)) = (closure, first) {
            self.bind(&first, Bound { resolved, fw });
        }
        if let Some(body) = node.child_by_field_name("body") {
            self.visit(body);
        }
        self.funcs.pop();
        self.scopes.pop();
    }

    fn call(&mut self, node: Node<'_>, bind: Option<&str>) {
        let Some(func) = node.child_by_field_name("function") else {
            self.children(node);
            return;
        };
        let args = node.child_by_field_name("arguments").map(named_children).unwrap_or_default();
        let shape = match (func.kind(), func.child_by_field_name("operand"), func.child_by_field_name("field")) {
            ("selector_expression", Some(operand), Some(field)) => self.selector_call(node, operand, self.text(field), &args, bind),
            _ => Shape::No,
        };
        match shape {
            Shape::Visited => return,
            Shape::Matched => {}
            Shape::No => self.pass_site(node, func, &args),
        }
        self.visit(func);
        for arg in args {
            self.visit(arg);
        }
    }

    fn receiver(&self, operand: Node<'_>) -> Recv {
        match operand.kind() {
            "identifier" => match self.lookup(self.text(operand)) {
                Some(b) => Recv { name: b.resolved.clone(), known: Some(b.fw), middleware: Vec::new() },
                None => Recv { name: self.text(operand).to_owned(), known: None, middleware: Vec::new() },
            },
            "call_expression" => {
                let func = operand.child_by_field_name("function");
                let inner = func.filter(|f| f.kind() == "selector_expression");
                let with = inner.and_then(|f| f.child_by_field_name("field")).is_some_and(|f| self.text(f) == "With");
                match inner.and_then(|f| f.child_by_field_name("operand")) {
                    Some(inner_operand) if with => {
                        let mut recv = self.receiver(inner_operand);
                        let args = operand.child_by_field_name("arguments").map(named_children).unwrap_or_default();
                        recv.middleware.extend(args.into_iter().map(|a| snippet(a, self.src)));
                        recv
                    }
                    _ => Recv { name: snippet(operand, self.src), known: None, middleware: Vec::new() },
                }
            }
            _ => Recv { name: snippet(operand, self.src), known: None, middleware: Vec::new() },
        }
    }

    /// Framework owning a call on `recv` among `fws`: the known receiver's framework, or the
    /// walking framework for unknown receivers.
    fn owner(&self, recv: &Recv, fws: &[FrameworkId]) -> Option<FrameworkId> {
        match recv.known {
            Some(known) => fws.contains(&known).then_some(known),
            None => fws.contains(&self.fw).then_some(self.fw),
        }
    }

    /// Path argument: string-ish expression; for unknown receivers it must also look like a path.
    fn path_arg(&self, node: Node<'_>, recv: &Recv) -> Option<UrlTemplate> {
        let template = go_path_template(node, self.src)?;
        if recv.known.is_some() || looks_like_path(&template) { Some(template) } else { None }
    }

    fn selector_call(&mut self, node: Node<'_>, operand: Node<'_>, method: &str, args: &[Node<'_>], bind: Option<&str>) -> Shape {
        let line = line_of(node);
        let unbound_pkg = (operand.kind() == "identifier" && self.lookup(self.text(operand)).is_none()).then(|| self.text(operand));

        if let Some(pkg) = unbound_pkg {
            if let Some(fw) = constructor(pkg, method) {
                if let Some(name) = bind {
                    self.bind(name, Bound { resolved: name.to_owned(), fw });
                    if let Some(func) = self.named_fn().map(|f| format!("{f}()")) {
                        self.emit_group(fw, name.to_owned(), UrlTemplate::default(), func, line);
                    }
                }
                return Shape::Matched;
            }
            if pkg == "http" {
                if matches!(method, "HandleFunc" | "Handle") && args.len() == 2 {
                    if self.fw == FrameworkId::GoNetHttp {
                        self.net_http_decl(node, None, args);
                    }
                    return Shape::Matched;
                }
                return Shape::No;
            }
        }

        // gorilla `r.PathPrefix("/p").Subrouter()` / `.Handler(h)` / `.HandlerFunc(h)`
        if matches!(method, "Subrouter" | "Handler" | "HandlerFunc") && operand.kind() == "call_expression" {
            if let Some((inner_recv, prefix)) = self.path_prefix(operand) {
                let Some(fw) = self.owner(&inner_recv, &[FrameworkId::GorillaMux]) else { return Shape::No };
                if method == "Subrouter" {
                    if let Some(name) = bind {
                        self.bind(name, Bound { resolved: name.to_owned(), fw });
                        self.emit_group(fw, name.to_owned(), prefix, inner_recv.name, line);
                    }
                } else if let Some(handler) = args.first().copied() {
                    if fw == self.fw {
                        let methods = self.chained_methods(node);
                        self.emit_decls(line, methods, prefix, Some(inner_recv.name), handler, Vec::new());
                    }
                }
                return Shape::Matched;
            }
        }

        let recv = self.receiver(operand);
        match method {
            "Group" => {
                let Some(first) = args.first().copied() else { return Shape::No };
                if first.kind() == "func_literal" {
                    if let Some(fw) = self.owner(&recv, &[FrameworkId::Chi]) {
                        return self.chi_closure(node, recv, UrlTemplate::default(), first, fw);
                    }
                    return Shape::No;
                }
                let (Some(fw), Some(prefix)) = (self.owner(&recv, &[FrameworkId::Gin, FrameworkId::Echo]), self.path_arg(first, &recv)) else {
                    return Shape::No;
                };
                if let Some(name) = bind {
                    self.bind(name, Bound { resolved: name.to_owned(), fw });
                    self.emit_group(fw, name.to_owned(), prefix, recv.name, line);
                }
                Shape::Matched
            }
            "Route" if args.len() == 2 => {
                let (Some(fw), Some(prefix)) = (self.owner(&recv, &[FrameworkId::Chi]), self.path_arg(args[0], &recv)) else {
                    return Shape::No;
                };
                if args[1].kind() == "func_literal" {
                    return self.chi_closure(node, recv, prefix, args[1], fw);
                }
                let var = format!("{}()", self.callee_name(args[1]));
                self.emit_group(fw, var, prefix, recv.name, line);
                Shape::Matched
            }
            "Mount" if args.len() == 2 => {
                let (Some(fw), Some(prefix)) = (self.owner(&recv, &[FrameworkId::Chi]), self.path_arg(args[0], &recv)) else {
                    return Shape::No;
                };
                let var = match args[1].kind() {
                    "call_expression" => format!("{}()", self.callee_name(args[1])),
                    "identifier" => {
                        let name = self.text(args[1]);
                        self.lookup(name).map_or_else(|| name.to_owned(), |b| b.resolved.clone())
                    }
                    _ => snippet(args[1], self.src),
                };
                self.emit_group(fw, var, prefix, recv.name, line);
                Shape::Matched
            }
            _ => self.route_decl(node, method, recv, args),
        }
    }

    /// `Y.PathPrefix(<path>)` → (receiver Y, path).
    fn path_prefix(&self, call: Node<'_>) -> Option<(Recv, UrlTemplate)> {
        let func = call.child_by_field_name("function").filter(|f| f.kind() == "selector_expression")?;
        if self.text(func.child_by_field_name("field")?) != "PathPrefix" {
            return None;
        }
        let recv = self.receiver(func.child_by_field_name("operand")?);
        let args = named_children(call.child_by_field_name("arguments")?);
        let [path] = args.as_slice() else { return None };
        let prefix = self.path_arg(*path, &recv)?;
        Some((recv, prefix))
    }

    fn chi_closure(&mut self, node: Node<'_>, recv: Recv, prefix: UrlTemplate, lit: Node<'_>, fw: FrameworkId) -> Shape {
        let line = line_of(node);
        let (params, _) = self.params(lit.child_by_field_name("parameters"));
        let param = params.first().filter(|p| !p.is_empty()).cloned().unwrap_or_else(|| "r".to_owned());
        let var = format!("{param}@{line}");
        self.emit_group(fw, var.clone(), prefix, recv.name, line);
        self.func_lit(lit, Some((var, fw)));
        Shape::Visited
    }

    /// Final segment of a callee or function value (`h.routes` → `routes`, `adminRouter()` → `adminRouter`).
    fn callee_name(&self, node: Node<'_>) -> String {
        let target = if node.kind() == "call_expression" { node.child_by_field_name("function").unwrap_or(node) } else { node };
        match target.kind() {
            "selector_expression" => target.child_by_field_name("field").map_or_else(|| snippet(target, self.src), |f| self.text(f).to_owned()),
            "identifier" => self.text(target).to_owned(),
            _ => snippet(target, self.src),
        }
    }

    /// Route declaration on `recv` for the receiver's framework (known) or the walking one (unknown).
    fn route_decl(&mut self, node: Node<'_>, method: &str, recv: Recv, args: &[Node<'_>]) -> Shape {
        let fw = recv.known.unwrap_or(self.fw);
        if recv.known.is_none() && fw == FrameworkId::GoNetHttp {
            return Shape::No;
        }
        let Some(spec) = self.decl_spec(fw, method, args, node) else { return Shape::No };
        let (Some(&path_node), Some(&handler)) = (args.get(spec.path), args.get(spec.handler)) else { return Shape::No };
        let Some(path) = self.path_arg(path_node, &recv) else { return Shape::No };
        if fw != self.fw {
            return Shape::Matched;
        }
        if fw == FrameworkId::GoNetHttp {
            self.net_http_decl(node, Some(recv.name), args);
            return Shape::Matched;
        }
        let mut middleware = recv.middleware;
        middleware.extend(spec.middleware.iter().filter_map(|&i| args.get(i)).map(|a| snippet(*a, self.src)));
        self.emit_decls(line_of(node), spec.methods, path, Some(recv.name), handler, middleware);
        Shape::Matched
    }

    /// Argument layout of `method(args)` in framework `fw`'s router API.
    fn decl_spec(&self, fw: FrameworkId, method: &str, args: &[Node<'_>], node: Node<'_>) -> Option<DeclSpec> {
        let n = args.len();
        let upper = |s: String| Some(s.to_ascii_uppercase());
        match fw {
            FrameworkId::Gin => match method {
                m if GIN_METHODS.contains(&m) && n >= 2 => Some(DeclSpec::new(vec![Some(m.to_owned())], 0, n - 1, 1..n - 1)),
                "Any" if n >= 2 => Some(DeclSpec::new(vec![None], 0, n - 1, 1..n - 1)),
                "Handle" if n >= 3 => {
                    let m = go_string(args[0], self.src)?;
                    Some(DeclSpec::new(vec![upper(m)], 1, n - 1, 2..n - 1))
                }
                _ => None,
            },
            FrameworkId::Echo => match method {
                m if ECHO_METHODS.contains(&m) && n >= 2 => Some(DeclSpec::new(vec![Some(m.to_owned())], 0, 1, 2..n)),
                "Any" if n >= 2 => Some(DeclSpec::new(vec![None], 0, 1, 2..n)),
                "Match" if n >= 3 => {
                    let methods: Vec<Option<String>> = self.string_elements(args[0]).into_iter().map(upper).collect();
                    (!methods.is_empty()).then(|| DeclSpec::new(methods, 1, 2, 3..n))
                }
                _ => None,
            },
            FrameworkId::Chi => match method {
                m if CHI_METHODS.contains(&m) && n == 2 => Some(DeclSpec::new(vec![upper(m.to_owned())], 0, 1, [])),
                "Method" | "MethodFunc" if n == 3 => {
                    let m = go_string(args[0], self.src)?;
                    Some(DeclSpec::new(vec![upper(m)], 1, 2, []))
                }
                "Handle" | "HandleFunc" if n == 2 => Some(DeclSpec::new(vec![None], 0, 1, [])),
                _ => None,
            },
            FrameworkId::GorillaMux => match method {
                "Handle" | "HandleFunc" if n == 2 => Some(DeclSpec::new(self.chained_methods(node), 0, 1, [])),
                _ => None,
            },
            FrameworkId::GoNetHttp => match method {
                "Handle" | "HandleFunc" if n == 2 => Some(DeclSpec::new(vec![None], 0, 1, [])),
                _ => None,
            },
            _ => None,
        }
    }

    /// String elements of a `[]string{"GET", "POST"}` literal.
    fn string_elements(&self, node: Node<'_>) -> Vec<String> {
        let Some(body) = node.child_by_field_name("body") else { return Vec::new() };
        named_children(body)
            .into_iter()
            .filter_map(|el| {
                let value = if el.kind() == "literal_element" { named_children(el).first().copied()? } else { el };
                go_string(value, self.src)
            })
            .collect()
    }

    /// Methods of gorilla `.Methods("GET", "POST")` calls chained onto `call` (`[None]` without any).
    fn chained_methods(&self, call: Node<'_>) -> Vec<Option<String>> {
        let mut methods = Vec::new();
        let mut cur = call;
        while let Some(sel) = cur.parent().filter(|p| p.kind() == "selector_expression") {
            if sel.child_by_field_name("operand") != Some(cur) {
                break;
            }
            let Some(outer) = sel.parent().filter(|p| p.kind() == "call_expression") else { break };
            if sel.child_by_field_name("field").is_some_and(|f| self.text(f) == "Methods") {
                let args = outer.child_by_field_name("arguments").map(named_children).unwrap_or_default();
                methods.extend(args.into_iter().filter_map(|a| go_string(a, self.src)).map(|m| Some(m.to_ascii_uppercase())));
            }
            cur = outer;
        }
        if methods.is_empty() { vec![None] } else { methods }
    }

    /// net/http `Handle(pattern, h)`: Go 1.22 `"GET /x"` method prefix and host prefix handled.
    fn net_http_decl(&mut self, node: Node<'_>, group: Option<String>, args: &[Node<'_>]) {
        let [pattern, handler] = args else { return };
        let Some(template) = go_path_template(*pattern, self.src) else { return };
        let (method, path) = split_pattern(template);
        self.emit_decls(line_of(node), vec![method], path, group, *handler, Vec::new());
    }

    fn emit_decls(
        &mut self,
        line: u32,
        methods: Vec<Option<String>>,
        path: UrlTemplate,
        group: Option<String>,
        handler: Node<'_>,
        middleware: Vec<String>,
    ) {
        let handler = self.handler(handler);
        for method in methods {
            let fact = AppFact::RouteDecl {
                method,
                path: path.clone(),
                group: group.clone(),
                handler: handler.clone(),
                middleware: middleware.clone(),
            };
            self.emit(fact, line);
        }
    }

    fn handler(&self, node: Node<'_>) -> HandlerRef {
        let named = |name: String, qualifier: Option<String>| HandlerRef::Named { name, qualifier };
        match node.kind() {
            "selector_expression" => match (node.child_by_field_name("operand"), node.child_by_field_name("field")) {
                (Some(operand), Some(field)) => named(self.text(field).to_owned(), Some(snippet(operand, self.src))),
                _ => named(snippet(node, self.src), None),
            },
            "identifier" => {
                let name = self.text(node);
                match self.param_index(name) {
                    Some(index) => HandlerRef::Param { index },
                    None => named(name.to_owned(), None),
                }
            }
            "call_expression" => {
                let Some(func) = node.child_by_field_name("function") else { return named(snippet(node, self.src), None) };
                let args = node.child_by_field_name("arguments").map(named_children).unwrap_or_default();
                if self.text(func) == "http.HandlerFunc" && args.len() == 1 {
                    return self.handler(args[0]);
                }
                match func.kind() {
                    "selector_expression" | "identifier" => self.handler(func),
                    _ => named(snippet(node, self.src), None),
                }
            }
            "func_literal" => {
                let line = line_of(node);
                HandlerRef::Inline { enclosing: enclosing_ord(self.symbols, line), line }
            }
            "unary_expression" | "parenthesized_expression" => match node.child_by_field_name("operand").or_else(|| named_children(node).first().copied()) {
                Some(inner) => self.handler(inner),
                None => named(snippet(node, self.src), None),
            },
            "composite_literal" => match node.child_by_field_name("type") {
                Some(ty) if ty.kind() == "qualified_type" => match (ty.child_by_field_name("name"), ty.child_by_field_name("package")) {
                    (Some(name), Some(pkg)) => named(self.text(name).to_owned(), Some(self.text(pkg).to_owned())),
                    _ => named(snippet(ty, self.src), None),
                },
                Some(ty) => named(snippet(ty, self.src), None),
                None => named(snippet(node, self.src), None),
            },
            _ => named(snippet(node, self.src), None),
        }
    }

    /// Index of `name` among the enclosing named function's parameters (not shadowed by a closure parameter).
    fn param_index(&self, name: &str) -> Option<u8> {
        for func in self.funcs.iter().rev() {
            let pos = func.params.iter().position(|p| p == name);
            if func.named.is_none() {
                if pos.is_some() {
                    return None;
                }
                continue;
            }
            return pos.and_then(|i| u8::try_from(i).ok());
        }
        None
    }

    /// `Register(api)` / `x.Register(api)` with a router variable argument.
    fn pass_site(&mut self, node: Node<'_>, func: Node<'_>, args: &[Node<'_>]) {
        let callee = match func.kind() {
            "identifier" => self.text(func).to_owned(),
            "selector_expression" => {
                let Some(field) = func.child_by_field_name("field") else { return };
                // `http.ListenAndServe(addr, r)` / `httptest.NewServer(r)` serve a router, they don't register on it.
                let pkg = func.child_by_field_name("operand").filter(|o| o.kind() == "identifier").map(|o| self.text(o));
                if pkg.is_some_and(|p| matches!(p, "http" | "httptest") && self.lookup(p).is_none()) {
                    return;
                }
                self.text(field).to_owned()
            }
            _ => return,
        };
        let line = line_of(node);
        for arg in args {
            if arg.kind() != "identifier" {
                continue;
            }
            let Some(bound) = self.lookup(self.text(*arg)).cloned() else { continue };
            self.emit_group(bound.fw, format!("{callee}()"), UrlTemplate::default(), bound.resolved, line);
        }
    }
}

/// For unknown receivers: the path starts with `/` (or is empty, or is a hole followed by a `/` literal).
fn looks_like_path(template: &UrlTemplate) -> bool {
    match template.parts.first() {
        None => true,
        Some(UrlPart::Lit(s)) => s.starts_with('/'),
        Some(UrlPart::Var(_)) => template.parts.iter().any(|p| matches!(p, UrlPart::Lit(s) if s.contains('/'))),
    }
}

/// Go 1.22 ServeMux pattern `[METHOD ][HOST]/[PATH]` → (method, path without host).
fn split_pattern(template: UrlTemplate) -> (Option<String>, UrlTemplate) {
    let mut parts = template.parts;
    let Some(UrlPart::Lit(first)) = parts.first().cloned() else { return (None, UrlTemplate { parts }) };
    let trimmed = first.trim_start();
    let (method, rest) = match trimmed.split_once([' ', '\t']) {
        Some((m, rest)) if !m.is_empty() && m.bytes().all(|b| b.is_ascii_uppercase()) => (Some(m.to_owned()), rest.trim_start()),
        _ => (None, trimmed),
    };
    let rest = match rest.find('/') {
        Some(slash) => &rest[slash..],
        None => rest,
    };
    if rest.is_empty() {
        parts.remove(0);
    } else {
        parts[0] = UrlPart::Lit(rest.to_owned());
    }
    (method, UrlTemplate { parts })
}
