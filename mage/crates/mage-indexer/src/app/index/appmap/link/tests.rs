use mage_core::domain::{BlobOid, CommitSha, Language, Layer, LineRange};
use mage_core::ports::knowledge::{
    AppEdgeKind, AppFact, AppFactAt, AppFactRecord, AppNode, AppNodeKind, FrameworkId, HandlerRef, NameRef, ParsedBlob, RefKind, StageStatus, SymbolDef,
    SymbolKind, TableOp,
};

use super::*;
use crate::testing::world::{World, WorldFile, world_org, world_path};

fn lit(s: &str) -> UrlTemplate {
    UrlTemplate { parts: vec![UrlPart::Lit(s.into())] }
}

fn var(name: &str) -> UrlPart {
    UrlPart::Var(Some(name.into()))
}

fn oid(n: u8) -> BlobOid {
    BlobOid::from_bytes([n; 20])
}

/// Facts of one file: `(line, fact)` with ords in order and no enclosing symbol unless given.
fn facts(path: &str, blob: BlobOid, framework: FrameworkId, items: Vec<(u32, Option<u32>, AppFact)>) -> Vec<AppFactAt> {
    items
        .into_iter()
        .enumerate()
        .map(|(ord, (line, enclosing, fact))| AppFactAt { path: world_path(path), blob, fact: AppFactRecord { ord: ord as u32, framework, fact, line, enclosing } })
        .collect()
}

fn group(var: &str, prefix: UrlTemplate, parent: Option<&str>) -> AppFact {
    AppFact::RouteGroup { var: var.into(), prefix, parent: parent.map(str::to_owned) }
}

fn route(method: Option<&str>, path: &str, group: Option<&str>, handler: HandlerRef) -> AppFact {
    AppFact::RouteDecl { method: method.map(str::to_owned), path: lit(path), group: group.map(str::to_owned), handler, middleware: Vec::new() }
}

fn named(name: &str, qualifier: Option<&str>) -> HandlerRef {
    HandlerRef::Named { name: name.into(), qualifier: qualifier.map(str::to_owned) }
}

fn prefixes(groups: &RouteGroups<'_>, file: &str, var: &str, line: u32) -> Vec<String> {
    scoped(groups, file, var, line, None)
}

fn scoped(groups: &RouteGroups<'_>, file: &str, var: &str, line: u32, scope: Option<u32>) -> Vec<String> {
    let file = world_path(file);
    groups.prefixes(var, GroupUse { file: &file, line, scope }).iter().map(render).collect()
}

#[test]
fn join_inserts_and_merges_slashes() {
    assert_eq!(render(&join(&lit("/api"), &lit("/x"))), "/api/x");
    assert_eq!(render(&join(&lit("/api/"), &lit("x"))), "/api/x");
    assert_eq!(render(&join(&lit("/api"), &lit("x"))), "/api/x");
    assert_eq!(render(&join(&UrlTemplate::default(), &lit("x"))), "x");
    let joined = join(&UrlTemplate { parts: vec![var("base"), UrlPart::Lit("/api".into())] }, &lit("/users"));
    assert_eq!(joined.parts, vec![var("base"), UrlPart::Lit("/api/users".into())], "adjacent literals merge");
}

/// Gin: groups across functions — `main` builds `/api`, passes it to `Register(rg)`, which adds
/// `/connections`.
#[test]
fn gin_groups_across_functions() {
    let mut all = facts("cmd/main.go", oid(1), FrameworkId::Gin, vec![
        (10, None, group("r", UrlTemplate::default(), Some("main()"))),
        (11, None, group("api", lit("/api"), Some("r"))),
        (12, None, group("Register()", UrlTemplate::default(), Some("api"))),
    ]);
    all.extend(facts("internal/handlers/routes.go", oid(2), FrameworkId::Gin, vec![
        (5, None, group("rg", UrlTemplate::default(), Some("Register()"))),
        (6, None, group("conns", lit("/connections"), Some("rg"))),
        (7, None, route(Some("GET"), "/:id", Some("conns"), named("Get", Some("h")))),
    ]));
    let groups = RouteGroups::new(&all);
    assert_eq!(prefixes(&groups, "internal/handlers/routes.go", "conns", 7), ["/api/connections"]);
    assert_eq!(prefixes(&groups, "internal/handlers/routes.go", "rg", 7), ["/api"]);
    assert_eq!(prefixes(&groups, "cmd/main.go", "r", 11), [""], "root router");
    assert_eq!(prefixes(&groups, "cmd/main.go", "unknown", 11), [""], "unknown var = root");
}

