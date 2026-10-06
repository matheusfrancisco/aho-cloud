//! `lookup_error` (layer ErrorSites → Files literal search, knowledge.md).

use std::collections::{BTreeMap, BTreeSet};
use std::cmp::Ordering;

use mage_core::domain::Layer;
use mage_core::ports::knowledge::{AppEdgeKind, AppNode, AppNodeKind, CallDirection, ErrorMatch, PageReq, Precision, SymbolHit, SymbolRef};
use mage_core::wire::indexer::{CaseMode, PatternKind, TextQuery};
use serde_json::{Value, json};

use super::super::envelope::{Degraded, Envelope, ToolError};
use super::super::errors::prepare;
use super::super::inputs::{LookupErrorInput, check_len, check_range};
use super::super::scope::KnowledgeScope;
use super::super::service::{Ctx, Service, check_scope};
use super::super::text::longest_literal_run;

/// Minimum literal run searched when ErrorSites is not Ready.
const MIN_LITERAL_RUN: usize = 12;

/// Chain positions: per segment, verified matches over disjoint spans ordered by span start
/// (outermost wrapper first). Returns position by match index.
pub(crate) fn chain_positions(matches: &[ErrorMatch]) -> BTreeMap<usize, u32> {
    let mut by_segment: BTreeMap<u16, Vec<usize>> = BTreeMap::new();
    for (i, m) in matches.iter().enumerate().filter(|(_, m)| m.verified) {
        by_segment.entry(m.segment).or_default().push(i);
    }
    let mut out = BTreeMap::new();
    for (_, mut idx) in by_segment {
        idx.sort_by_key(|i| (matches[*i].span.0, std::cmp::Reverse(matches[*i].span.1)));
        let mut chain: Vec<usize> = Vec::new();
        let mut end = 0;
        for i in idx {
            let (s, e) = matches[i].span;
            if chain.is_empty() || s >= end {
                chain.push(i);
                end = e;
            }
        }
        if chain.len() > 1 {
            for (pos, i) in chain.into_iter().enumerate() {
                out.insert(i, u32::try_from(pos).unwrap_or(u32::MAX));
            }
        }
    }
    out
}

/// `"GET /api/x/{}"` → `{"method": "GET", "path": "/api/x/{}"}`; pages have no method.
pub(crate) fn route_of(node: &AppNode, ctx: &Ctx<'_>) -> Value {
    let (method, path) = match node.key.split_once(' ') {
        Some((method, path)) if !method.starts_with('/') => (Some(method), path),
        _ => (None, node.key.as_str()),
    };
    let mut route = json!({ "kind": node.kind, "method": method, "path": path });
    if let Some((at, line)) = &node.at {
        route["citation"] = ctx.cite(at, *line, *line);
    }
    route
}

