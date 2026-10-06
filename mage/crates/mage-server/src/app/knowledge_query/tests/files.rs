//! `read_file`, `search_code`, `repo_overview`, the size cap and cursors.

use mage_core::domain::{BlobOid, EntryKind, FileClass, Layer, ManifestEntry};
use mage_core::ports::knowledge::SymbolKind;
use serde_json::{Value, json};

use super::{Fx, assert_envelope, cite_ref, path, sym};
use crate::app::knowledge_query::envelope::MAX_RESULT_BYTES;
use crate::app::knowledge_query::{CallError, ToolErrorCode};
use crate::ports::indexer_link::IndexerLinkError;

fn numbered(n: u32) -> String {
    (1..=n).map(|i| format!("line {i}\n")).collect()
}

#[tokio::test]
async fn read_file_returns_numbered_lines_with_a_full_sha_citation() {
    let fx = Fx::new().await;
    fx.file("src/app.go", "package main\r\n\nfunc main() {}\n").await;
    let got = fx.call("read_file", json!({ "path": "src/app.go" })).await.unwrap_or_else(|e| panic!("{e}"));
    assert_envelope(&got, &fx, "Files");
    assert_eq!(got["text"], "     1\tpackage main\n     2\t\n     3\tfunc main() {}");
    assert_eq!((got["start_line"].as_u64(), got["end_line"].as_u64(), got["total_lines"].as_u64()), (Some(1), Some(3), Some(3)));
    assert_eq!(got["citation"]["ref"], cite_ref(&fx, "src/app.go", 1, 3));
    assert_eq!(got["next_cursor"], Value::Null);
    assert_eq!(got["lang"], "go");
    assert_eq!(got["class"], "source");
}

#[tokio::test]
async fn read_file_windows_at_400_lines_and_the_cursor_continues() {
    let fx = Fx::new().await;
    fx.file("big.txt", &numbered(450)).await;
    let first = fx.call("read_file", json!({ "path": "big.txt" })).await.unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(first["end_line"], 400);
    let cursor = first["next_cursor"].as_str().unwrap_or_else(|| panic!("cursor: {first}")).to_owned();
    let second = fx.call("read_file", json!({ "path": "big.txt", "cursor": cursor })).await.unwrap_or_else(|e| panic!("{e}"));
    assert_eq!((second["start_line"].as_u64(), second["end_line"].as_u64()), (Some(401), Some(450)));
    assert!(second["text"].as_str().is_some_and(|t| t.starts_with("   401\tline 401")));
    assert_eq!(second["next_cursor"], Value::Null);
    // An explicit end bounds the window and the continuation.
    let bounded = fx.call("read_file", json!({ "path": "big.txt", "start_line": 10, "end_line": 12 })).await.unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(bounded["text"], "    10\tline 10\n    11\tline 11\n    12\tline 12");
    assert_eq!(bounded["next_cursor"], Value::Null);
    // A cursor of this call does not fit another call's arguments.
    let other = fx.call("read_file", json!({ "path": "big.txt", "start_line": 2, "cursor": first["next_cursor"] })).await;
    assert!(matches!(&other, Err(CallError::Tool(e)) if e.code == ToolErrorCode::BadCursor), "{other:?}");
    assert_eq!(fx.error("read_file", json!({ "path": "big.txt", "start_line": 451 })).await, ToolErrorCode::InvalidArgument);
}

#[tokio::test]
async fn read_file_caps_bytes_and_cuts_long_lines() {
    let fx = Fx::new().await;
    let long: String = (0..60).map(|i| format!("{i:04}{}\n", "x".repeat(2496))).collect();
    fx.file("wide.txt", &long).await;
    let got = fx.call("read_file", json!({ "path": "wide.txt" })).await.unwrap_or_else(|e| panic!("{e}"));
    let text = got["text"].as_str().unwrap_or_default();
    assert!(text.lines().all(|l| l.ends_with("…[+500 chars]")), "lines over 2000 chars are cut with the marker");
    let shown = got["end_line"].as_u64().unwrap_or(0);
    assert!(shown > 0 && shown < 60, "byte cap stops early: {shown}");
    assert!(serde_json::to_vec(&got).map_or(usize::MAX, |v| v.len()) <= MAX_RESULT_BYTES);
    assert_eq!(got["truncated"], true);
    let cursor = got["next_cursor"].clone();
    let next = fx.call("read_file", json!({ "path": "wide.txt", "cursor": cursor })).await.unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(next["start_line"].as_u64(), Some(shown + 1));
}

