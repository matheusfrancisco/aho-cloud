//! `app_map`, `route_info` (layer AppMap, knowledge.md).

use std::collections::{BTreeMap, BTreeSet};

use mage_core::domain::Layer;
use mage_core::domain::knowledge::{RouteKey, RoutePattern, route_match};
use mage_core::ports::knowledge::{AppEdge, AppEdgeKind, AppGraph, AppNode, AppNodeKind, ErrorsRev, NameMatch, PageReq, SymbolQuery};
use mage_core::wire::indexer::{CaseMode, PatternKind, TextQuery};
use serde_json::{Value, json};

use super::super::envelope::{CursorScope, Degraded, Envelope, StoreKey, ToolError};
use super::super::inputs::{AppMapInput, FocusKind, RouteInfoInput, check_range, parse_path};
use super::super::scope::KnowledgeScope;
use super::super::service::{Ctx, Service, check_scope};
use super::super::text::{longest_static_run, split_qualified};
use super::symbols::set_graph;
use super::{resume_key, store_page};

/// Nodes read when the whole graph is scanned (route matching, coverage).
const MAX_SCAN_NODES: usize = 5000;
const ROUTES_PER_ANSWER: usize = 5;

fn node_json(node: &AppNode, ctx: &Ctx<'_>) -> Value {
    let mut out = json!({ "id": node.id, "kind": node.kind, "key": node.key, "label": node.label, "framework": node.framework });
    if let Some((path, line)) = &node.at {
        out["citation"] = ctx.cite(path, *line, *line);
    }
    out
}

fn edge_json(edge: &AppEdge, ctx: &Ctx<'_>) -> Value {
    let mut out = json!({ "from": edge.from, "to": edge.to, "kind": edge.kind, "confidence": edge.confidence });
    if let Some((path, line)) = &edge.evidence {
        out["evidence"] = ctx.cite(path, *line, *line);
    }
    out
}

/// Route key of an Endpoint (`GET /x/{}`) or Page (`/x/{}`) node.
fn node_route(node: &AppNode) -> Option<RouteKey> {
    node.key.parse::<RouteKey>().ok().or_else(|| node.key.parse::<RoutePattern>().ok().map(|p| RouteKey::new(None, p)))
}

impl Service {
    /// Every node of `kinds` (empty = all), up to [`MAX_SCAN_NODES`].
    async fn scan_nodes(&self, ctx: &Ctx<'_>, kinds: &[AppNodeKind]) -> Result<(Vec<AppNode>, bool), ToolError> {
        let mut out = Vec::new();
        let mut page = PageReq::first(1000);
        loop {
            let got = self.knowledge.app_nodes(ctx.org(), &ctx.key(), kinds, &page).await?;
            out.extend(got.items);
            match got.next {
                Some(next) if out.len() < MAX_SCAN_NODES => page.after = Some(next),
                Some(_) => return Ok((out, true)),
                None => return Ok((out, false)),
            }
        }
    }

