//! `explain` (knowledge.md): Semantic summaries are not part of v1 (no summaries are stored), so
//! the answer is always the deterministic fact sheet from Symbols (+ AppMap, ErrorSites, git
//! history when available), labeled `degraded` with `wanted: "Semantic"`. Nothing is generated.

use std::collections::BTreeSet;

use mage_core::domain::{Layer, RepoPath};
use mage_core::ports::knowledge::{CallDirection, KnowledgeError, Precision, SymbolRef};
use mage_core::wire::indexer::LogQuery;
use serde_json::{Map, Value, json};

use super::super::envelope::{Degraded, Envelope, ToolError};
use super::super::inputs::{ExplainInput, parse_path};
use super::super::scope::KnowledgeScope;
use super::super::service::{Ctx, Service, check_scope};

const OUTLINE_MAX: usize = 50;
const ERROR_SITES_MAX: u32 = 10;
const COMMITS_MAX: u32 = 3;
const EDGE_COUNT_LIMIT: u32 = 100;

impl Service {
    pub(crate) async fn explain(&self, scope: &KnowledgeScope, input: ExplainInput) -> Result<Envelope, ToolError> {
        check_scope(scope, input.repo.as_deref(), input.sha.as_deref())?;
        if input.symbol.is_none() && input.path.is_none() {
            return Err(ToolError::invalid("give `symbol` or `path`"));
        }
        let ctx = self.ctx(scope).await?;
        ctx.require(Layer::Symbols, "use read_file")?;
        let (org, key) = (ctx.org(), ctx.key());
        let def = match (&input.symbol, input.line) {
            (Some(_), _) | (None, Some(_)) => self.resolve_symbol(&ctx, input.symbol.as_deref(), input.path.as_deref(), input.line).await?,
            (None, None) => None,
        };
        if input.symbol.is_some() && def.is_none() {
            return Err(ToolError::not_found("no symbol with that name; try find_symbol"));
        }
        let path: RepoPath = match (&def, &input.path) {
            (Some(def), _) => def.at.path.clone(),
            (None, Some(path)) => parse_path(path)?,
            (None, None) => return Err(ToolError::invalid("give `symbol` or `path`")),
        };
        let mut env = ctx.envelope(Layer::Symbols);
        let reason = if ctx.ready(Layer::Semantic) { "no summary is stored for this subject".to_owned() } else { ctx.not_ready_reason(Layer::Semantic) };
        env.degraded = Some(Degraded::new(reason, Layer::Semantic));
        env.note("fact sheet from indexed facts; no generated summary");
        let mut facts = Map::new();

        let subject = match &def {
            Some(hit) => {
                facts.insert(
                    "definition".into(),
                    json!({
                        "name": hit.def.name, "qualified": hit.def.qualified, "kind": hit.def.kind, "signature": hit.def.signature,
                        "doc": hit.def.doc, "exported": hit.def.exported, "citation": ctx.cite_symbol(hit),
                    }),
                );
                if let Some(ord) = hit.def.container {
                    let container = SymbolRef { ord, ..hit.at.clone() };
                    if let Some(c) = self.names_of(&ctx, vec![container]).await?.into_values().next() {
                        facts.insert("container".into(), json!({ "name": c.def.name, "kind": c.def.kind, "citation": ctx.cite_symbol(&c) }));
                    }
                }
                facts.insert("calls".into(), self.call_counts(&ctx, &hit.at).await?);
                if ctx.ready(Layer::AppMap) {
                    facts.insert("routes".into(), Value::Array(self.routes_for(&ctx, &hit.at).await?));
                } else {
                    env.note(format!("routes omitted: {}", ctx.not_ready_reason(Layer::AppMap)));
                }
                match ctx.version.revs {
                    Some(revs) if ctx.ready(Layer::ErrorSites) => {
                        let sites = self.knowledge.error_sites_in(org, &key, revs.errors, &hit.at, ERROR_SITES_MAX).await?;
                        facts.insert(
                            "error_sites".into(),
                            Value::Array(
                                sites
                                    .iter()
                                    .map(|s| json!({ "template": s.site.template, "kind": s.site.kind, "http_status": s.site.http_status, "citation": ctx.cite(&s.path, s.site.pos.0, s.site.pos.0) }))
                                    .collect(),
                            ),
                        );
                    }
                    _ => env.note(format!("error sites omitted: {}", ctx.not_ready_reason(Layer::ErrorSites))),
                }
                json!({ "name": hit.def.qualified, "path": hit.at.path, "citation": ctx.cite_symbol(hit) })
            }
            None => {
                let symbols = self.knowledge.symbols_in_file(org, &key, &path).await?;
                let entry = self.knowledge.manifest_entries(org, &key, std::slice::from_ref(&path)).await?.into_iter().next();
                let Some(entry) = entry else { return Err(ToolError::not_found(format!("{path} is not a file at this sha"))) };
                facts.insert("file".into(), json!({ "lang": entry.lang, "class": entry.class, "lines": entry.line_count, "size": entry.size }));
                let outline: Vec<Value> = symbols
                    .iter()
                    .filter(|s| s.def.container.is_none() || symbols.len() <= OUTLINE_MAX)
                    .take(OUTLINE_MAX)
                    .map(|s| json!({ "name": s.def.name, "kind": s.def.kind, "signature": s.def.signature, "citation": ctx.cite_symbol(s) }))
                    .collect();
                if symbols.len() > outline.len() {
                    env.note(format!("outline shows {} of {} symbols", outline.len(), symbols.len()));
                }
                facts.insert("outline".into(), Value::Array(outline));
                let end = entry.line_count.max(1);
                json!({ "path": path, "citation": ctx.cite(&path, 1, end) })
            }
        };

        let log = LogQuery { include: vec![scope.sha], exclude: Vec::new(), path_prefix: Some(path.clone()), since: None, skip: 0, max: COMMITS_MAX, with_files: false };
        match self.indexer.log(org, scope.repo, &log).await {
            Ok(commits) => {
                facts.insert(
                    "recent_commits".into(),
                    Value::Array(
                        commits
                            .iter()
                            .map(|c| json!({ "sha": c.sha, "subject": c.subject, "author_name": c.author_name, "time": super::rfc3339(c.committer_time) }))
                            .collect(),
                    ),
                );
            }
            Err(e) => env.note(format!("recent commits omitted: {e}")),
        }
        env.set("subject", subject);
        env.set("facts", Value::Object(facts));
        Ok(env)
    }