/// Chi: nested `Route` closures (unique `r@line` vars) and `Mount` of a router built elsewhere,
/// mounted twice.
#[test]
fn chi_nested_routes_and_mounts() {
    let mut all = facts("server/router.go", oid(1), FrameworkId::Chi, vec![
        (3, None, group("r", UrlTemplate::default(), Some("NewRouter()"))),
        (4, None, group("r@4", lit("/api"), Some("r"))),
        (5, None, group("r@5", lit("/users"), Some("r@4"))),
        (6, None, route(Some("GET"), "/{id}", Some("r@5"), named("GetUser", None))),
        (9, None, group("adminRouter()", lit("/admin"), Some("r"))),
        (10, None, group("adminRouter()", lit("/internal/admin"), Some("r@4"))),
    ]);
    all.extend(facts("server/admin.go", oid(2), FrameworkId::Chi, vec![
        (2, None, group("r", UrlTemplate::default(), Some("adminRouter()"))),
        (3, None, route(Some("GET"), "/stats", Some("r"), named("Stats", None))),
    ]));
    let groups = RouteGroups::new(&all);
    assert_eq!(prefixes(&groups, "server/router.go", "r@5", 6), ["/api/users"]);
    assert_eq!(prefixes(&groups, "server/admin.go", "r", 3), ["/admin", "/api/internal/admin"], "one prefix per mount site");
}

/// The facts the chi extractor emits for a router file (closure vars `r@line`, local
/// constructors bound to their function, a router mounted by identifier after its routes, a
/// mount of a router-returning function), with function scopes: `NewRouter` = 0, `debugRouter` = 1.
#[test]
fn chi_extractor_facts_with_scopes() {
    let none = UrlTemplate::default;
    let all = facts("server/router.go", oid(1), FrameworkId::Chi, vec![
        (4, Some(0), group("r", none(), Some("NewRouter()"))),
        (6, Some(0), group("r@6", lit("/api"), Some("r"))),
        (8, Some(0), group("ar@8", lit("/admin"), Some("r@6"))),
        (11, Some(0), group("r@11", none(), Some("r@6"))),
        (13, Some(0), group("files", none(), Some("NewRouter()"))),
        (15, Some(0), group("debugRouter()", lit("/debug"), Some("r"))),
        (16, Some(0), group("files", lit("/static"), Some("r"))),
        (23, Some(1), group("r", none(), Some("debugRouter()"))),
    ]);
    let groups = RouteGroups::new(&all);
    let at = |var, line, scope| scoped(&groups, "server/router.go", var, line, Some(scope));
    assert_eq!(at("r@6", 7, 0), ["/api"]);
    assert_eq!(at("ar@8", 9, 0), ["/api/admin"]);
    assert_eq!(at("r@11", 12, 0), ["/api"], "middleware-only group");
    assert_eq!(at("files", 14, 0), ["/static"], "the later mount of the same scope wins over the constructor");
    assert_eq!(at("r", 17, 0), [""]);
    assert_eq!(at("r", 24, 1), ["/debug"], "router built in a mounted function");

    // Same variable name in two functions: each use resolves in its own scope.
    let all = facts("x.go", oid(2), FrameworkId::Gin, vec![
        (1, Some(0), group("r", none(), Some("A()"))),
        (5, Some(1), group("e", none(), Some("B()"))),
        (6, Some(1), group("r", lit("/b"), Some("e"))),
    ]);
    let groups = RouteGroups::new(&all);
    assert_eq!(scoped(&groups, "x.go", "r", 2, Some(0)), [""]);
    assert_eq!(scoped(&groups, "x.go", "r", 7, Some(1)), ["/b"]);
}

#[test]
fn reassigned_group_and_cycles() {
    let all = facts("main.go", oid(1), FrameworkId::Gin, vec![
        (1, None, group("r", lit("/v0"), None)),
        (5, None, group("r", lit("/v1"), Some("r"))),
        (8, None, group("a", lit("/a"), Some("b"))),
        (9, None, group("b", lit("/b"), Some("a"))),
        (12, None, group("F()", UrlTemplate::default(), Some("p"))),
        (13, None, group("p", lit("/p"), Some("F()"))),
    ]);
    let groups = RouteGroups::new(&all);
    assert_eq!(prefixes(&groups, "main.go", "r", 3), ["/v0"]);
    assert_eq!(prefixes(&groups, "main.go", "r", 6), ["/v0/v1"], "the parent is the earlier binding");
    assert_eq!(prefixes(&groups, "main.go", "a", 10).len(), 1, "group cycles terminate");
    assert!(!prefixes(&groups, "main.go", "p", 14).is_empty(), "call cycles terminate");
}