    pub(crate) async fn app_map(&self, scope: &KnowledgeScope, mut input: AppMapInput) -> Result<Envelope, ToolError> {
        check_scope(scope, input.repo.as_deref(), input.sha.as_deref())?;
        check_range("depth", input.depth, 1, 2)?;
        check_range("limit", input.limit, 1, 200)?;
        let cursor = input.cursor.take();
        let cursors = CursorScope::new("app_map", &input);
        let keyset = cursors.decode_opt(cursor.as_deref())?;
        let ctx = self.ctx(scope).await?;
        ctx.require(Layer::AppMap, "use search_code / route_info")?;
        let (org, key) = (ctx.org(), ctx.key());
        let kinds: Vec<AppNodeKind> = input.kinds.iter().map(|k| AppNodeKind::from(*k)).collect();
        let mut env = ctx.envelope(Layer::AppMap);
        match &input.focus {
            None => {
                let kinds = if kinds.is_empty() { vec![AppNodeKind::Page, AppNodeKind::Endpoint, AppNodeKind::Table] } else { kinds };
                let page_key = StoreKey::parse(keyset.as_deref())?;
                let (nodes, next) =
                    store_page(&page_key, input.limit, |page| async move { self.knowledge.app_nodes(org, &key, &kinds, &page).await }).await?;
                let fetched = nodes.len();
                set_graph(&mut env, nodes.iter().map(|n| node_json(n, &ctx)).collect(), Vec::new());
                let kept = env.extra["nodes"].as_array().map_or(0, Vec::len);
                env.next_cursor = resume_key(&page_key, kept, fetched, next).map(|k| cursors.encode(&k.to_keyset()));
            }
            Some(focus) => {
                if keyset.is_some() {
                    return Err(ToolError::bad_cursor());
                }
                let ids = self.focus_ids(&ctx, focus.kind, &focus.value).await?;
                if ids.is_empty() {
                    return Err(ToolError::not_found(format!("nothing in the app map matches {:?}", focus.value)));
                }
                let graph = self.knowledge.app_neighborhood(org, &key, &ids, input.depth, input.limit).await?;
                let kept: BTreeSet<u32> = graph.nodes.iter().filter(|n| kinds.is_empty() || kinds.contains(&n.kind) || ids.contains(&n.id)).map(|n| n.id).collect();
                let nodes = graph.nodes.iter().filter(|n| kept.contains(&n.id)).map(|n| node_json(n, &ctx)).collect();
                let edges = graph.edges.iter().filter(|e| kept.contains(&e.from) && kept.contains(&e.to)).map(|e| edge_json(e, &ctx)).collect();
                set_graph(&mut env, nodes, edges);
            }
        }
        match self.coverage(&ctx).await? {
            Some(coverage) => env.set("coverage", coverage),
            None => env.note("coverage omitted: graph larger than the scan limit"),
        }
        Ok(env)
    }

    async fn focus_ids(&self, ctx: &Ctx<'_>, kind: FocusKind, value: &str) -> Result<Vec<u32>, ToolError> {
        let (org, key) = (ctx.org(), ctx.key());
        let value = value.trim();
        Ok(match kind {
            FocusKind::Route | FocusKind::Page => {
                let kinds: &[AppNodeKind] =
                    if kind == FocusKind::Page { &[AppNodeKind::Page] } else { &[AppNodeKind::Endpoint, AppNodeKind::Page] };
                let (nodes, _) = self.scan_nodes(ctx, kinds).await?;
                let request = request_key(value, None);
                best_routes(&request, &nodes).into_iter().map(|(n, _)| n.id).collect()
            }
            FocusKind::Component | FocusKind::Table => {
                let kinds: &[AppNodeKind] = if kind == FocusKind::Table { &[AppNodeKind::Table] } else { &[AppNodeKind::Component] };
                let (nodes, _) = self.scan_nodes(ctx, kinds).await?;
                nodes.iter().filter(|n| n.key.eq_ignore_ascii_case(value) || n.label.eq_ignore_ascii_case(value)).map(|n| n.id).collect()
            }
            FocusKind::Symbol => {
                ctx.require(Layer::Symbols, "focus on a route, table or path instead")?;
                let (qualifier, name) = split_qualified(value);
                let q = SymbolQuery { name, qualifier, mode: NameMatch::Exact, kinds: Vec::new(), lang: None, path_prefix: None, include_tests: false };
                let hits = self.knowledge.find_symbols(org, &key, &q, &PageReq::first(10)).await?;
                let refs: Vec<_> = hits.items.into_iter().map(|h| h.at).collect();
                self.knowledge.app_nodes_for_symbols(org, &key, &refs).await?.into_iter().map(|n| n.id).collect()
            }
            FocusKind::Path => {
                let path = parse_path(value)?;
                let (nodes, _) = self.scan_nodes(ctx, &[]).await?;
                nodes.iter().filter(|n| n.at.as_ref().is_some_and(|(p, _)| *p == path)).map(|n| n.id).collect()
            }
        })
    }