    /// Distinct callers and callees one hop away.
    async fn call_counts(&self, ctx: &Ctx<'_>, symbol: &SymbolRef) -> Result<Value, ToolError> {
        let (org, key) = (ctx.org(), ctx.key());
        let mut precision = match self.scip_index(ctx).await? {
            Some((_, p)) => p,
            None => Precision::Name,
        };
        let mut counts = [0usize; 2];
        for (i, dir) in [CallDirection::Callers, CallDirection::Callees].into_iter().enumerate() {
            let edges = match self.knowledge.call_edges(org, &key, std::slice::from_ref(symbol), dir, &precision, EDGE_COUNT_LIMIT).await {
                Err(KnowledgeError::NotFound) if precision != Precision::Name => {
                    precision = Precision::Name;
                    self.knowledge.call_edges(org, &key, std::slice::from_ref(symbol), dir, &precision, EDGE_COUNT_LIMIT).await?
                }
                other => other?,
            };
            let others: BTreeSet<&SymbolRef> = edges.iter().map(|e| if dir == CallDirection::Callers { &e.from } else { &e.to }).collect();
            counts[i] = others.len();
        }
        let precision = match precision {
            Precision::Precise => "precise",
            Precision::PreciseStale { .. } => "precise_stale",
            Precision::Name => "name",
        };
        Ok(json!({ "callers": counts[0], "callees": counts[1], "precision": precision }))
    }
}