fn endpoints(keys: &[&str]) -> Vec<(u32, RouteKey)> {
    keys.iter().enumerate().map(|(i, k)| (i as u32 + 1, k.parse().unwrap_or_else(|e| panic!("{k}: {e:?}")))).collect()
}

#[test]
fn client_url_with_base_url_variable_links_to_the_endpoint() {
    let eps = endpoints(&["GET /api/connections/{}", "GET /api/users/{}", "GET /health"]);
    let url = UrlTemplate { parts: vec![var("API_BASE"), UrlPart::Lit("/api/connections/".into()), var("id"), UrlPart::Lit("?verbose=1".into())] };
    let links = match_client(&url, Some("GET"), &eps);
    assert_eq!(links.len(), 1);
    assert_eq!(links[0].0, 1);
    assert!((links[0].1 - 2.0 / 3.0).abs() < 1e-5, "{links:?}");
    // The base URL may carry the `/api` prefix: suffix match at 0.9 ×.
    let url = UrlTemplate { parts: vec![var("baseURL"), UrlPart::Lit("/health".into())] };
    let links = match_client(&url, None, &endpoints(&["GET /internal/health"]));
    assert_eq!(links.len(), 1);
    assert!((links[0].1 - 0.9 * 0.9).abs() < 1e-5, "suffix × unknown method: {links:?}");
    // Without a leading hole, a shorter path never suffix-matches.
    assert!(match_client(&lit("/health"), None, &endpoints(&["GET /internal/health"])).is_empty());
    // No literal segment: no evidence, no link.
    assert!(match_client(&UrlTemplate { parts: vec![var("url")] }, Some("GET"), &endpoints(&["GET /"])).is_empty());
}

#[test]
fn ambiguous_matches_are_ordered_by_confidence() {
    let eps = endpoints(&[
        "POST /api/connections/{}",
        "ANY /api/connections/{}",
        "GET /api/{}/{}",
        "GET /api/connections/{}",
        "GET /api/connections/active",
        "GET /api/connections/{}/sessions",
    ]);
    let url = UrlTemplate { parts: vec![UrlPart::Lit("/api/connections/".into()), var("id")] };
    let links = match_client(&url, Some("GET"), &eps);
    let ids: Vec<u32> = links.iter().map(|(id, _)| *id).collect();
    assert_eq!(ids, [4, 5, 2], "equal method first, ANY after; other method and weak matches dropped");
    assert!(links.windows(2).all(|w| w[0].1 >= w[1].1));
    // Only a different-method endpoint: still linked, at half confidence.
    let links = match_client(&url, Some("DELETE"), &endpoints(&["POST /api/connections/{}"]));
    assert_eq!(links.len(), 1);
    assert!((links[0].1 - 2.0 / 3.0 * 0.5).abs() < 1e-5);
}

// ---------------------------------------------------------------------------------------------
// Graph over the symbol layer

fn sym(ord: u32, name: &str, qualified: &str, kind: SymbolKind, lines: (u32, u32)) -> SymbolDef {
    SymbolDef {
        ord,
        name: name.into(),
        qualified: qualified.into(),
        kind,
        container: None,
        span: LineRange::new(lines.0, lines.1).unwrap_or_else(|e| panic!("{e}")),
        name_pos: (lines.0, 1),
        signature: None,
        doc: None,
        exported: true,
    }
}

fn call(ord: u32, name: &str, kind: RefKind, line: u32, enclosing: u32) -> NameRef {
    NameRef { ord, name: name.into(), qualifier: None, kind, pos: (line, 3), enclosing: Some(enclosing) }
}

fn parsed(lang: Language, symbols: Vec<SymbolDef>, refs: Vec<NameRef>) -> ParsedBlob {
    ParsedBlob { lang, status: StageStatus::Ok, symbols, refs, literals: Vec::new() }
}