    /// `endpoints_with_handler` and `client_calls_linked` as `"x/y"`.
    async fn coverage(&self, ctx: &Ctx<'_>) -> Result<Option<Value>, ToolError> {
        let (nodes, capped) = self.scan_nodes(ctx, &[AppNodeKind::Endpoint, AppNodeKind::ClientCall]).await?;
        if capped {
            return Ok(None);
        }
        let endpoints: Vec<u32> = nodes.iter().filter(|n| n.kind == AppNodeKind::Endpoint).map(|n| n.id).collect();
        let calls: Vec<u32> = nodes.iter().filter(|n| n.kind == AppNodeKind::ClientCall).map(|n| n.id).collect();
        let graph = if nodes.is_empty() {
            AppGraph::default()
        } else {
            let ids: Vec<u32> = nodes.iter().map(|n| n.id).collect();
            self.knowledge.app_neighborhood(ctx.org(), &ctx.key(), &ids, 1, u32::try_from(MAX_SCAN_NODES * 2).unwrap_or(u32::MAX)).await?
        };
        let handled: BTreeSet<u32> = graph.edges.iter().filter(|e| e.kind == AppEdgeKind::HandledBy).map(|e| e.from).collect();
        let linked: BTreeSet<u32> = graph.edges.iter().filter(|e| e.kind == AppEdgeKind::CallsEndpoint).map(|e| e.from).collect();
        Ok(Some(json!({
            "endpoints_with_handler": format!("{}/{}", endpoints.iter().filter(|id| handled.contains(id)).count(), endpoints.len()),
            "client_calls_linked": format!("{}/{}", calls.iter().filter(|id| linked.contains(id)).count(), calls.len()),
        })))
    }

    pub(crate) async fn route_info(&self, scope: &KnowledgeScope, input: RouteInfoInput) -> Result<Envelope, ToolError> {
        check_scope(scope, input.repo.as_deref(), input.sha.as_deref())?;
        if input.url.trim().is_empty() {
            return Err(ToolError::invalid("url is empty"));
        }
        let ctx = self.ctx(scope).await?;
        let request = request_key(&input.url, input.method.as_deref());
        if !ctx.ready(Layer::AppMap) {
            return self.route_info_by_text(&ctx, &request).await;
        }
        let (nodes, capped) = self.scan_nodes(&ctx, &[AppNodeKind::Endpoint, AppNodeKind::Page]).await?;
        let best = best_routes(&request, &nodes);
        let mut env = ctx.envelope(Layer::AppMap);
        env.note(format!("canonical request: {request}"));
        if capped {
            env.note(format!("matched against the first {MAX_SCAN_NODES} routes only"));
        }
        let errors_rev = ctx.version.revs.filter(|_| ctx.ready(Layer::ErrorSites)).map(|r| r.errors);
        for (node, _) in best {
            env.items.push(self.route_item(&ctx, node, errors_rev).await?);
        }
        if env.items.is_empty() {
            env.note("no route matches; try search_code with a literal path segment");
        }
        Ok(env)
    }