#[tokio::test]
async fn read_file_lists_directories_and_refuses_non_text() {
    let fx = Fx::new().await;
    fx.file("src/a.go", "package a\n").await;
    fx.file("src/sub/b.go", "package b\n").await;
    fx.file("README.md", "# hi\n").await;
    let root = fx.call("read_file", json!({ "path": "" })).await.unwrap_or_else(|e| panic!("{e}"));
    let names: Vec<&str> = root["entries"].as_array().into_iter().flatten().filter_map(|e| e["name"].as_str()).collect();
    assert_eq!(names, vec!["README.md", "src"]);
    let src = fx.call("read_file", json!({ "path": "src/" })).await.unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(src["path"], "src");
    assert_eq!(src["entries"][1]["kind"], "dir");
    assert_eq!(fx.error("read_file", json!({ "path": "nope" })).await, ToolErrorCode::NotFound);

    let binary = BlobOid::from_bytes([0xb1; 20]);
    fx.put_blob(binary, &[0, 1, 2], false, false).await;
    fx.put_entry(entry("logo.png", binary, EntryKind::File, 3)).await;
    assert_eq!(fx.error("read_file", json!({ "path": "logo.png" })).await, ToolErrorCode::NotText);
    let sub = BlobOid::from_bytes([0xb2; 20]);
    fx.put_entry(entry("vendor/lib", sub, EntryKind::Submodule, 0)).await;
    assert_eq!(fx.error("read_file", json!({ "path": "vendor/lib" })).await, ToolErrorCode::NotText);
    let huge = BlobOid::from_bytes([0xb3; 20]);
    fx.put_blob(huge, b"x\n", true, false).await;
    let mut big = entry("dump.sql", huge, EntryKind::File, 3 * 1024 * 1024);
    big.is_text = true;
    fx.put_entry(big).await;
    assert_eq!(fx.error("read_file", json!({ "path": "dump.sql" })).await, ToolErrorCode::TooLarge);
    let link = BlobOid::from_bytes([0xb4; 20]);
    fx.put_blob(link, b"src/a.go", true, true).await;
    let mut symlink = entry("current", link, EntryKind::Symlink, 8);
    symlink.is_text = true;
    fx.put_entry(symlink).await;
    let got = fx.call("read_file", json!({ "path": "current" })).await.unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(got["text"], "src/a.go");
    assert!(got["notes"].as_array().is_some_and(|n| n.iter().any(|n| n.as_str().is_some_and(|s| s.contains("symlink")))));
}

fn entry(p: &str, blob: BlobOid, kind: EntryKind, size: u64) -> ManifestEntry {
    ManifestEntry { path: path(p), kind, executable: false, blob, lang: None, class: FileClass::Data, size, is_text: false, line_count: 0 }
}

#[tokio::test]
async fn search_code_text_hits_carry_enclosing_symbols_and_page_by_keyset() {
    let fx = Fx::new().await;
    let oid = fx.file("api/conn.go", "package api\n\nfunc Connect() {\n\treturn ErrNotFound\n}\n").await;
    fx.parsed(oid, vec![sym(0, "Connect", SymbolKind::Function, (3, 5))], vec![]).await;
    fx.file("api/other.go", "var ErrNotFound = errors.New(\"x\")\n// ErrNotFound again\n").await;
    fx.file_with("gen/api.pb.go", "ErrNotFound\n", FileClass::Generated).await;
    let all = fx.call("search_code", json!({ "query": "ErrNotFound", "limit": 50 })).await.unwrap_or_else(|e| panic!("{e}"));
    assert_envelope(&all, &fx, "Files");
    let refs: Vec<&str> = all["items"].as_array().into_iter().flatten().filter_map(|i| i["citation"]["ref"].as_str()).collect();
    assert_eq!(refs, vec![cite_ref(&fx, "api/conn.go", 4, 4), cite_ref(&fx, "api/other.go", 1, 1), cite_ref(&fx, "api/other.go", 2, 2)]);
    assert_eq!(all["items"][0]["symbol"], json!({ "name": "Connect", "kind": "function" }));
    assert_eq!(all["items"][0]["line_text"], "\treturn ErrNotFound");

    // Paging with limit 1 visits the same hits in order.
    let mut paged = Vec::new();
    let mut cursor = Value::Null;
    for _ in 0..5 {
        let page = fx.call("search_code", json!({ "query": "ErrNotFound", "limit": 1, "cursor": cursor })).await.unwrap_or_else(|e| panic!("{e}"));
        paged.extend(page["items"].as_array().into_iter().flatten().filter_map(|i| i["citation"]["ref"].as_str().map(str::to_owned)));
        cursor = page["next_cursor"].clone();
        if cursor.is_null() {
            break;
        }
    }
    assert_eq!(paged, refs);
    let generated = fx.call("search_code", json!({ "query": "ErrNotFound", "include_generated": true, "limit": 50 })).await.unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(generated["items"].as_array().map(Vec::len), Some(4));
}

