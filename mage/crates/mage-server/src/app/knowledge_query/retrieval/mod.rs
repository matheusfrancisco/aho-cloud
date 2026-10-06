//! `Retriever::triage_candidates` (knowledge.md "Retrieval pipeline for triage"): seeds a triage
//! run with candidate code locations at the ticket's sha. Each stage runs only when its layer is
//! Ready (and not disabled by the token), under `stage_timeout` within `total_timeout`; a failed
//! or slow stage is reported and never fails the call.
//!
//! Candidate unit = (path, innermost symbol) or (path, ±10-line window). Score = Σ signal
//! contributions; weights come from `[server.knowledge.retrieval]`.

pub mod extract;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mage_core::domain::knowledge::{RouteKey, RoutePattern, route_match};
use mage_core::domain::{Citation, Layer, LineRange, RepoPath, RepoVersion, RepoVersionKey};
use mage_core::ports::knowledge::{
    AppEdgeKind, AppNode, AppNodeKind, CallDirection, KnowledgeError, KnowledgeStore, NameMatch, PageReq, Precision, SymbolHit, SymbolQuery,
    SymbolRef,
};
use mage_core::ports::repos::{RepoStore, RepoStoreError};
use mage_core::wire::indexer::TreeDiffOptions;
use serde::Deserialize;

use super::errors::prepare;
use super::scope::KnowledgeScope;
use crate::domain::knowledge::{AnswerSource, Candidate, CandidateSet, Signal, SimilarResolved, Stage, StageReport, SymbolSummary, TicketContext};
use crate::error::AppError;
use crate::ports::indexer_link::IndexerLink;
use crate::ports::ticket::{SimilarQuery, TicketStore};

/// Stage weights (`[server.knowledge.retrieval]`), tuned with `mage knowledge eval`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RetrievalWeights {
    pub stack_frame: f32,
    pub error_match: f32,
    pub i18n_match: f32,
    pub route: f32,
    pub name: f32,
    pub glossary: f32,
    pub semantic: f32,
    /// Graph neighbors get parent score × this.
    pub graph: f32,
    /// Candidates whose file changed since `last_good` are multiplied by this.
    pub history_boost: f32,
    /// Changed files next to top seeds are added at this score.
    pub history_added: f32,
    pub similar_ticket: f32,
}

impl Default for RetrievalWeights {
    fn default() -> Self {
        RetrievalWeights {
            stack_frame: 1.0,
            error_match: 1.0,
            i18n_match: 0.9,
            route: 0.7,
            name: 0.5,
            glossary: 0.4,
            semantic: 0.4,
            graph: 0.5,
            history_boost: 1.5,
            history_added: 0.2,
            similar_ticket: 0.5,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetrievalLimits {
    pub max_candidates: u32,
    pub stage_timeout: Duration,
    pub total_timeout: Duration,
}

impl Default for RetrievalLimits {
    fn default() -> Self {
        RetrievalLimits { max_candidates: 30, stage_timeout: Duration::from_millis(1500), total_timeout: Duration::from_secs(5) }
    }
}

pub struct RetrieverDeps {
    pub knowledge: Arc<dyn KnowledgeStore>,
    pub repos: Arc<dyn RepoStore>,
    pub tickets: Arc<dyn TicketStore>,
    pub indexer: Arc<dyn IndexerLink>,
    pub weights: RetrievalWeights,
}

pub struct Retriever {
    knowledge: Arc<dyn KnowledgeStore>,
    repos: Arc<dyn RepoStore>,
    tickets: Arc<dyn TicketStore>,
    indexer: Arc<dyn IndexerLink>,
    weights: RetrievalWeights,
}

/// Seeds kept from stage 4 onward, and per seed.
const GRAPH_SEEDS: usize = 10;
const GRAPH_PER_SEED: usize = 5;
const WINDOW: u32 = 10;
const SIMILAR_MAX: u16 = 5;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Unit {
    Symbol(SymbolRef),
    Window(RepoPath, u32, u32),
}

#[derive(Debug, Clone)]
struct Draft {
    path: RepoPath,
    lines: LineRange,
    symbol: Option<SymbolHit>,
    score: f32,
    signals: Vec<Signal>,
}

/// The working candidate set: merges signals on the same symbol or overlapping windows.
#[derive(Default)]
struct Board {
    drafts: BTreeMap<Unit, Draft>,
}

impl Board {
    fn add(&mut self, path: RepoPath, (start, end): (u32, u32), symbol: Option<SymbolHit>, signal: Signal) {
        let Ok(lines) = LineRange::new(start, end) else { return };
        let unit = match &symbol {
            Some(hit) => Unit::Symbol(hit.at.clone()),
            None => {
                let overlapping = self.drafts.keys().find_map(|unit| match unit {
                    Unit::Window(p, s, e) if *p == path && LineRange::new(*s, *e).is_ok_and(|r| r.overlaps(&lines)) => Some(unit.clone()),
                    _ => None,
                });
                overlapping.unwrap_or(Unit::Window(path.clone(), lines.start(), lines.end()))
            }
        };
        let draft = self.drafts.entry(unit).or_insert_with(|| Draft { path, lines, symbol, score: 0.0, signals: Vec::new() });
        draft.score += signal.contribution;
        draft.signals.push(signal);
    }

    fn ranked(&self) -> Vec<&Draft> {
        let mut out: Vec<&Draft> = self.drafts.values().collect();
        out.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal).then(a.path.cmp(&b.path)).then(a.lines.cmp(&b.lines)));
        out
    }
}

/// A stage's outcome before it becomes a report.
enum Outcome {
    Ran(u32),
    Skipped(String),
}

struct Run<'a> {
    scope: &'a KnowledgeScope,
    version: RepoVersion,
    key: RepoVersionKey,
}

