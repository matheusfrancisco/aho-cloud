//! `find_symbol`, `references`, `call_graph`, `explain`, `glossary`.

use mage_core::domain::{ArtifactId, Layer};
use mage_core::ports::knowledge::{
    KnowledgeStore, ScipDocRows, ScipMetadata, ScipOccurrence, ScipRange, ScipRoles, ScipSymbolInfo, SymbolKind,
};
use mage_core::wire::indexer::CommitInfo;
use serde_json::{Value, json};
use time::macros::datetime;
use uuid::Uuid;

use super::{Fx, assert_envelope, call, cite_ref, path, sym};
use crate::app::knowledge_query::ToolErrorCode;
use mage_core::ports::repos::RepoStore;
use crate::ports::indexer_link::IndexerLinkError;

/// `svc/helper.go`: `Helper` (3-5); `svc/run.go`: `Run` (3-6) calls `Helper` on line 4;
/// `cmd/main.go`: `main` (3-6) calls `Run` on line 4 and `Helper` on line 5.
async fn call_fixture(fx: &Fx) -> (mage_core::domain::BlobOid, mage_core::domain::BlobOid) {
    let helper = fx.file("svc/helper.go", "package svc\n\nfunc Helper() {\n\treturn\n}\n").await;
    fx.parsed(helper, vec![sym(0, "svc", SymbolKind::Module, (1, 1)), sym(1, "Helper", SymbolKind::Function, (3, 5))], vec![]).await;
    let run = fx.file("svc/run.go", "package svc\n\nfunc Run() {\n\tHelper()\n\treturn\n}\n").await;
    fx.parsed(run, vec![sym(0, "svc", SymbolKind::Module, (1, 1)), sym(1, "Run", SymbolKind::Function, (3, 6))], vec![call(0, "Helper", 4, Some(1))]).await;
    let main = fx.file("cmd/main.go", "package main\n\nfunc main() {\n\tsvc.Run()\n\tsvc.Helper()\n}\n").await;
    let mut run_call = call(0, "Run", 4, Some(1));
    run_call.qualifier = Some("svc".into());
    let mut helper_call = call(1, "Helper", 5, Some(1));
    helper_call.qualifier = Some("svc".into());
    fx.parsed(main, vec![sym(0, "main", SymbolKind::Module, (1, 1)), sym(1, "main", SymbolKind::Function, (3, 6))], vec![run_call, helper_call]).await;
    (helper, run)
}

fn refs(value: &Value) -> Vec<String> {
    value["items"].as_array().into_iter().flatten().filter_map(|i| i["citation"]["ref"].as_str().map(str::to_owned)).collect()
}

#[tokio::test]
async fn find_symbol_auto_mode_reports_the_match_and_splits_qualified_names() {
    let fx = Fx::new().await;
    call_fixture(&fx).await;
    let exact = fx.call("find_symbol", json!({ "name": "Helper" })).await.unwrap_or_else(|e| panic!("{e}"));
    assert_envelope(&exact, &fx, "Symbols");
    assert_eq!(refs(&exact), vec![cite_ref(&fx, "svc/helper.go", 3, 5)]);
    assert_eq!(exact["items"][0]["signature"], "func Helper()");
    assert!(exact["notes"].as_array().is_some_and(|n| n.contains(&json!("matched by exact"))));
    let qualified = fx.call("find_symbol", json!({ "name": "svc.Helper" })).await.unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(qualified["items"][0]["name"], "Helper");
    let prefix = fx.call("find_symbol", json!({ "name": "helpe" })).await.unwrap_or_else(|e| panic!("{e}"));
    assert!(prefix["notes"].as_array().is_some_and(|n| n.contains(&json!("matched by prefix"))), "{prefix}");
    fx.reset(Layer::Symbols).await;
    assert_eq!(fx.error("find_symbol", json!({ "name": "Helper" })).await, ToolErrorCode::LayerNotReady);
}

