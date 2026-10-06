//! `lookup_error`, `app_map`, `route_info`.

use mage_core::domain::{BlobOid, Layer};
use mage_core::ports::knowledge::{
    AppEdge, AppEdgeKind, AppGraph, AppNode, AppNodeKind, ErrorSiteKind, ErrorSiteRecord, ErrorsRev, FrameworkId, KnowledgeStore, SymbolKind,
    SymbolRef,
};
use serde_json::json;

use super::{Fx, REV, assert_envelope, cite_ref, path, sym};
use crate::app::knowledge_query::ToolErrorCode;

const CONN_GO: &str = "package api\n\nfunc GetConnection(id string) error {\n\tif missing {\n\t\treturn fmt.Errorf(\"connection %s not found\", id)\n\t}\n\treturn nil\n}\n";

/// `api/conn.go`: `GetConnection` (3-8) with a Throw site on line 5; `GET /api/connections/{}`
/// handled by it; a client call in `web/api.ts`.
pub(super) async fn app_fixture(fx: &Fx) -> BlobOid {
    let conn = fx.file("api/conn.go", CONN_GO).await;
    fx.parsed(conn, vec![sym(0, "GetConnection", SymbolKind::Function, (3, 8))], vec![]).await;
    fx.file("api/routes.go", "package api\n\nfunc Routes(r *gin.Engine) {\n\tr.GET(\"/api/connections/:id\", GetConnection)\n}\n").await;
    fx.file("web/api.ts", "export const get = (id: string) =>\n  fetch(\"/api/connections/\" + id);\n").await;
    let site = ErrorSiteRecord {
        ord: 0,
        literal_ord: 0,
        kind: ErrorSiteKind::Throw,
        level: None,
        template: "connection {} not found".into(),
        pattern: r"\bconnection\s+.+?\s+not\s+found\b".into(),
        tokens: vec!["connection".into(), "found".into(), "not".into()],
        specificity: 18,
        pos: (5, 10),
        enclosing: Some(0),
        http_status: None,
        i18n_key: None,
    };
    fx.knowledge.put_error_sites(fx.org, ErrorsRev(REV), &[(conn, vec![site])]).await.unwrap_or_else(|e| panic!("{e}"));
    let handler = SymbolRef { path: path("api/conn.go"), blob: conn, ord: 0 };
    let nodes = vec![
        AppNode {
            id: 1,
            kind: AppNodeKind::Endpoint,
            key: "GET /api/connections/{}".into(),
            label: "GET /api/connections/:id".into(),
            framework: Some(FrameworkId::Gin),
            at: Some((path("api/routes.go"), 4)),
            symbol: None,
        },
        AppNode {
            id: 2,
            kind: AppNodeKind::Handler,
            key: "GetConnection".into(),
            label: "GetConnection".into(),
            framework: Some(FrameworkId::Gin),
            at: Some((path("api/conn.go"), 3)),
            symbol: Some(handler),
        },
        AppNode {
            id: 3,
            kind: AppNodeKind::ClientCall,
            key: "ANY /api/connections/{}".into(),
            label: "fetch".into(),
            framework: Some(FrameworkId::JsFetch),
            at: Some((path("web/api.ts"), 2)),
            symbol: None,
        },
        AppNode { id: 4, kind: AppNodeKind::Table, key: "connections".into(), label: "connections".into(), framework: None, at: None, symbol: None },
    ];
    let edges = vec![
        AppEdge { from: 1, to: 2, kind: AppEdgeKind::HandledBy, confidence: 1.0, evidence: Some((path("api/routes.go"), 4)) },
        AppEdge { from: 3, to: 1, kind: AppEdgeKind::CallsEndpoint, confidence: 0.67, evidence: None },
        AppEdge { from: 2, to: 4, kind: AppEdgeKind::Reads, confidence: 1.0, evidence: None },
    ];
    fx.knowledge.replace_app_graph(fx.org, &fx.key(), &AppGraph { nodes, edges }).await.unwrap_or_else(|e| panic!("{e}"));
    conn
}

#[tokio::test]
async fn lookup_error_finds_verified_sites_with_symbol_and_route() {
    let fx = Fx::new().await;
    app_fixture(&fx).await;
    let text = "time=2026-05-01 level=error msg=\"connection pg-prod not found\" request_id=7";
    let got = fx.call("lookup_error", json!({ "text": text })).await.unwrap_or_else(|e| panic!("{e}"));
    assert_envelope(&got, &fx, "ErrorSites");
    let item = &got["items"][0];
    assert_eq!(item["verified"], true, "{got}");
    assert_eq!(item["citation"]["ref"], cite_ref(&fx, "api/conn.go", 5, 5));
    assert_eq!(item["template"], "connection {} not found");
    assert_eq!(item["enclosing"]["name"], "GetConnection");
    assert_eq!(item["routes"][0]["method"], "GET");
    assert_eq!(item["routes"][0]["path"], "/api/connections/{}");
    let none = fx.call("lookup_error", json!({ "text": "everything is fine" })).await.unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(none["items"], json!([]));
}