fn node<'g>(graph: &'g AppGraph, kind: AppNodeKind, key: &str) -> &'g AppNode {
    graph.nodes.iter().find(|n| n.kind == kind && n.key == key).unwrap_or_else(|| panic!("no {kind:?} {key}: {:#?}", graph.nodes))
}

fn edge(graph: &AppGraph, from: u32, to: u32, kind: AppEdgeKind) -> f32 {
    graph.edges.iter().find(|e| e.from == from && e.to == to && e.kind == kind).map(|e| e.confidence).unwrap_or_else(|| panic!("no {kind:?} {from}->{to}: {:#?}", graph.edges))
}

/// Go backend + TS frontend: page → component → client call → endpoint → handler → table.
#[tokio::test]
async fn frontend_call_links_to_backend_route_and_tables() {
    let w = World::new();
    let repo = w.repo("app").await;
    let key = w.version(repo.id, CommitSha::from_bytes([1; 20]), &[Layer::Files, Layer::Symbols]).await;
    let (routes, handlers, store, api, page) = (oid(1), oid(2), oid(3), oid(4), oid(5));
    w.manifest(&key, &[
        WorldFile::source("svc/routes.go", routes, Language::Go),
        WorldFile::source("svc/handlers/conn.go", handlers, Language::Go),
        WorldFile::source("svc/store/conn.go", store, Language::Go),
        WorldFile::source("web/src/api.ts", api, Language::TypeScript),
        WorldFile::source("web/src/pages/Connections.tsx", page, Language::Tsx),
    ])
    .await;
    w.parsed(vec![
        (routes, parsed(Language::Go, vec![sym(0, "Routes", "Routes", SymbolKind::Function, (1, 20))], Vec::new())),
        (
            handlers,
            parsed(Language::Go, vec![sym(0, "ListConnections", "Handler.ListConnections", SymbolKind::Method, (10, 30))], vec![call(
                0,
                "FetchAll",
                RefKind::Call,
                12,
                0,
            )]),
        ),
        (store, parsed(Language::Go, vec![sym(0, "FetchAll", "FetchAll", SymbolKind::Function, (3, 9))], Vec::new())),
        (api, parsed(Language::TypeScript, vec![sym(0, "fetchConnections", "fetchConnections", SymbolKind::Function, (1, 6))], Vec::new())),
        (
            page,
            parsed(Language::Tsx, vec![sym(0, "ConnectionsPage", "ConnectionsPage", SymbolKind::Function, (3, 20))], vec![call(
                0,
                "fetchConnections",
                RefKind::Call,
                5,
                0,
            )]),
        ),
    ])
    .await;
    let mut all = facts("svc/routes.go", routes, FrameworkId::Gin, vec![
        (4, Some(0), group("api", lit("/api"), Some("r"))),
        (5, Some(0), route(Some("GET"), "/connections", Some("api"), named("ListConnections", Some("h")))),
        (6, Some(0), route(Some("POST"), "/connections/:id/close", Some("api"), HandlerRef::Inline { enclosing: Some(0), line: 6 })),
    ]);
    all.extend(facts("svc/store/conn.go", store, FrameworkId::SqlStrings, vec![(5, Some(0), AppFact::TableRef {
        table: "connections".into(),
        op: TableOp::Select,
    })]));
    all.extend(facts("web/src/api.ts", api, FrameworkId::Axios, vec![(3, Some(0), AppFact::HttpCall {
        method: Some("GET".into()),
        url: UrlTemplate { parts: vec![var("baseURL"), UrlPart::Lit("/api/connections".into())] },
        client: "api".into(),
    })]));
    all.extend(facts("web/src/App.tsx", oid(6), FrameworkId::ReactRouter, vec![(9, None, AppFact::PageDecl {
        path: lit("/connections"),
        component: "ConnectionsPage".into(),
    })]));

    let outcome = link(&*w.knowledge, world_org(), &key, &all, LinkOptions::default()).await.unwrap_or_else(|e| panic!("{e}"));
    let g = &outcome.graph;
    let endpoint = node(g, AppNodeKind::Endpoint, "GET /api/connections");
    assert_eq!(endpoint.label, "GET /api/connections");
    assert_eq!(endpoint.at, Some((world_path("svc/routes.go"), 5)));
    let handler = node(g, AppNodeKind::Handler, "Handler.ListConnections");
    assert_eq!(edge(g, endpoint.id, handler.id, AppEdgeKind::HandledBy), 1.0);
    let inline = node(g, AppNodeKind::Endpoint, "POST /api/connections/{}/close");
    let routes_fn = node(g, AppNodeKind::Handler, "Routes");
    assert!(edge(g, inline.id, routes_fn.id, AppEdgeKind::HandledBy) > 0.5, "inline handler = enclosing symbol");
    let client = node(g, AppNodeKind::ClientCall, "GET /api/connections");
    assert_eq!(edge(g, client.id, endpoint.id, AppEdgeKind::CallsEndpoint), 1.0);
    let page_node = node(g, AppNodeKind::Page, "/connections");
    let component = node(g, AppNodeKind::Component, "ConnectionsPage");
    edge(g, page_node.id, component.id, AppEdgeKind::Renders);
    edge(g, component.id, endpoint.id, AppEdgeKind::CallsEndpoint);
    let table = node(g, AppNodeKind::Table, "connections");
    assert!((edge(g, handler.id, table.id, AppEdgeKind::Reads) - 0.8).abs() < 1e-5, "one call away");
    assert_eq!(outcome.coverage, Coverage {
        endpoints: 2,
        endpoints_with_handler: 2,
        client_calls: 1,
        client_calls_linked: 1,
        pages: 1,
        pages_with_component: 1
    });
    // Deterministic: same input, same graph.
    let again = link(&*w.knowledge, world_org(), &key, &all, LinkOptions::default()).await.unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(&again.graph, g);
}