#[tokio::test]
async fn references_fall_back_to_name_refs_without_a_scip_index() {
    let fx = Fx::new().await;
    call_fixture(&fx).await;
    let got = fx.call("references", json!({ "symbol": "Helper" })).await.unwrap_or_else(|e| panic!("{e}"));
    assert_envelope(&got, &fx, "Symbols");
    assert_eq!(got["degraded"], json!({ "reason": "no SCIP index for this sha", "wanted": "Refs" }));
    assert_eq!(refs(&got), vec![cite_ref(&fx, "cmd/main.go", 5, 5), cite_ref(&fx, "svc/run.go", 4, 4)]);
    assert_eq!(got["items"][0]["precision"], "name");
    assert_eq!(got["items"][0]["line_text"], "\tsvc.Helper()");
    assert_eq!(got["items"][1]["enclosing"], json!({ "name": "Run", "kind": "function" }));
    // Paging by one returns the same refs.
    let first = fx.call("references", json!({ "symbol": "Helper", "limit": 1 })).await.unwrap_or_else(|e| panic!("{e}"));
    let second = fx.call("references", json!({ "symbol": "Helper", "limit": 1, "cursor": first["next_cursor"] })).await.unwrap_or_else(|e| panic!("{e}"));
    assert_eq!([refs(&first), refs(&second)].concat(), refs(&got));
    assert_eq!(second["next_cursor"], Value::Null);
}

#[tokio::test]
async fn references_are_precise_with_a_scip_index_at_the_sha() {
    let fx = Fx::new().await;
    let (helper, run) = call_fixture(&fx).await;
    let artifact = ArtifactId::from_uuid(Uuid::from_u128(0x0d01));
    let symbol = "scip-go gomod example.com/app v1 `example.com/app/svc`/Helper().";
    let occ = |line: u32, col: u32, roles: u32| ScipOccurrence {
        range: ScipRange { start_line: line, start_col: col, end_line: line, end_col: col + 6 },
        symbol: symbol.into(),
        roles: ScipRoles(roles),
        enclosing: None,
    };
    let key = fx.key();
    let meta = ScipMetadata { tool_name: "scip-go".into(), tool_version: "0.1".into(), project_root: "file:///src".into() };
    let k = &fx.knowledge;
    k.begin_scip_index(fx.org, &key, artifact, &meta).await.unwrap_or_else(|e| panic!("{e}"));
    let docs = vec![
        ScipDocRows { path: path("svc/helper.go"), blob: helper, occurrences: vec![occ(3, 5, 1)] },
        ScipDocRows { path: path("svc/run.go"), blob: run, occurrences: vec![occ(4, 1, 8)] },
    ];
    k.put_scip_documents(fx.org, artifact, &docs).await.unwrap_or_else(|e| panic!("{e}"));
    let info = ScipSymbolInfo { symbol: symbol.into(), display_name: Some("Helper".into()), kind: None, documentation: None, relationships: vec![] };
    k.put_scip_symbols(fx.org, artifact, &[info]).await.unwrap_or_else(|e| panic!("{e}"));
    k.finish_scip_index(fx.org, artifact, &["go".to_owned()], 2).await.unwrap_or_else(|e| panic!("{e}"));
    fx.repos
        .apply_layer_event(fx.org, key, Layer::Refs, mage_core::domain::LayerEvent::Reset, &[])
        .await
        .unwrap_or_else(|e| panic!("{e}"));
    fx.ready(Layer::Refs).await;

    let got = fx.call("references", json!({ "symbol": "Helper" })).await.unwrap_or_else(|e| panic!("{e}"));
    assert_envelope(&got, &fx, "Refs");
    assert_eq!(got["degraded"], Value::Null);
    assert_eq!(refs(&got), vec![cite_ref(&fx, "svc/run.go", 4, 4)], "SCIP knows the main.go call is not this symbol's");
    assert_eq!(got["items"][0]["precision"], "precise");
    let by_symbol = fx.call("references", json!({ "symbol": symbol })).await.unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(refs(&by_symbol), refs(&got), "a SCIP symbol string works directly");
    let found = fx.call("find_symbol", json!({ "name": "Helper" })).await.unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(found["items"][0]["scip_symbol"], symbol);
}

#[tokio::test]
async fn call_graph_walks_callers_by_depth() {
    let fx = Fx::new().await;
    call_fixture(&fx).await;
    let one = fx.call("call_graph", json!({ "symbol": "Helper" })).await.unwrap_or_else(|e| panic!("{e}"));
    assert_envelope(&one, &fx, "Symbols");
    assert_eq!(one["precision"], "name");
    let names: Vec<&str> = one["nodes"].as_array().into_iter().flatten().filter_map(|n| n["name"].as_str()).collect();
    assert_eq!(names[0], "Helper", "the root is node 0");
    assert!(names.contains(&"Run") && names.contains(&"main"), "{names:?}");
    let edges = one["edges"].as_array().cloned().unwrap_or_default();
    assert!(edges.iter().all(|e| e["to"] == 0), "callers point at the root: {edges:?}");
    assert!(edges.iter().any(|e| e["sites"][0]["ref"] == cite_ref(&fx, "svc/run.go", 4, 4)));
    let callees = fx.call("call_graph", json!({ "path": "cmd/main.go", "line": 4, "direction": "callees" })).await.unwrap_or_else(|e| panic!("{e}"));
    let names: Vec<&str> = callees["nodes"].as_array().into_iter().flatten().filter_map(|n| n["name"].as_str()).collect();
    assert_eq!(names[0], "main");
    assert!(names.contains(&"Run") && names.contains(&"Helper"));
    let capped = fx.call("call_graph", json!({ "symbol": "Helper", "limit": 2 })).await.unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(capped["nodes"].as_array().map(Vec::len), Some(2));
}