    async fn route_item(&self, ctx: &Ctx<'_>, node: &AppNode, errors_rev: Option<ErrorsRev>) -> Result<Value, ToolError> {
        let (org, key) = (ctx.org(), ctx.key());
        let route = node_route(node);
        let mut route_json = json!({
            "kind": if node.kind == AppNodeKind::Page { "page" } else { "endpoint" },
            "method": route.as_ref().and_then(|r| r.method().map(str::to_owned)),
            "pattern": route.as_ref().map(|r| r.path().to_string()),
            "framework": node.framework,
        });
        if let Some((path, line)) = &node.at {
            route_json["citation"] = ctx.cite(path, *line, *line);
        }
        let around = self.knowledge.app_neighborhood(org, &key, &[node.id], 1, 100).await?;
        let by_id: BTreeMap<u32, &AppNode> = around.nodes.iter().map(|n| (n.id, n)).collect();
        let handler_node = around
            .edges
            .iter()
            .find(|e| e.from == node.id && e.kind == AppEdgeKind::HandledBy)
            .and_then(|e| by_id.get(&e.to).copied());
        let callers: Vec<Value> = around
            .edges
            .iter()
            .filter(|e| e.to == node.id && matches!(e.kind, AppEdgeKind::CallsEndpoint | AppEdgeKind::Renders))
            .filter_map(|e| by_id.get(&e.from))
            .map(|n| {
                let mut caller = json!({ "kind": n.kind, "key": n.key, "label": n.label });
                if let Some((path, line)) = &n.at {
                    caller["citation"] = ctx.cite(path, *line, *line);
                }
                caller
            })
            .collect();
        let mut item = json!({ "route": route_json, "callers": callers, "touches": [], "error_sites": [] });
        // Page components and handlers carry the symbol.
        let symbol = handler_node.and_then(|h| h.symbol.clone()).or_else(|| node.symbol.clone());
        if let Some(handler) = handler_node {
            let touched = self.knowledge.app_neighborhood(org, &key, &[handler.id], 1, 100).await?;
            let touched_by: BTreeMap<u32, &AppNode> = touched.nodes.iter().map(|n| (n.id, n)).collect();
            item["touches"] = Value::Array(
                touched
                    .edges
                    .iter()
                    .filter(|e| e.from == handler.id && matches!(e.kind, AppEdgeKind::Reads | AppEdgeKind::Writes | AppEdgeKind::Enqueues | AppEdgeKind::Checks))
                    .filter_map(|e| touched_by.get(&e.to).map(|n| (e, n)))
                    .map(|(e, n)| {
                        let mut touch = json!({ "kind": n.kind, "key": n.key, "edge": e.kind });
                        if let Some((path, line)) = &n.at {
                            touch["citation"] = ctx.cite(path, *line, *line);
                        }
                        touch
                    })
                    .collect(),
            );
        }
        if let Some(symbol) = symbol {
            if ctx.ready(Layer::Symbols) {
                if let Some(hit) = self.knowledge.symbols_by_ref(org, &key, std::slice::from_ref(&symbol)).await?.into_iter().next() {
                    item["handler"] = json!({ "name": hit.def.qualified, "signature": hit.def.signature, "citation": ctx.cite_symbol(&hit) });
                }
            }
            if let Some(rev) = errors_rev {
                let sites = self.knowledge.error_sites_in(org, &key, rev, &symbol, 20).await?;
                item["error_sites"] = Value::Array(
                    sites
                        .iter()
                        .map(|s| {
                            json!({ "template": s.site.template, "kind": s.site.kind, "http_status": s.site.http_status, "citation": ctx.cite(&s.path, s.site.pos.0, s.site.pos.0) })
                        })
                        .collect(),
                );
            }
        }
        Ok(item)
    }

    /// AppMap not Ready: literal search for the longest static run of the URL path.
    async fn route_info_by_text(&self, ctx: &Ctx<'_>, request: &RouteKey) -> Result<Envelope, ToolError> {
        let mut env = ctx.envelope(Layer::Files);
        env.degraded = Some(Degraded::new(ctx.not_ready_reason(Layer::AppMap), Layer::AppMap));
        let Some(run) = longest_static_run(request.path()) else {
            env.note("the URL has no literal path segment to search for");
            return Ok(env);
        };
        let mut query = TextQuery::new(run.clone(), PatternKind::Literal);
        query.case = CaseMode::Sensitive;
        query.max_hits = 20;
        let hits = self.indexer.search(ctx.org(), &ctx.key(), &query, false).await?;
        env.note(format!("literal search for {run:?}"));
        env.items = hits.hits.iter().map(|h| json!({ "citation": ctx.cite(&h.path, h.line, h.line), "line_text": h.text })).collect();
        Ok(env)
    }
}

/// The request as a route key: canonical path, method when given.
fn request_key(url: &str, method: Option<&str>) -> RouteKey {
    RouteKey::new(method, RoutePattern::from_url(url))
}

/// Up to five routes with the most literal segments, then method equality.
fn best_routes<'a>(request: &RouteKey, nodes: &'a [AppNode]) -> Vec<(&'a AppNode, mage_core::domain::knowledge::RouteMatch)> {
    let mut scored: Vec<_> = nodes.iter().filter_map(|n| node_route(n).and_then(|r| route_match(request, &r)).map(|m| (n, m))).collect();
    scored.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.id.cmp(&b.0.id)));
    if let Some(best) = scored.first().map(|(_, m)| m.path.literal) {
        scored.retain(|(_, m)| m.path.literal == best);
    }
    scored.truncate(ROUTES_PER_ANSWER);
    scored
}