#[tokio::test]
async fn search_code_degrades_when_the_indexer_is_down() {
    let fx = Fx::new().await;
    let oid = fx.file("api/connections.go", "package api\n\nfunc ListConnections() {}\n").await;
    fx.parsed(oid, vec![sym(0, "ListConnections", SymbolKind::Function, (3, 3))], vec![]).await;
    fx.indexer.fail_with(IndexerLinkError::Unavailable("down".into()));
    let got = fx.call("search_code", json!({ "query": "connections" })).await.unwrap_or_else(|e| panic!("{e}"));
    assert_envelope(&got, &fx, "Symbols");
    assert_eq!(got["degraded"], json!({ "reason": "text_search_unavailable", "wanted": "Files" }));
    let refs: Vec<&str> = got["items"].as_array().into_iter().flatten().filter_map(|i| i["citation"]["ref"].as_str()).collect();
    assert!(refs.contains(&cite_ref(&fx, "api/connections.go", 3, 3).as_str()), "symbol match: {got}");
    assert!(refs.contains(&cite_ref(&fx, "api/connections.go", 1, 1).as_str()), "path match: {got}");
    assert_eq!(fx.error("search_code", json!({ "query": "conn.*", "mode": "regex" })).await, ToolErrorCode::Unavailable);
    fx.indexer.heal();
    assert_eq!(fx.error("search_code", json!({ "query": "(unclosed", "mode": "regex" })).await, ToolErrorCode::InvalidArgument);
}

#[tokio::test]
async fn search_code_symbol_and_path_modes() {
    let fx = Fx::new().await;
    let oid = fx.file("api/conn.go", "package api\n\nfunc Connect() {}\n").await;
    fx.parsed(oid, vec![sym(0, "Connect", SymbolKind::Function, (3, 3))], vec![]).await;
    fx.file("web/conn.ts", "export {}\n").await;
    let symbols = fx.call("search_code", json!({ "query": "Conect", "mode": "symbol" })).await.unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(symbols["layer"], "Symbols");
    assert_eq!(symbols["items"][0]["symbol"]["name"], "Connect");
    let paths = fx.call("search_code", json!({ "query": "**/*.ts", "mode": "path" })).await.unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(paths["items"].as_array().map(Vec::len), Some(1));
    assert_eq!(paths["items"][0]["citation"]["ref"], cite_ref(&fx, "web/conn.ts", 1, 1));
    fx.reset(Layer::Symbols).await;
    assert_eq!(fx.error("search_code", json!({ "query": "Connect", "mode": "symbol" })).await, ToolErrorCode::LayerNotReady);
}

#[tokio::test]
async fn results_over_24_kib_are_cut_and_resume_after_the_last_item() {
    let fx = Fx::new().await;
    let line = format!("needle {}", "y".repeat(230));
    let text: String = (0..50).map(|_| format!("{line}\n")).collect();
    fx.file("a.txt", &text).await;
    fx.file("b.txt", &text).await;
    let first = fx.call("search_code", json!({ "query": "needle", "limit": 50, "context": 3 })).await.unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(first["truncated"], true);
    assert!(serde_json::to_vec(&first).map_or(usize::MAX, |v| v.len()) <= MAX_RESULT_BYTES);
    let kept = first["items"].as_array().map_or(0, Vec::len);
    assert!(kept > 0 && kept < 50, "{kept}");
    let next = fx.call("search_code", json!({ "query": "needle", "limit": 50, "context": 3, "cursor": first["next_cursor"] })).await.unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(next["items"][0]["citation"]["start_line"].as_u64(), Some(kept as u64 + 1), "resumes right after the cut");
}

#[tokio::test]
async fn repo_overview_reports_layers_entry_points_and_recipe() {
    let fx = Fx::new().await;
    let main = fx.file("cmd/server/main.go", "package main\n\nfunc main() {}\n").await;
    fx.parsed(main, vec![sym(0, "main", SymbolKind::Module, (1, 1)), sym(1, "main", SymbolKind::Function, (3, 3))], vec![]).await;
    fx.file("go.mod", "module x\n").await;
    fx.file("README.md", "# x\n").await;
    fx.file("docs/README.md", "# docs\n").await;
    let got = fx.call("repo_overview", json!({})).await.unwrap_or_else(|e| panic!("{e}"));
    assert_envelope(&got, &fx, "Files");
    let overview = &got["overview"];
    assert_eq!(overview["layers"]["Files"], "Ready");
    assert_eq!(overview["layers"]["Refs"], "Skipped: no recipe");
    let entry_refs: Vec<&str> = overview["entry_points"].as_array().into_iter().flatten().filter_map(|e| e["citation"]["ref"].as_str()).collect();
    for want in [cite_ref(&fx, "go.mod", 1, 1), cite_ref(&fx, "README.md", 1, 1), cite_ref(&fx, "docs/README.md", 1, 1), cite_ref(&fx, "cmd/server/main.go", 3, 3)] {
        assert!(entry_refs.contains(&want.as_str()), "{want} in {entry_refs:?}");
    }
    assert_eq!(overview["recipe"], Value::Null);
    assert_eq!(overview["brief"], Value::Null);
    assert_eq!(overview["counts"]["files"], 4);
}