#[tokio::test]
async fn explain_returns_a_labeled_fact_sheet_without_summaries() {
    let fx = Fx::new().await;
    call_fixture(&fx).await;
    let commit = CommitInfo {
        sha: fx.sha,
        tree: mage_core::domain::TreeOid::from_bytes([9; 20]),
        parents: vec![],
        author_name: "Ada".into(),
        author_time: datetime!(2026-03-01 10:00 UTC),
        committer_time: datetime!(2026-03-01 10:00 UTC),
        subject: "add helper".into(),
        body: String::new(),
        files: Some(vec![mage_core::wire::indexer::FileChange {
            path: path("svc/helper.go"),
            old_path: None,
            change: mage_core::wire::indexer::ChangeKind::Added,
            adds: Some(5),
            dels: Some(0),
        }]),
    };
    fx.indexer.put_commits(fx.org, fx.repo, vec![commit]);
    let got = fx.call("explain", json!({ "symbol": "Helper" })).await.unwrap_or_else(|e| panic!("{e}"));
    assert_envelope(&got, &fx, "Symbols");
    assert_eq!(got["degraded"]["wanted"], "Semantic");
    assert!(got.get("summary").is_none(), "nothing is generated");
    assert_eq!(got["facts"]["definition"]["citation"]["ref"], cite_ref(&fx, "svc/helper.go", 3, 5));
    assert_eq!(got["facts"]["calls"]["callers"], 2);
    assert_eq!(got["facts"]["recent_commits"][0]["subject"], "add helper");
    let outline = fx.call("explain", json!({ "path": "svc/run.go" })).await.unwrap_or_else(|e| panic!("{e}"));
    let names: Vec<&str> = outline["facts"]["outline"].as_array().into_iter().flatten().filter_map(|s| s["name"].as_str()).collect();
    assert_eq!(names, vec!["svc", "Run"]);
    fx.indexer.fail_with(IndexerLinkError::Timeout);
    let without_history = fx.call("explain", json!({ "symbol": "Run" })).await.unwrap_or_else(|e| panic!("{e}"));
    assert!(without_history["facts"].get("recent_commits").is_none());
    assert!(without_history["notes"].as_array().is_some_and(|n| n.iter().any(|n| n.as_str().is_some_and(|s| s.starts_with("recent commits omitted")))));
}

#[tokio::test]
async fn glossary_groups_terms_and_respects_layer_readiness() {
    let fx = Fx::new().await;
    let oid = fx.file("api/connections.go", "package api\n\ntype Connection struct{}\nfunc ListConnections() {}\n").await;
    fx.parsed(oid, vec![sym(0, "Connection", SymbolKind::Struct, (3, 3)), sym(1, "ListConnections", SymbolKind::Function, (4, 4))], vec![]).await;
    let got = fx.call("glossary", json!({ "term": "connections" })).await.unwrap_or_else(|e| panic!("{e}"));
    assert_envelope(&got, &fx, "Symbols");
    let terms: Vec<&str> = got["items"].as_array().into_iter().flatten().filter_map(|i| i["term"].as_str()).collect();
    assert!(terms.contains(&"Connection"), "{terms:?}");
    let item = got["items"].as_array().into_iter().flatten().find(|i| i["term"] == "Connection").cloned().unwrap_or_default();
    assert_eq!(item["sources"], json!(["type"]));
    assert_eq!(item["refs"][0]["ref"], cite_ref(&fx, "api/connections.go", 3, 3));
    fx.reset(Layer::AppMap).await;
    let degraded = fx.call("glossary", json!({ "term": "connection" })).await.unwrap_or_else(|e| panic!("{e}"));
    assert!(degraded["notes"].as_array().is_some_and(|n| n.iter().any(|n| n.as_str().is_some_and(|s| s.starts_with("AppMap terms omitted")))));
}