impl Run<'_> {
    fn ready(&self, layer: Layer) -> bool {
        !self.scope.layer_disabled(layer) && self.version.layer(layer).is_some_and(|s| s.is_ready())
    }

    fn not_ready(&self, layer: Layer) -> String {
        if self.scope.layer_disabled(layer) {
            return "layer_disabled".into();
        }
        let state = self.version.layer(layer).map_or_else(|| "pending".to_owned(), |s| s.name().to_owned());
        format!("layer {} is {state}", crate::domain::knowledge::layer_name(layer))
    }

    fn citation(&self, path: &RepoPath, lines: LineRange) -> Citation {
        Citation { repo_id: self.key.repo_id, sha: self.key.sha, path: path.clone(), lines, snippet_hash: None, note: None }
    }
}

/// ±10 lines around `line` as `(start, end)`.
fn window(line: u32) -> (u32, u32) {
    let line = line.max(1);
    (line.saturating_sub(WINDOW).max(1), line.saturating_add(WINDOW))
}

fn span(hit: &SymbolHit) -> (u32, u32) {
    (hit.def.span.start(), hit.def.span.end())
}

fn signal(stage: Stage, detail: impl Into<String>, contribution: f32) -> Signal {
    Signal { stage, detail: detail.into(), contribution }
}

fn stage_layer(stage: Stage) -> AnswerSource {
    match stage {
        Stage::StackFrame => Layer::Files.into(),
        Stage::ErrorMatch | Stage::I18nMatch => Layer::ErrorSites.into(),
        Stage::Route => Layer::AppMap.into(),
        Stage::Name | Stage::Glossary | Stage::Graph => Layer::Symbols.into(),
        Stage::Semantic => Layer::Semantic.into(),
        Stage::History => Layer::History.into(),
        Stage::SimilarTicket => AnswerSource::Memory,
    }
}

/// Longest common suffix of path segments.
fn common_suffix(a: &[&str], b: &[&str]) -> usize {
    a.iter().rev().zip(b.iter().rev()).take_while(|(x, y)| x == y).count()
}

impl Retriever {
    pub fn new(deps: RetrieverDeps) -> Self {
        Retriever { knowledge: deps.knowledge, repos: deps.repos, tickets: deps.tickets, indexer: deps.indexer, weights: deps.weights }
    }

