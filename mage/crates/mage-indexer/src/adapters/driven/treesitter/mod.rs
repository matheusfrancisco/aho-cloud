//! Tree-sitter `Parser` adapter (knowledge.md "`mage_indexer::adapters::driven::treesitter`").
//!
//! - Grammar registry `Language → (tree_sitter::Language, compiled queries)` built once in
//!   [`TreeSitterParser::new`].
//! - Blocking work runs on `tokio::task::spawn_blocking` workers gated by a semaphore of
//!   `parse_threads` permits shared by every call; each worker owns one `tree_sitter::Parser`
//!   and pulls blobs from a shared queue.
//! - Per blob: size cap → `TooLarge`, parse progress callback + query / walk deadline checks →
//!   `Timeout`, `catch_unwind` → `Invalid("parser panicked")`, `ERROR` / `MISSING` nodes →
//!   `StageStatus::Partial` (facts inside error nodes dropped), caps keep the lowest ords.

pub(crate) mod app_map;
mod catalog;
mod clojure;
mod detect;
mod literals;
mod registry;
mod symbols;
mod text;

#[cfg(test)]
mod tests;

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use mage_core::domain::{Language, RepoPath};
use mage_core::ports::knowledge::{
    AppFactRecord, AppMapRev, BlobInput, Catalog, FrameworkId, FrameworkScope, LiteralSite, NameRef, ParseLimits, ParsedBlob, ParserRev, StageStatus,
};
use tokio::sync::Semaphore;
use tree_sitter::{Node, ParseOptions, ParseState, Tree};

use self::registry::{CodeGrammar, Family, Registry};
use self::symbols::SymbolTable;
use self::text::{Deadline, ErrorRanges};
use crate::ports::parser::{ParseError, Parser};

/// Grammars + queries + extraction code revision; bump on any change to them.
pub const PARSER_REV: ParserRev = ParserRev(1);

/// Languages with symbol / literal extraction.
const LANGUAGES: &[Language] = &[
    Language::Go,
    Language::TypeScript,
    Language::Tsx,
    Language::JavaScript,
    Language::Python,
    Language::Rust,
    Language::Clojure,
    Language::ClojureScript,
    Language::Cljc,
];

#[derive(serde::Deserialize, Clone, Debug, Default)]
pub struct TreeSitterConfig {
    /// Concurrent blocking parse workers; 0 = available cores.
    #[serde(default)]
    pub parse_threads: usize,
}

pub struct TreeSitterParser {
    registry: Arc<Registry>,
    permits: Arc<Semaphore>,
    threads: usize,
}

impl TreeSitterParser {
    pub fn new(config: TreeSitterConfig) -> Result<Self, ParseError> {
        let threads = match config.parse_threads {
            0 => std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get),
            n => n,
        };
        Ok(Self { registry: Arc::new(Registry::new()?), permits: Arc::new(Semaphore::new(threads)), threads })
    }

    /// Runs `job` over `items` on up to `threads` blocking workers; results in input order.
    async fn run_batch<I, T, F>(&self, items: Vec<I>, job: F) -> Vec<Result<T, ParseError>>
    where
        I: Send + Sync + 'static,
        T: Send + 'static,
        F: Fn(&Registry, &mut tree_sitter::Parser, &I) -> Result<T, ParseError> + Send + Sync + 'static,
    {
        let count = items.len();
        let items = Arc::new(items);
        let job = Arc::new(job);
        let next = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..self.threads.min(count) {
            let Ok(permit) = Arc::clone(&self.permits).acquire_owned().await else { break };
            if next.load(Ordering::Relaxed) >= count {
                break;
            }
            let (items, job, next, registry) = (Arc::clone(&items), Arc::clone(&job), Arc::clone(&next), Arc::clone(&self.registry));
            handles.push(tokio::task::spawn_blocking(move || {
                let _permit = permit;
                let mut parser = tree_sitter::Parser::new();
                let mut out = Vec::new();
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(i) else { break };
                    let result = match catch_unwind(AssertUnwindSafe(|| job(&registry, &mut parser, item))) {
                        Ok(result) => result,
                        Err(_) => {
                            parser = tree_sitter::Parser::new();
                            Err(ParseError::Invalid("parser panicked".into()))
                        }
                    };
                    out.push((i, result));
                }
                out
            }));
        }
        let mut results: Vec<Option<Result<T, ParseError>>> = std::iter::repeat_with(|| None).take(count).collect();
        for handle in handles {
            match handle.await {
                Ok(out) => {
                    for (i, result) in out {
                        results[i] = Some(result);
                    }
                }
                Err(err) => tracing::warn!(error = %err, "parse worker failed"),
            }
        }
        results.into_iter().map(|r| r.unwrap_or_else(|| Err(ParseError::Unavailable("parse worker failed".into())))).collect()
    }

    /// Runs one blocking job under a permit.
    async fn run_one<T, F>(&self, job: F) -> Result<T, ParseError>
    where
        T: Send + 'static,
        F: FnOnce(&Registry) -> Result<T, ParseError> + Send + 'static,
    {
        let permit = Arc::clone(&self.permits).acquire_owned().await.map_err(|_| ParseError::Unavailable("parser shut down".into()))?;
        let registry = Arc::clone(&self.registry);
        let joined = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            catch_unwind(AssertUnwindSafe(|| job(&registry))).unwrap_or_else(|_| Err(ParseError::Invalid("parser panicked".into())))
        })
        .await;
        joined.map_err(|err| ParseError::Unavailable(format!("parse worker failed: {err}")))?
    }
}