impl Service {
    pub(crate) async fn lookup_error(&self, scope: &KnowledgeScope, input: LookupErrorInput) -> Result<Envelope, ToolError> {
        check_scope(scope, input.repo.as_deref(), input.sha.as_deref())?;
        check_len("text", &input.text, 1, 4096)?;
        check_range("limit", input.limit, 1, 20)?;
        let ctx = self.ctx(scope).await?;
        let revs = match ctx.version.revs {
            Some(revs) if ctx.ready(Layer::ErrorSites) => revs,
            _ => return self.lookup_error_by_text(&ctx, &input).await,
        };
        let (org, key) = (ctx.org(), ctx.key());
        let q = prepare(&input.text, input.limit, input.include_low_specificity);
        let matches = self.knowledge.match_errors(org, &key, revs.errors, &q).await?;
        let catalog = self.knowledge.match_catalog(org, &key, revs.errors, &q).await?;
        let chain = chain_positions(&matches);

        struct Row {
            verified: bool,
            score: f32,
            chain: Option<u32>,
            item: Value,
        }
        let mut rows = Vec::new();
        for (i, m) in matches.iter().enumerate() {
            let mut item = json!({
                "citation": ctx.cite(&m.path, m.site.pos.0, m.site.pos.0),
                "kind": m.site.kind,
                "level": m.site.level,
                "template": m.site.template,
                "score": m.score,
                "verified": m.verified,
                "span": [m.span.0, m.span.1],
            });
            if let Some(pos) = chain.get(&i) {
                item["chain_position"] = json!(pos);
            }
            if let Some(status) = m.site.http_status {
                item["http_status"] = json!(status);
            }
            if let Some(enclosing) = self.enclosing_at(&ctx, m).await? {
                item["enclosing"] = json!({ "name": enclosing.def.name, "kind": enclosing.def.kind, "citation": ctx.cite_symbol(&enclosing) });
                let routes = self.routes_for(&ctx, &enclosing.at).await?;
                if !routes.is_empty() {
                    item["routes"] = Value::Array(routes);
                }
            }
            rows.push(Row { verified: m.verified, score: m.score, chain: chain.get(&i).copied(), item });
        }
        for c in &catalog {
            let catalog_cite = ctx.cite(&c.path, c.entry.line, c.entry.line);
            let i18n = json!({ "key": c.entry.key, "catalog": catalog_cite });
            let sites = self.knowledge.sites_for_i18n_key(org, &key, revs.errors, &c.entry.key, &PageReq::first(input.limit)).await?;
            if sites.items.is_empty() {
                rows.push(Row {
                    verified: c.verified,
                    score: c.score,
                    chain: None,
                    item: json!({
                        "citation": catalog_cite, "kind": "catalog_entry", "template": c.entry.template, "score": c.score,
                        "verified": c.verified, "span": [c.span.0, c.span.1], "i18n": i18n,
                    }),
                });
            }
            for site in &sites.items {
                let mut item = json!({
                    "citation": ctx.cite(&site.path, site.site.pos.0, site.site.pos.0),
                    "kind": site.site.kind,
                    "template": c.entry.template,
                    "score": c.score,
                    "verified": c.verified,
                    "span": [c.span.0, c.span.1],
                    "i18n": i18n,
                });
                if let Some(enclosing) = self.enclosing_at(&ctx, site).await? {
                    item["enclosing"] = json!({ "name": enclosing.def.name, "kind": enclosing.def.kind, "citation": ctx.cite_symbol(&enclosing) });
                }
                rows.push(Row { verified: c.verified, score: c.score, chain: None, item });
            }
        }
        rows.sort_by(|a, b| {
            b.verified
                .cmp(&a.verified)
                .then(b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal))
                .then(a.chain.unwrap_or(u32::MAX).cmp(&b.chain.unwrap_or(u32::MAX)))
        });
        rows.truncate(input.limit as usize);
        let mut env = ctx.envelope(Layer::ErrorSites);
        if !ctx.ready(Layer::Symbols) {
            env.note(format!("enclosing symbols omitted: {}", ctx.not_ready_reason(Layer::Symbols)));
        }
        if !ctx.ready(Layer::AppMap) {
            env.note(format!("routes omitted: {}", ctx.not_ready_reason(Layer::AppMap)));
        }
        env.items = rows.into_iter().map(|r| r.item).collect();
        Ok(env)
    }

    async fn enclosing_at(&self, ctx: &Ctx<'_>, m: &ErrorMatch) -> Result<Option<SymbolHit>, ToolError> {
        if !ctx.ready(Layer::Symbols) {
            return Ok(None);
        }
        let chain = self.knowledge.symbols_at(ctx.org(), &ctx.key(), &m.path, m.site.pos.0).await?;
        Ok(chain.into_iter().next())
    }

    /// Endpoints and pages handled by `symbol` or by its direct callers (AppMap Ready only).
    pub(crate) async fn routes_for(&self, ctx: &Ctx<'_>, symbol: &SymbolRef) -> Result<Vec<Value>, ToolError> {
        if !ctx.ready(Layer::AppMap) {
            return Ok(Vec::new());
        }
        let (org, key) = (ctx.org(), ctx.key());
        let mut symbols = vec![symbol.clone()];
        let callers = self.knowledge.call_edges(org, &key, std::slice::from_ref(symbol), CallDirection::Callers, &Precision::Name, 10).await?;
        symbols.extend(callers.into_iter().map(|e| e.from));
        let nodes = self.knowledge.app_nodes_for_symbols(org, &key, &symbols).await?;
        let mut routes: BTreeMap<u32, AppNode> = nodes
            .iter()
            .filter(|n| matches!(n.kind, AppNodeKind::Endpoint | AppNodeKind::Page))
            .map(|n| (n.id, n.clone()))
            .collect();
        let handlers: Vec<u32> = nodes.iter().filter(|n| n.kind == AppNodeKind::Handler).map(|n| n.id).collect();
        if !handlers.is_empty() {
            let graph = self.knowledge.app_neighborhood(org, &key, &handlers, 1, 50).await?;
            let handled: BTreeSet<u32> = graph
                .edges
                .iter()
                .filter(|e| e.kind == AppEdgeKind::HandledBy && handlers.contains(&e.to))
                .map(|e| e.from)
                .collect();
            for node in graph.nodes.into_iter().filter(|n| handled.contains(&n.id) && matches!(n.kind, AppNodeKind::Endpoint | AppNodeKind::Page)) {
                routes.insert(node.id, node);
            }
        }
        Ok(routes.values().map(|n| route_of(n, ctx)).collect())
    }

    /// ErrorSites not Ready: literal search for the longest literal run of the input.
    async fn lookup_error_by_text(&self, ctx: &Ctx<'_>, input: &LookupErrorInput) -> Result<Envelope, ToolError> {
        let mut env = ctx.envelope(Layer::Files);
        env.degraded = Some(Degraded::new(ctx.not_ready_reason(Layer::ErrorSites), Layer::ErrorSites));
        let Some(run) = longest_literal_run(&super::super::errors::strip_ansi(&input.text), MIN_LITERAL_RUN) else {
            env.note(format!("no literal run of at least {MIN_LITERAL_RUN} characters to search for"));
            return Ok(env);
        };
        let mut query = TextQuery::new(run.clone(), PatternKind::Literal);
        query.case = CaseMode::Sensitive;
        query.max_hits = input.limit;
        let hits = self.indexer.search(ctx.org(), &ctx.key(), &query, false).await?;
        env.note(format!("literal search for {run:?}"));
        env.items = hits
            .hits
            .iter()
            .map(|h| json!({ "citation": ctx.cite(&h.path, h.line, h.line), "line_text": h.text, "verified": false }))
            .collect();
        Ok(env)
    }
}