    pub async fn triage_candidates(&self, scope: &KnowledgeScope, ctx: &TicketContext, limits: &RetrievalLimits) -> Result<CandidateSet, AppError> {
        let key = scope.version();
        let version = self.repos.version(scope.org, key).await.map_err(|e| match e {
            RepoStoreError::NotFound => AppError::NotFound(format!("{key} is not indexed")),
            RepoStoreError::Invalid(m) => AppError::Invalid(m),
            RepoStoreError::Conflict => AppError::Conflict("repo store conflict".into()),
            RepoStoreError::Unavailable(m) => AppError::Unavailable(m),
        })?;
        let run = Run { scope, version, key };
        let deadline = Instant::now() + limits.total_timeout;
        let mut board = Board::default();
        let mut stages = Vec::new();
        let mut similar = Vec::new();
        let text = format!("{}\n{}\n{}", ctx.title, ctx.body, ctx.error_strings.join("\n"));
        let identifiers = extract::identifiers(&text);

        for stage in [Stage::StackFrame, Stage::ErrorMatch, Stage::I18nMatch, Stage::Route, Stage::Name, Stage::Glossary, Stage::Semantic] {
            let started = Instant::now();
            let layer = stage_layer(stage);
            let outcome = match layer {
                AnswerSource::Layer(l) if !run.ready(l) => Outcome::Skipped(run.not_ready(l)),
                _ => {
                    let budget = limits.stage_timeout.min(deadline.saturating_duration_since(Instant::now()));
                    if budget.is_zero() {
                        Outcome::Skipped("total timeout".into())
                    } else {
                        let work = self.seed_stage(stage, &run, ctx, &text, &identifiers, &mut board);
                        timed(budget, work).await
                    }
                }
            };
            stages.push(report(stage, layer, outcome, started.elapsed()));
        }

        // Stage 4: graph expansion around the top seeds.
        let started = Instant::now();
        let outcome = if !run.ready(Layer::Symbols) {
            Outcome::Skipped(run.not_ready(Layer::Symbols))
        } else {
            let budget = limits.stage_timeout.min(deadline.saturating_duration_since(Instant::now()));
            if budget.is_zero() { Outcome::Skipped("total timeout".into()) } else { timed(budget, self.graph_stage(&run, &mut board)).await }
        };
        stages.push(report(Stage::Graph, stage_layer(Stage::Graph), outcome, started.elapsed()));

        // Stage 5: history boost.
        let started = Instant::now();
        let outcome = match ctx.last_good {
            None => Outcome::Skipped("no last good version".into()),
            Some(_) if run.scope.layer_disabled(Layer::History) => Outcome::Skipped("layer_disabled".into()),
            Some(_) if !run.ready(Layer::Files) => Outcome::Skipped(run.not_ready(Layer::Files)),
            Some(last_good) => {
                let budget = limits.stage_timeout.min(deadline.saturating_duration_since(Instant::now()));
                if budget.is_zero() { Outcome::Skipped("total timeout".into()) } else { timed(budget, self.history_stage(&run, last_good, &mut board)).await }
            }
        };
        stages.push(report(Stage::History, stage_layer(Stage::History), outcome, started.elapsed()));

        // Stage 6: similar resolved tickets (memory cutoff applied).
        let started = Instant::now();
        let budget = limits.stage_timeout.min(deadline.saturating_duration_since(Instant::now()));
        let outcome = if budget.is_zero() {
            Outcome::Skipped("total timeout".into())
        } else {
            timed(budget, self.similar_stage(&run, ctx, &mut board, &mut similar)).await
        };
        stages.push(report(Stage::SimilarTicket, AnswerSource::Memory, outcome, started.elapsed()));

        let candidates = board
            .ranked()
            .into_iter()
            .take(limits.max_candidates as usize)
            .map(|d| Candidate {
                citation: run.citation(&d.path, d.lines),
                symbol: d.symbol.as_ref().map(|s| SymbolSummary { name: s.def.name.clone(), kind: s.def.kind, signature: s.def.signature.clone() }),
                score: d.score,
                signals: d.signals.clone(),
            })
            .collect();
        similar.truncate(usize::from(SIMILAR_MAX));
        Ok(CandidateSet { version: key, candidates, similar, stages })
    }