#[async_trait]
impl Parser for TreeSitterParser {
    fn rev(&self) -> ParserRev {
        PARSER_REV
    }

    fn app_rev(&self) -> AppMapRev {
        app_map::APP_MAP_REV
    }

    fn languages(&self) -> &[Language] {
        LANGUAGES
    }

    fn detect_language(&self, path: &RepoPath, head: &[u8]) -> Option<Language> {
        detect::detect_language(path, head)
    }

    async fn parse(&self, blobs: Vec<BlobInput>, limits: ParseLimits) -> Vec<Result<ParsedBlob, ParseError>> {
        self.run_batch(blobs, move |registry, parser, blob| parse_blob(registry, parser, blob, &limits)).await
    }

    async fn catalog(&self, blob: BlobInput) -> Result<Option<Catalog>, ParseError> {
        self.run_one(move |registry| catalog::catalog(registry, &blob)).await
    }

    async fn detect_frameworks(&self, manifests: Vec<BlobInput>) -> Result<Vec<FrameworkScope>, ParseError> {
        let manifests: Vec<(RepoPath, Vec<u8>)> = manifests.into_iter().map(|b| (b.path, b.bytes)).collect();
        self.run_one(move |_| Ok(app_map::detect_frameworks(&manifests))).await
    }

    async fn app_facts(&self, blobs: Vec<(BlobInput, Vec<FrameworkId>)>, limits: ParseLimits) -> Vec<Result<Vec<AppFactRecord>, ParseError>> {
        self.run_batch(blobs, move |registry, parser, (blob, frameworks)| app_facts_blob(registry, parser, blob, frameworks, &limits)).await
    }
}

/// A parsed code blob.
struct Parsed<'r> {
    grammar: &'r CodeGrammar,
    error_nodes: u32,
    errors: ErrorRanges,
    deadline: Deadline,
}

fn cap(v: u32) -> usize {
    usize::try_from(v).unwrap_or(usize::MAX)
}