#[cfg(test)]
mod tests {
    use mage_core::domain::{BlobOid, RepoPath};
    use mage_core::ports::knowledge::{ErrorSiteKind, ErrorSiteRecord};

    use super::*;

    fn m(segment: u16, span: (u32, u32), verified: bool) -> ErrorMatch {
        ErrorMatch {
            path: RepoPath::new("a.go").unwrap_or_else(|e| panic!("{e}")),
            blob: BlobOid::from_bytes([1; 20]),
            site: ErrorSiteRecord {
                ord: 0,
                literal_ord: 0,
                kind: ErrorSiteKind::Throw,
                level: None,
                template: "t".into(),
                pattern: "t".into(),
                tokens: vec![],
                specificity: 10,
                pos: (1, 1),
                enclosing: None,
                http_status: None,
                i18n_key: None,
            },
            segment,
            span,
            verified,
            score: 1.0,
        }
    }

    #[test]
    fn disjoint_verified_spans_form_a_chain() {
        let matches = vec![m(0, (30, 50), true), m(0, (0, 25), true), m(0, (5, 10), true), m(1, (0, 9), true), m(0, (60, 70), false)];
        let chain = chain_positions(&matches);
        assert_eq!(chain.get(&1), Some(&0), "outermost (earliest) wrapper first");
        assert_eq!(chain.get(&0), Some(&1));
        assert_eq!(chain.get(&2), None, "overlapping span is not part of the chain");
        assert_eq!(chain.get(&3), None, "a lone match in its segment has no chain position");
        assert_eq!(chain.get(&4), None, "unverified matches never chain");
    }
}