    async fn seed_stage(
        &self,
        stage: Stage,
        run: &Run<'_>,
        ctx: &TicketContext,
        text: &str,
        identifiers: &[String],
        board: &mut Board,
    ) -> Result<u32, KnowledgeError> {
        let w = &self.weights;
        let (org, key) = (run.scope.org, run.key);
        let mut hits = 0;
        match stage {
            Stage::StackFrame => {
                for frame in extract::frames(text).into_iter().take(10) {
                    let segments = frame.segments();
                    let paths = self.knowledge.paths_by_basename(org, &key, frame.basename(), 20).await?;
                    let scored: Vec<(usize, RepoPath)> = paths
                        .into_iter()
                        .map(|p| (common_suffix(&p.segments().collect::<Vec<_>>(), &segments), p))
                        .filter(|(n, _)| *n > 0)
                        .collect();
                    let Some(best) = scored.iter().map(|(n, _)| *n).max() else { continue };
                    for (n, path) in scored.into_iter().filter(|(n, _)| *n == best).take(3) {
                        let fraction = n as f32 / segments.len().max(1) as f32;
                        let detail = match frame.line {
                            Some(line) => format!("{}:{line}", frame.path),
                            None => frame.path.clone(),
                        };
                        let (lines, symbol) = self.locate(run, &path, frame.line).await?;
                        board.add(path, lines, symbol, signal(Stage::StackFrame, detail, w.stack_frame * fraction));
                        hits += 1;
                    }
                }
            }
            Stage::ErrorMatch => {
                let Some(revs) = run.version.revs else { return Ok(0) };
                for line in extract::error_lines(&ctx.body, &ctx.error_strings).into_iter().take(10) {
                    let q = prepare(&line, 10, false);
                    for m in self.knowledge.match_errors(org, &key, revs.errors, &q).await? {
                        let (lines, symbol) = self.locate(run, &m.path, Some(m.site.pos.0)).await?;
                        let detail = format!("{} ← {:?}", m.site.template, line);
                        board.add(m.path, lines, symbol, signal(Stage::ErrorMatch, detail, w.error_match * m.score));
                        hits += 1;
                    }
                }
            }
            Stage::I18nMatch => {
                let Some(revs) = run.version.revs else { return Ok(0) };
                let mut texts = extract::ui_texts(text);
                texts.extend(ctx.error_strings.iter().filter(|s| s.split_whitespace().count() <= 6).cloned());
                for ui in texts.into_iter().take(10) {
                    let q = prepare(&ui, 5, true);
                    for c in self.knowledge.match_catalog(org, &key, revs.errors, &q).await? {
                        let sites = self.knowledge.sites_for_i18n_key(org, &key, revs.errors, &c.entry.key, &PageReq::first(10)).await?;
                        for site in sites.items {
                            let (lines, symbol) = self.locate(run, &site.path, Some(site.site.pos.0)).await?;
                            let detail = format!("{:?} → {}", ui, c.entry.key);
                            board.add(site.path, lines, symbol, signal(Stage::I18nMatch, detail, w.i18n_match * c.score));
                            hits += 1;
                        }
                    }
                }
            }
            Stage::Route => {
                let urls = extract::url_paths(text);
                if urls.is_empty() {
                    return Ok(0);
                }
                let mut nodes: Vec<AppNode> = Vec::new();
                let mut page = PageReq::first(1000);
                loop {
                    let got = self.knowledge.app_nodes(org, &key, &[AppNodeKind::Endpoint, AppNodeKind::Page], &page).await?;
                    nodes.extend(got.items);
                    match got.next {
                        Some(next) if nodes.len() < 5000 => page.after = Some(next),
                        _ => break,
                    }
                }
                for url in urls.into_iter().take(10) {
                    let request = RouteKey::new(None, RoutePattern::from_url(&url));
                    let mut matched: Vec<(&AppNode, f32)> = nodes
                        .iter()
                        .filter_map(|n| {
                            let route = n.key.parse::<RouteKey>().ok().or_else(|| n.key.parse::<RoutePattern>().ok().map(|p| RouteKey::new(None, p)))?;
                            route_match(&request, &route).map(|m| (n, m.path.confidence()))
                        })
                        .collect();
                    matched.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.id.cmp(&b.0.id)));
                    for (node, confidence) in matched.into_iter().take(3) {
                        for symbol in self.route_symbols(run, node).await? {
                            let lines = span(&symbol);
                            let detail = format!("{url} → {}", node.key);
                            board.add(symbol.at.path.clone(), lines, Some(symbol), signal(Stage::Route, detail, w.route * confidence));
                            hits += 1;
                        }
                    }
                }
            }
            Stage::Name => {
                for ident in identifiers.iter().take(20) {
                    let (qualifier, name) = super::text::split_qualified(ident);
                    for mode in [NameMatch::Exact, NameMatch::CaseInsensitive] {
                        let q = SymbolQuery { name: name.clone(), qualifier: qualifier.clone(), mode, kinds: Vec::new(), lang: None, path_prefix: None, include_tests: false };
                        let found = self.knowledge.find_symbols(org, &key, &q, &PageReq::first(3)).await?;
                        if found.items.is_empty() {
                            continue;
                        }
                        for hit in found.items {
                            board.add(hit.at.path.clone(), span(&hit), Some(hit), signal(Stage::Name, ident.clone(), w.name));
                            hits += 1;
                        }
                        break;
                    }
                }
            }
            Stage::Glossary => {
                let nouns = extract::nouns(&ctx.title, identifiers);
                if nouns.is_empty() {
                    return Ok(0);
                }
                let found = self.knowledge.glossary_hits(org, &key, &nouns, 30).await?;
                let max = found.iter().map(|h| h.weight).fold(0.0_f32, f32::max);
                for hit in found {
                    let (path, line) = hit.at.clone();
                    let (lines, symbol) = self.locate(run, &path, Some(line)).await?;
                    let share = if max > 0.0 { hit.weight / max } else { 1.0 };
                    board.add(path, lines, symbol, signal(Stage::Glossary, hit.text, w.glossary * share));
                    hits += 1;
                }
            }
            Stage::Semantic => {
                // Layer 5 is not built in v1: `Semantic` is never Ready, so this arm is not reached.
            }
            Stage::Graph | Stage::History | Stage::SimilarTicket => {}
        }
        Ok(hits)
    }

    /// The candidate unit at (path, line): innermost symbol when Symbols is Ready, else a window.
    async fn locate(&self, run: &Run<'_>, path: &RepoPath, line: Option<u32>) -> Result<((u32, u32), Option<SymbolHit>), KnowledgeError> {
        let line = line.unwrap_or(1).max(1);
        if run.ready(Layer::Symbols) {
            if let Some(hit) = self.knowledge.symbols_at(run.scope.org, &run.key, path, line).await?.into_iter().next() {
                return Ok((span(&hit), Some(hit)));
            }
        }
        Ok((window(line), None))
    }

    /// Handler symbol of an endpoint, or the page's own component symbol.
    async fn route_symbols(&self, run: &Run<'_>, node: &AppNode) -> Result<Vec<SymbolHit>, KnowledgeError> {
        let (org, key) = (run.scope.org, run.key);
        let mut refs: Vec<SymbolRef> = node.symbol.iter().cloned().collect();
        let around = self.knowledge.app_neighborhood(org, &key, &[node.id], 1, 50).await?;
        let handlers: BTreeSet<u32> = around.edges.iter().filter(|e| e.from == node.id && e.kind == AppEdgeKind::HandledBy).map(|e| e.to).collect();
        refs.extend(around.nodes.iter().filter(|n| handlers.contains(&n.id)).filter_map(|n| n.symbol.clone()));
        if refs.is_empty() || !run.ready(Layer::Symbols) {
            return Ok(Vec::new());
        }
        self.knowledge.symbols_by_ref(org, &key, &refs).await
    }

    async fn graph_stage(&self, run: &Run<'_>, board: &mut Board) -> Result<u32, KnowledgeError> {
        let (org, key) = (run.scope.org, run.key);
        let seeds: Vec<(SymbolRef, f32)> = board
            .ranked()
            .into_iter()
            .filter_map(|d| d.symbol.as_ref().map(|s| (s.at.clone(), d.score)))
            .take(GRAPH_SEEDS)
            .collect();
        let precision = if run.ready(Layer::Refs) {
            match self.knowledge.scip_index_for(org, &key).await? {
                Some(index) if index.sha == key.sha => Precision::Precise,
                Some(index) => Precision::PreciseStale { from_sha: index.sha },
                None => Precision::Name,
            }
        } else {
            Precision::Name
        };
        let mut hits = 0;
        for (seed, score) in seeds {
            let mut neighbors: Vec<SymbolRef> = Vec::new();
            for dir in [CallDirection::Callers, CallDirection::Callees] {
                let edges = match self.knowledge.call_edges(org, &key, std::slice::from_ref(&seed), dir, &precision, 10).await {
                    Err(KnowledgeError::NotFound) => self.knowledge.call_edges(org, &key, std::slice::from_ref(&seed), dir, &Precision::Name, 10).await?,
                    other => other?,
                };
                neighbors.extend(edges.into_iter().map(|e| if dir == CallDirection::Callers { e.from } else { e.to }));
            }
            if run.ready(Layer::AppMap) {
                let nodes = self.knowledge.app_nodes_for_symbols(org, &key, std::slice::from_ref(&seed)).await?;
                let ids: Vec<u32> = nodes.iter().map(|n| n.id).collect();
                if !ids.is_empty() {
                    let around = self.knowledge.app_neighborhood(org, &key, &ids, 1, 20).await?;
                    neighbors.extend(
                        around
                            .nodes
                            .into_iter()
                            .filter(|n| matches!(n.kind, AppNodeKind::Endpoint | AppNodeKind::Handler | AppNodeKind::ClientCall))
                            .filter_map(|n| n.symbol),
                    );
                }
            }
            let unique: Vec<SymbolRef> = neighbors.into_iter().filter(|n| *n != seed).collect::<BTreeSet<_>>().into_iter().take(GRAPH_PER_SEED).collect();
            if unique.is_empty() {
                continue;
            }
            for hit in self.knowledge.symbols_by_ref(org, &key, &unique).await? {
                let detail = format!("neighbor of {}:{}", seed.path, seed.ord);
                board.add(hit.at.path.clone(), span(&hit), Some(hit), signal(Stage::Graph, detail, score * self.weights.graph));
                hits += 1;
            }
        }
        Ok(hits)
    }

    async fn history_stage(&self, run: &Run<'_>, last_good: mage_core::domain::CommitSha, board: &mut Board) -> Result<u32, KnowledgeError> {
        let (org, key) = (run.scope.org, run.key);
        let from_key = RepoVersionKey::new(key.repo_id, last_good);
        let from_indexed = matches!(self.repos.version(org, from_key).await, Ok(v) if v.layer(Layer::Files).is_some_and(|s| s.is_ready()));
        let mut changed: BTreeSet<RepoPath> = BTreeSet::new();
        if from_indexed {
            let mut page = PageReq::first(1000);
            loop {
                let got = self.knowledge.manifest_diff(org, &from_key, &key, None, &page).await?;
                changed.extend(got.items.into_iter().map(|c| c.path));
                match got.next {
                    Some(next) if changed.len() < 5000 => page.after = Some(next),
                    _ => break,
                }
            }
        } else {
            let opts = TreeDiffOptions { paths: Vec::new(), hunks: false, context: 0, max_bytes_per_file: 0, renames: true };
            let diffs = self.indexer.diff(org, key.repo_id, last_good, key.sha, &opts).await.map_err(|e| KnowledgeError::Unavailable(e.to_string()))?;
            changed.extend(diffs.into_iter().map(|d| d.change.path));
        }
        let boost = self.weights.history_boost;
        let mut hits = 0;
        let top_files: BTreeSet<RepoPath> = board.ranked().into_iter().take(GRAPH_SEEDS).map(|d| d.path.clone()).collect();
        for draft in board.drafts.values_mut() {
            if changed.contains(&draft.path) {
                let extra = draft.score * (boost - 1.0);
                draft.score += extra;
                draft.signals.push(signal(Stage::History, format!("changed since {last_good}"), extra));
                hits += 1;
            }
        }
        // Changed files that call into or are called by the top seed files.
        let neighbor_files: BTreeSet<RepoPath> = board
            .drafts
            .values()
            .filter(|d| d.signals.iter().any(|s| s.stage == Stage::Graph))
            .map(|d| d.path.clone())
            .chain(top_files.iter().cloned())
            .collect();
        let present: BTreeSet<RepoPath> = board.drafts.values().map(|d| d.path.clone()).collect();
        for path in changed.iter().filter(|p| neighbor_files.contains(*p) && !present.contains(*p)) {
            board.add(path.clone(), window(1), None, signal(Stage::History, format!("changed since {last_good}"), self.weights.history_added));
            hits += 1;
        }
        Ok(hits)
    }

    async fn similar_stage(
        &self,
        run: &Run<'_>,
        ctx: &TicketContext,
        board: &mut Board,
        similar: &mut Vec<SimilarResolved>,
    ) -> Result<u32, KnowledgeError> {
        let scope = run.scope;
        let q = SimilarQuery {
            repos: vec![scope.repo],
            text: format!("{}\n\n{}", ctx.title, ctx.body),
            error_strings: ctx.error_strings.clone(),
            exclude: scope.memory.exclude_tickets.clone(),
            as_of: scope.memory.as_of,
        };
        let found = self.tickets.similar_resolved(scope.org, &q, SIMILAR_MAX).await.map_err(|e| KnowledgeError::Unavailable(AppError::from(e).to_string()))?;
        let found: Vec<SimilarResolved> = found
            .into_iter()
            .filter(|s| !scope.memory.exclude_tickets.contains(&s.ticket) && scope.memory.as_of.is_none_or(|as_of| s.resolved_at < as_of))
            .collect();
        let mut hits = 0;
        if run.ready(Layer::Files) {
            let paths: Vec<RepoPath> =
                found.iter().flat_map(|s| s.cause_citations.iter()).filter(|c| c.repo_id == scope.repo).map(|c| c.path.clone()).collect::<BTreeSet<_>>().into_iter().collect();
            let present: BTreeMap<RepoPath, u32> = if paths.is_empty() {
                BTreeMap::new()
            } else {
                self.knowledge.manifest_entries(scope.org, &run.key, &paths).await?.into_iter().map(|e| (e.path, e.line_count)).collect()
            };
            for s in &found {
                for c in s.cause_citations.iter().filter(|c| c.repo_id == scope.repo) {
                    let Some(&line_count) = present.get(&c.path) else { continue };
                    if line_count == 0 || c.lines.start() > line_count {
                        continue;
                    }
                    let cited = (c.lines.start(), c.lines.end().min(line_count));
                    let (lines, symbol) = match self.locate(run, &c.path, Some(cited.0)).await? {
                        (span, Some(symbol)) => (span, Some(symbol)),
                        (_, None) => (cited, None),
                    };
                    board.add(c.path.clone(), lines, symbol, signal(Stage::SimilarTicket, s.identifier.clone(), self.weights.similar_ticket * s.score));
                    hits += 1;
                }
            }
        }
        *similar = found;
        Ok(hits)
    }
}

async fn timed<F: Future<Output = Result<u32, KnowledgeError>>>(budget: Duration, work: F) -> Outcome {
    match tokio::time::timeout(budget, work).await {
        Ok(Ok(hits)) => Outcome::Ran(hits),
        Ok(Err(e)) => Outcome::Skipped(format!("failed: {e}")),
        Err(_) => Outcome::Skipped("stage timeout".into()),
    }
}

fn report(stage: Stage, layer: AnswerSource, outcome: Outcome, elapsed: Duration) -> StageReport {
    match outcome {
        Outcome::Ran(hits) => StageReport { stage, layer, ran: true, reason: None, hits, elapsed },
        Outcome::Skipped(reason) => StageReport { stage, layer, ran: false, reason: Some(reason), hits: 0, elapsed },
    }
}