#[tokio::test]
async fn lookup_error_without_error_sites_searches_the_longest_literal_run() {
    let fx = Fx::new().await;
    fx.file("api/settings.go", "return fmt.Errorf(\"failed fetching connection settings for %d\", id)\n").await;
    fx.reset(Layer::ErrorSites).await;
    let got = fx.call("lookup_error", json!({ "text": "failed fetching connection settings for 42" })).await.unwrap_or_else(|e| panic!("{e}"));
    assert_envelope(&got, &fx, "Files");
    assert_eq!(got["degraded"]["wanted"], "ErrorSites");
    assert_eq!(got["items"][0]["citation"]["ref"], cite_ref(&fx, "api/settings.go", 1, 1));
    assert_eq!(got["items"][0]["verified"], false);
}

#[tokio::test]
async fn app_map_site_map_focus_and_skip_reason() {
    let fx = Fx::new().await;
    app_fixture(&fx).await;
    let site = fx.call("app_map", json!({})).await.unwrap_or_else(|e| panic!("{e}"));
    assert_envelope(&site, &fx, "AppMap");
    let kinds: Vec<&str> = site["nodes"].as_array().into_iter().flatten().filter_map(|n| n["kind"].as_str()).collect();
    assert_eq!(kinds, vec!["endpoint", "table"], "default site map: pages, endpoints, tables");
    assert_eq!(site["coverage"], json!({ "endpoints_with_handler": "1/1", "client_calls_linked": "1/1" }));
    let focused = fx.call("app_map", json!({ "focus": { "kind": "route", "value": "/api/connections/42" } })).await.unwrap_or_else(|e| panic!("{e}"));
    let ids: Vec<u64> = focused["nodes"].as_array().into_iter().flatten().filter_map(|n| n["id"].as_u64()).collect();
    assert!(ids.contains(&1) && ids.contains(&2) && ids.contains(&3), "{focused}");
    assert!(focused["edges"].as_array().is_some_and(|e| e.iter().any(|e| e["kind"] == "handled_by")));
    assert_eq!(fx.error("app_map", json!({ "focus": { "kind": "table", "value": "missing" } })).await, ToolErrorCode::NotFound);

    fx.reset(Layer::AppMap).await;
    fx.skip(Layer::AppMap, "no supported framework").await;
    match fx.call("app_map", json!({})).await {
        Err(crate::app::knowledge_query::CallError::Tool(e)) => {
            assert_eq!(e.code, ToolErrorCode::LayerNotReady);
            assert!(e.message.contains("no supported framework"), "{}", e.message);
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn route_info_resolves_handler_callers_tables_and_errors() {
    let fx = Fx::new().await;
    app_fixture(&fx).await;
    let got = fx.call("route_info", json!({ "url": "https://app.example.com/api/connections/42?tab=x", "method": "get" })).await.unwrap_or_else(|e| panic!("{e}"));
    assert_envelope(&got, &fx, "AppMap");
    let item = &got["items"][0];
    assert_eq!(item["route"]["method"], "GET");
    assert_eq!(item["route"]["pattern"], "/api/connections/{}");
    assert_eq!(item["route"]["citation"]["ref"], cite_ref(&fx, "api/routes.go", 4, 4));
    assert_eq!(item["handler"]["name"], "GetConnection");
    assert_eq!(item["callers"][0]["citation"]["ref"], cite_ref(&fx, "web/api.ts", 2, 2));
    assert_eq!(item["touches"][0]["key"], "connections");
    assert_eq!(item["error_sites"][0]["template"], "connection {} not found");
    let nothing = fx.call("route_info", json!({ "url": "/billing/invoices" })).await.unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(nothing["items"], json!([]));

    fx.reset(Layer::AppMap).await;
    let degraded = fx.call("route_info", json!({ "url": "/api/connections/42/edit" })).await.unwrap_or_else(|e| panic!("{e}"));
    assert_envelope(&degraded, &fx, "Files");
    assert_eq!(degraded["degraded"]["wanted"], "AppMap");
    let refs: Vec<&str> = degraded["items"].as_array().into_iter().flatten().filter_map(|i| i["citation"]["ref"].as_str()).collect();
    assert!(refs.contains(&cite_ref(&fx, "web/api.ts", 2, 2).as_str()), "literal search for /api/connections: {degraded}");
}