fn parse_code<'r>(registry: &'r Registry, parser: &mut tree_sitter::Parser, blob: &BlobInput, limits: &ParseLimits) -> Result<(Tree, Parsed<'r>), ParseError> {
    if u64::try_from(blob.bytes.len()).unwrap_or(u64::MAX) > limits.max_bytes {
        return Err(ParseError::TooLarge);
    }
    let grammar = registry.code(blob.lang).ok_or(ParseError::Unsupported(blob.lang))?;
    let now = Instant::now();
    let at = now
        .checked_add(limits.timeout)
        .or_else(|| now.checked_add(Duration::from_secs(365 * 24 * 3600)))
        .unwrap_or(now);
    let deadline = Deadline::new(at);
    parser.set_language(&grammar.language).map_err(|e| ParseError::Unavailable(format!("grammar: {e}")))?;
    let mut timed_out = false;
    let mut progress = |_: &ParseState| {
        let late = Instant::now() >= at;
        timed_out |= late;
        late
    };
    let bytes = blob.bytes.as_slice();
    let mut input = |offset: usize, _: tree_sitter::Point| bytes.get(offset..).unwrap_or(&[]);
    let tree = parser.parse_with_options(&mut input, None, Some(ParseOptions::new().progress_callback(&mut progress)));
    let Some(tree) = tree else {
        parser.reset();
        return Err(if timed_out { ParseError::Timeout } else { ParseError::Invalid("parse failed".into()) });
    };
    let (error_nodes, errors) = ErrorRanges::scan(tree.root_node());
    Ok((tree, Parsed { grammar, error_nodes, errors, deadline }))
}

/// Symbols (first `max_symbols`) and refs (first `max_refs`; none when 0).
fn symbols_and_refs(p: &mut Parsed<'_>, root: Node<'_>, src: &[u8], path: &RepoPath, limits: (usize, usize)) -> Result<(SymbolTable, Vec<NameRef>), ParseError> {
    match (&p.grammar.queries, p.grammar.family) {
        (Some(queries), family) => symbols::extract(family, queries, root, src, path, &p.errors, limits, &p.deadline),
        (None, Family::Clojure) => clojure::extract(root, src, &p.errors, limits, &mut p.deadline),
        (None, family) => Err(ParseError::Unavailable(format!("no symbol queries for {family:?}"))),
    }
}

fn literal_sites(p: &mut Parsed<'_>, root: Node<'_>, src: &[u8], table: &SymbolTable, max: usize) -> Result<Vec<LiteralSite>, ParseError> {
    let (errors, deadline) = (&p.errors, &mut p.deadline);
    match p.grammar.family {
        Family::Go => literals::extract(&literals::GoRules, root, src, table, errors, max, deadline),
        Family::TypeScript | Family::JavaScript => literals::extract(&literals::JsRules, root, src, table, errors, max, deadline),
        Family::Python => literals::extract(&literals::PythonRules, root, src, table, errors, max, deadline),
        Family::Rust => literals::extract(&literals::RustRules, root, src, table, errors, max, deadline),
        Family::Clojure => literals::extract(&literals::ClojureRules, root, src, table, errors, max, deadline),
    }
}

fn parse_blob(registry: &Registry, parser: &mut tree_sitter::Parser, blob: &BlobInput, limits: &ParseLimits) -> Result<ParsedBlob, ParseError> {
    let (tree, mut p) = parse_code(registry, parser, blob, limits)?;
    let root = tree.root_node();
    let src = blob.bytes.as_slice();
    let (table, refs) = symbols_and_refs(&mut p, root, src, &blob.path, (cap(limits.max_symbols), cap(limits.max_refs)))?;
    let literals = literal_sites(&mut p, root, src, &table, cap(limits.max_literals))?;
    p.deadline.check()?;
    let status = if p.error_nodes > 0 { StageStatus::Partial { error_nodes: p.error_nodes } } else { StageStatus::Ok };
    Ok(ParsedBlob { lang: blob.lang, status, symbols: table.defs, refs, literals })
}

fn app_facts_blob(
    registry: &Registry,
    parser: &mut tree_sitter::Parser,
    blob: &BlobInput,
    frameworks: &[FrameworkId],
    limits: &ParseLimits,
) -> Result<Vec<AppFactRecord>, ParseError> {
    let (tree, mut p) = parse_code(registry, parser, blob, limits)?;
    let src = blob.bytes.as_slice();
    let (table, _) = symbols_and_refs(&mut p, tree.root_node(), src, &blob.path, (cap(limits.max_symbols), 0))?;
    let facts = app_map::extract(blob.lang, &blob.path, &tree, src, &table.defs, frameworks);
    p.deadline.check()?;
    Ok(facts)
}
