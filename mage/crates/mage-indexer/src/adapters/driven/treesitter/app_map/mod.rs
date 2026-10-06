//! App-map framework extractors (knowledge.md "Layer 4: App map").
//!
//! [`detect_frameworks`] decides which frameworks apply to which project roots from dependency
//! manifests; [`extract`] runs the matching [`AppExtractor`]s over one parsed file and returns its
//! [`AppFactRecord`]s. The route grouping conventions (`RouteGroup` / `RouteDecl`) are consumed by
//! the app-map linker; see `go.rs`.

mod cljs;
mod detect;
mod go;
mod js;
mod sql;
mod url;

use std::collections::BTreeMap;

use mage_core::domain::{Language, RepoPath};
use mage_core::ports::knowledge::{AppFact, AppFactRecord, AppMapRev, FrameworkId, FrameworkScope, SymbolDef, SymbolKind};

pub(crate) const APP_MAP_REV: AppMapRev = AppMapRev(1);

/// Upper bound on records per file.
const MAX_RECORDS: usize = 10_000;

/// A dependency manifest (`go.mod`, `package.json`, …) handed to [`AppExtractor::detect`].
pub(crate) struct ManifestFile<'a> {
    pub path: &'a RepoPath,
    pub bytes: &'a [u8],
}

pub(crate) trait AppExtractor: Send + Sync {
    fn id(&self) -> FrameworkId;
    fn languages(&self) -> &'static [Language];
    /// Inspect dependency manifests directly in one project root (None = repo root).
    fn detect(&self, root: Option<&RepoPath>, manifests: &[ManifestFile<'_>]) -> bool;
    fn extract(&self, path: &RepoPath, tree: &tree_sitter::Tree, src: &[u8], symbols: &[SymbolDef], out: &mut Vec<AppFactRecord>);
}

/// Static registry, one extractor per FrameworkId.
pub(crate) static EXTRACTORS: &[&dyn AppExtractor] = &[
    &go::GoNetHttp,
    &go::Gin,
    &go::Chi,
    &go::GorillaMux,
    &go::Echo,
    &js::JsFetch,
    &js::Axios,
    &js::ReactRouter,
    &js::NextRoutes,
    &cljs::ReFrame,
    &cljs::CljsHttpMaps,
    &cljs::Reitit,
    &sql::SqlStrings,
];

/// File names [`detect_frameworks`] treats as dependency manifests.
const MANIFEST_NAMES: &[&str] = &[
    "go.mod",
    "package.json",
    "deps.edn",
    "shadow-cljs.edn",
    "project.clj",
    "pyproject.toml",
    "requirements.txt",
    "Cargo.toml",
];

/// manifests = (path, bytes) of go.mod, package.json, deps.edn, shadow-cljs.edn, project.clj, pyproject.toml,
/// requirements.txt, Cargo.toml at any depth. Group by parent dir; run each extractor's detect per root.
/// Sorted, deduped output. SqlStrings is added for every root that has any manifest.
pub(crate) fn detect_frameworks(manifests: &[(RepoPath, Vec<u8>)]) -> Vec<FrameworkScope> {
    let mut roots: BTreeMap<Option<RepoPath>, Vec<ManifestFile<'_>>> = BTreeMap::new();
    for (path, bytes) in manifests {
        if !MANIFEST_NAMES.contains(&path.file_name()) {
            continue;
        }
        let root = match path.parent() {
            None => None,
            Some(dir) => match RepoPath::new(dir) {
                Ok(dir) => Some(dir),
                Err(_) => continue,
            },
        };
        roots.entry(root).or_default().push(ManifestFile { path, bytes });
    }
    let mut out = Vec::new();
    for (root, files) in &roots {
        for extractor in EXTRACTORS {
            if extractor.id() == FrameworkId::SqlStrings || extractor.detect(root.as_ref(), files) {
                out.push(FrameworkScope { root: root.clone(), framework: extractor.id() });
            }
        }
    }
    out.sort_by(|a, b| (&a.root, a.framework).cmp(&(&b.root, b.framework)));
    out.dedup();
    out
}

/// Runs every registered extractor whose id ∈ frameworks and lang ∈ languages(). Records sorted by (line, extractor
/// registry order, emission order); `ord` = 0.. in that order; at most 10_000 records.
pub(crate) fn extract(
    lang: Language,
    path: &RepoPath,
    tree: &tree_sitter::Tree,
    src: &[u8],
    symbols: &[SymbolDef],
    frameworks: &[FrameworkId],
) -> Vec<AppFactRecord> {
    let mut tagged: Vec<(u32, usize, usize, AppFactRecord)> = Vec::new();
    let mut buf = Vec::new();
    for (reg, extractor) in EXTRACTORS.iter().enumerate() {
        if !frameworks.contains(&extractor.id()) || !extractor.languages().contains(&lang) {
            continue;
        }
        buf.clear();
        extractor.extract(path, tree, src, symbols, &mut buf);
        tagged.extend(buf.drain(..).enumerate().map(|(i, r)| (r.line, reg, i, r)));
    }
    tagged.sort_by_key(|&(line, reg, i, _)| (line, reg, i));
    tagged.truncate(MAX_RECORDS);
    tagged
        .into_iter()
        .zip(0u32..)
        .map(|((_, _, _, mut rec), ord)| {
            rec.ord = ord;
            rec
        })
        .collect()
}

/// `ord` of the innermost symbol whose span contains `line`, preferring Function/Method/Test kinds.
pub(super) fn enclosing_ord(symbols: &[SymbolDef], line: u32) -> Option<u32> {
    let innermost = |preferred: bool| {
        symbols
            .iter()
            .filter(|s| s.span.contains(line))
            .filter(|s| !preferred || matches!(s.kind, SymbolKind::Function | SymbolKind::Method | SymbolKind::Test))
            .min_by_key(|s| (s.span.len(), std::cmp::Reverse(s.span.start()), std::cmp::Reverse(s.ord)))
            .map(|s| s.ord)
    };
    innermost(true).or_else(|| innermost(false))
}

/// A record with `ord` 0 (assigned by [`extract`]) and its enclosing symbol resolved.
pub(super) fn record(framework: FrameworkId, fact: AppFact, line: u32, symbols: &[SymbolDef]) -> AppFactRecord {
    AppFactRecord { ord: 0, framework, fact, line, enclosing: enclosing_ord(symbols, line) }
}

/// 1-based start line of a node.
pub(super) fn line_of(node: tree_sitter::Node<'_>) -> u32 {
    u32::try_from(node.start_position().row).unwrap_or(u32::MAX - 1) + 1
}

/// Source text of a node (empty on invalid UTF-8).
pub(super) fn text<'s>(node: tree_sitter::Node<'_>, src: &'s [u8]) -> &'s str {
    node.utf8_text(src).unwrap_or("")
}

/// Node text with whitespace runs collapsed, truncated to 64 chars (Var names, middleware, receivers).
pub(super) fn snippet(node: tree_sitter::Node<'_>, src: &[u8]) -> String {
    let mut out = String::new();
    for (i, word) in text(node, src).split_whitespace().enumerate() {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(word);
    }
    match out.char_indices().nth(64) {
        Some((cut, _)) => out[..cut].to_owned(),
        None => out,
    }
}

/// Named children excluding comments.
pub(super) fn named_children<'t>(node: tree_sitter::Node<'t>) -> Vec<tree_sitter::Node<'t>> {
    let mut cursor = node.walk();
    node.named_children(&mut cursor).filter(|c| !c.is_extra() && c.kind() != "comment").collect()
}

/// Pre-order traversal of every node under `root` (iterative).
pub(super) fn walk_tree<'t>(root: tree_sitter::Node<'t>, mut f: impl FnMut(tree_sitter::Node<'t>)) {
    let mut cursor = root.walk();
    loop {
        f(cursor.node());
        if cursor.goto_first_child() {
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if !cursor.goto_parent() {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests;