/// Re-frame: a view dispatches an event whose handler issues the HTTP call.
#[tokio::test]
async fn cljs_dispatch_chain_reaches_the_endpoint() {
    let w = World::new();
    let repo = w.repo("webapp").await;
    let key = w.version(repo.id, CommitSha::from_bytes([2; 20]), &[Layer::Files, Layer::Symbols]).await;
    let (events, views, server) = (oid(11), oid(12), oid(13));
    w.manifest(&key, &[
        WorldFile::source("src/webapp/connections/events.cljs", events, Language::ClojureScript),
        WorldFile::source("src/webapp/connections/views.cljs", views, Language::ClojureScript),
        WorldFile::source("gateway/api/conn.go", server, Language::Go),
    ])
    .await;
    w.parsed(vec![
        (
            events,
            parsed(Language::ClojureScript, vec![sym(0, ":connections/get", "webapp.connections.events/:connections/get", SymbolKind::Registered, (5, 14))], Vec::new()),
        ),
        (
            views,
            parsed(Language::ClojureScript, vec![sym(0, "panel", "webapp.connections.views/panel", SymbolKind::Function, (3, 12))], vec![call(
                0,
                ":connections/get",
                RefKind::Dispatch,
                6,
                0,
            )]),
        ),
        (server, parsed(Language::Go, vec![sym(0, "GetConnection", "GetConnection", SymbolKind::Function, (1, 10))], Vec::new())),
    ])
    .await;
    let mut all = facts("gateway/api/conn.go", server, FrameworkId::GoNetHttp, vec![(
        20,
        None,
        route(Some("GET"), "/api/connections/{name}", None, named("GetConnection", None)),
    )]);
    all.extend(facts("src/webapp/connections/events.cljs", events, FrameworkId::CljsHttpMaps, vec![(8, Some(0), AppFact::HttpCall {
        method: Some("GET".into()),
        url: UrlTemplate { parts: vec![UrlPart::Lit("/api/connections/".into()), var("name")] },
        client: ":http-xhrio".into(),
    })]));

    let outcome = link(&*w.knowledge, world_org(), &key, &all, LinkOptions { reframe: true }).await.unwrap_or_else(|e| panic!("{e}"));
    let g = &outcome.graph;
    let endpoint = node(g, AppNodeKind::Endpoint, "GET /api/connections/{}");
    let event = node(g, AppNodeKind::Event, ":connections/get");
    let view = node(g, AppNodeKind::Component, "webapp.connections.views/panel");
    assert_eq!(edge(g, view.id, event.id, AppEdgeKind::Dispatches), 1.0);
    assert!(edge(g, event.id, endpoint.id, AppEdgeKind::CallsEndpoint) > 0.6);
    // Without re-frame detected, no events are built.
    let plain = link(&*w.knowledge, world_org(), &key, &all, LinkOptions::default()).await.unwrap_or_else(|e| panic!("{e}"));
    assert!(plain.graph.nodes.iter().all(|n| n.kind != AppNodeKind::Event));
}
