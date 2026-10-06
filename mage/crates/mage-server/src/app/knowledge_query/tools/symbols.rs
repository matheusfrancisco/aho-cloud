//! `find_symbol`, `references`, `call_graph` (layers Symbols and Refs, knowledge.md).

use std::collections::{BTreeMap, BTreeSet, HashMap};

use mage_core::domain::{FileClass, Language, Layer, RepoPath};
use mage_core::ports::knowledge::{
    CallDirection, NameMatch, NameRefQuery, PageReq, Precision, RefHit, RefKind, ScipIndexInfo, ScipSymbolHit, SymbolHit, SymbolKind,
    SymbolQuery, SymbolRef,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::super::envelope::{CursorScope, Degraded, Envelope, ListField, StoreKey, ToolError};
use super::super::inputs::{
    CallGraphInput, DirectionArg, FindSymbolInput, MatchArg, RefKindArg, ReferencesInput, check_len, check_range, parse_lang, parse_path,
};
use super::super::scope::KnowledgeScope;
use super::super::service::{Ctx, Service, check_scope};
use super::super::text::{head, split_qualified};
use super::{resume_key, store_page, symbol_brief};

/// Name-level references collected per call before in-memory filtering and paging.
const MAX_NAME_REFS: usize = 2000;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FindKey {
    m: NameMatch,
    #[serde(flatten)]
    page: StoreKey,
}

/// Which source answered `references`; part of its cursor.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "p", rename_all = "snake_case")]
enum RefsKey {
    Scip { symbol: String, #[serde(flatten)] page: StoreKey },
    Name { o: u32 },
}

fn precision_name(p: &Precision) -> &'static str {
    match p {
        Precision::Precise => "precise",
        Precision::PreciseStale { .. } => "precise_stale",
        Precision::Name => "name",
    }
}

fn mode_name(mode: NameMatch) -> &'static str {
    match mode {
        NameMatch::Exact => "exact",
        NameMatch::CaseInsensitive => "case_insensitive",
        NameMatch::Prefix => "prefix",
        NameMatch::Fuzzy => "fuzzy",
    }
}

/// SCIP symbol strings carry a scheme and spaces (`scip-go gomod github.com/x v1 pkg/Func().`).
fn looks_like_scip(symbol: &str) -> bool {
    symbol.trim().contains(' ')
}

/// A SCIP symbol found at a stale index position must still name the expected definition.
fn scip_names(hit: &ScipSymbolHit, name: &str) -> bool {
    if hit.display_name.as_deref() == Some(name) {
        return true;
    }
    let descriptor = hit.symbol.trim_end_matches(['.', '#', '/', ':', '(', ')', '!']);
    descriptor.ends_with(name) || descriptor.ends_with(&format!("{name}()"))
}

impl Service {
    /// The definition a `symbol` / `path`+`line` argument pair names.
    pub(crate) async fn resolve_symbol(
        &self,
        ctx: &Ctx<'_>,
        symbol: Option<&str>,
        path: Option<&str>,
        line: Option<u32>,
    ) -> Result<Option<SymbolHit>, ToolError> {
        let (org, key) = (ctx.org(), ctx.key());
        match (symbol, path, line) {
            (Some(symbol), _, _) => {
                let (qualifier, name) = split_qualified(symbol);
                for mode in [NameMatch::Exact, NameMatch::CaseInsensitive] {
                    let q = SymbolQuery {
                        name: name.clone(),
                        qualifier: qualifier.clone(),
                        mode,
                        kinds: Vec::new(),
                        lang: None,
                        path_prefix: None,
                        include_tests: true,
                    };
                    let hits = self.knowledge.find_symbols(org, &key, &q, &PageReq::first(5)).await?;
                    if let Some(hit) = hits.items.into_iter().next() {
                        return Ok(Some(hit));
                    }
                }
                Ok(None)
            }
            (None, Some(path), Some(line)) => {
                let path = parse_path(path)?;
                let chain = self.knowledge.symbols_at(org, &key, &path, line).await?;
                Ok(chain.into_iter().next())
            }
            _ => Err(ToolError::invalid("give `symbol`, or `path` and `line`")),
        }
    }

    /// `(index, precision)` when Refs is Ready and an index covers the repo.
    pub(crate) async fn scip_index(&self, ctx: &Ctx<'_>) -> Result<Option<(ScipIndexInfo, Precision)>, ToolError> {
        if !ctx.ready(Layer::Refs) {
            return Ok(None);
        }
        let Some(index) = self.knowledge.scip_index_for(ctx.org(), &ctx.key()).await? else { return Ok(None) };
        let precision = if index.sha == ctx.scope.sha { Precision::Precise } else { Precision::PreciseStale { from_sha: index.sha } };
        Ok(Some((index, precision)))
    }

    /// Names of `refs` (container / enclosing symbols) in one store call.
    pub(crate) async fn names_of(&self, ctx: &Ctx<'_>, refs: Vec<SymbolRef>) -> Result<HashMap<SymbolRef, SymbolHit>, ToolError> {
        let unique: Vec<SymbolRef> = refs.into_iter().collect::<BTreeSet<_>>().into_iter().collect();
        if unique.is_empty() {
            return Ok(HashMap::new());
        }
        let hits = self.knowledge.symbols_by_ref(ctx.org(), &ctx.key(), &unique).await?;
        Ok(hits.into_iter().map(|hit| (hit.at.clone(), hit)).collect())
    }

    pub(crate) async fn find_symbol(&self, scope: &KnowledgeScope, mut input: FindSymbolInput) -> Result<Envelope, ToolError> {
        check_scope(scope, input.repo.as_deref(), input.sha.as_deref())?;
        check_len("name", &input.name, 1, 256)?;
        check_range("limit", input.limit, 1, 50)?;
        let lang = input.lang.as_deref().map(parse_lang).transpose()?;
        let path_prefix = input.path_prefix.as_deref().map(parse_path).transpose()?;
        let cursor = input.cursor.take();
        let cursors = CursorScope::new("find_symbol", &input);
        let resumed = match cursors.decode_opt(cursor.as_deref())? {
            Some(k) => Some(serde_json::from_str::<FindKey>(&k).map_err(|_| ToolError::bad_cursor())?),
            None => None,
        };
        let ctx = self.ctx(scope).await?;
        ctx.require(Layer::Symbols, "use search_code")?;
        let (org, key) = (ctx.org(), ctx.key());
        let (qualifier, name) = split_qualified(&input.name);
        let modes: Vec<NameMatch> = match (&resumed, input.matching) {
            (Some(k), _) => vec![k.m],
            (None, MatchArg::Auto) => vec![NameMatch::Exact, NameMatch::CaseInsensitive, NameMatch::Prefix, NameMatch::Fuzzy],
            (None, MatchArg::Exact) => vec![NameMatch::Exact],
            (None, MatchArg::Prefix) => vec![NameMatch::Prefix],
            (None, MatchArg::Fuzzy) => vec![NameMatch::Fuzzy],
        };
        let page_key = resumed.as_ref().map(|k| k.page.clone()).unwrap_or_default();
        let mut found = (NameMatch::Exact, Vec::new(), None);
        for mode in modes {
            let q = SymbolQuery {
                name: name.clone(),
                qualifier: qualifier.clone(),
                mode,
                kinds: input.kind.iter().map(|k| SymbolKind::from(*k)).collect(),
                lang,
                path_prefix: path_prefix.clone(),
                include_tests: input.include_tests,
            };
            let (hits, next) = store_page(&page_key, input.limit, |page| async move { self.knowledge.find_symbols(org, &key, &q, &page).await }).await?;
            let done = !hits.is_empty();
            found = (mode, hits, next);
            if done {
                break;
            }
        }
        let (mode, hits, next) = found;
        let mut env = ctx.envelope(Layer::Symbols);
        if input.matching == MatchArg::Auto && resumed.is_none() {
            env.note(format!("matched by {}", mode_name(mode)));
        }
        let containers = self
            .names_of(&ctx, hits.iter().filter_map(|h| h.def.container.map(|ord| SymbolRef { ord, ..h.at.clone() })).collect())
            .await?;
        let exact_index = match self.scip_index(&ctx).await? {
            Some((index, Precision::Precise)) => Some(index),
            _ => None,
        };
        let mut items = Vec::with_capacity(hits.len());
        for hit in &hits {
            let mut item = json!({
                "name": hit.def.name,
                "qualified": hit.def.qualified,
                "kind": hit.def.kind,
                "signature": hit.def.signature,
                "doc_head": hit.def.doc.as_deref().map(|d| head(d, 200)),
                "exported": hit.def.exported,
                "citation": ctx.cite_symbol(hit),
            });
            if let Some(container) = hit.def.container.and_then(|ord| containers.get(&SymbolRef { ord, ..hit.at.clone() })) {
                item["container"] = json!(container.def.name);
            }
            if let Some(index) = &exact_index {
                let (line, col) = hit.def.name_pos;
                if let Some(scip) = self.knowledge.scip_symbol_at(org, index.artifact, &hit.at.path, line, Some(col.saturating_sub(1))).await? {
                    item["scip_symbol"] = json!(scip.symbol);
                }
            }
            items.push(item);
        }
        env.items = items;
        let kept = env.fit(ListField::Items).unwrap_or(env.items.len());
        env.next_cursor =
            resume_key(&page_key, kept, hits.len(), next).map(|page| cursors.encode(&serde_json::to_string(&FindKey { m: mode, page }).unwrap_or_default()));
        Ok(env)
    }

    pub(crate) async fn references(&self, scope: &KnowledgeScope, mut input: ReferencesInput) -> Result<Envelope, ToolError> {
        check_scope(scope, input.repo.as_deref(), input.sha.as_deref())?;
        check_range("limit", input.limit, 1, 100)?;
        let cursor = input.cursor.take();
        let cursors = CursorScope::new("references", &input);
        let resumed = match cursors.decode_opt(cursor.as_deref())? {
            Some(k) => Some(serde_json::from_str::<RefsKey>(&k).map_err(|_| ToolError::bad_cursor())?),
            None => None,
        };
        let ctx = self.ctx(scope).await?;
        let (org, key) = (ctx.org(), ctx.key());
        let given_scip = input.symbol.as_deref().filter(|s| looks_like_scip(s)).map(str::to_owned);
        let def = match &given_scip {
            Some(_) => None,
            None if ctx.ready(Layer::Symbols) => self.resolve_symbol(&ctx, input.symbol.as_deref(), input.path.as_deref(), input.line).await?,
            None => None,
        };
        let index = self.scip_index(&ctx).await?;

        // Refs path: a SCIP symbol for the target in a ready index.
        let mut scip_symbol = match &resumed {
            Some(RefsKey::Scip { symbol, .. }) => Some(symbol.clone()),
            _ => given_scip.clone(),
        };
        if scip_symbol.is_none() && !matches!(resumed, Some(RefsKey::Name { .. })) {
            if let Some((index, precision)) = &index {
                let position = match (&input.path, input.line, &def) {
                    (Some(path), Some(line), _) => Some((parse_path(path)?, line, input.column.map(|c| c.saturating_sub(1)))),
                    (_, _, Some(def)) => Some((def.at.path.clone(), def.def.name_pos.0, Some(def.def.name_pos.1.saturating_sub(1)))),
                    _ => None,
                };
                if let Some((path, line, col)) = position {
                    let hit = self.knowledge.scip_symbol_at(org, index.artifact, &path, line, col).await?;
                    scip_symbol = hit
                        .filter(|hit| match (precision, &def) {
                            (Precision::PreciseStale { .. }, Some(def)) => scip_names(hit, &def.def.name),
                            _ => true,
                        })
                        .map(|hit| hit.symbol);
                }
            }
        }
        if let (Some(symbol), Some((index, precision))) = (scip_symbol, &index) {
            let page_key = match &resumed {
                Some(RefsKey::Scip { page, .. }) => page.clone(),
                _ => StoreKey::default(),
            };
            let (hits, next) = store_page(&page_key, input.limit, |page| {
                let symbol = symbol.clone();
                async move { self.knowledge.scip_occurrences(org, index.artifact, &key, &symbol, false, &page).await }
            })
            .await?;
            let fetched = hits.len();
            let hits = self.filter_tests(&ctx, hits, input.include_tests).await?;
            let hits: Vec<RefHit> = hits
                .into_iter()
                .filter(|h| {
                    input.kinds.is_empty()
                        || input.kinds.iter().any(|k| match h.kind {
                            RefKind::Import => *k == RefKindArg::Import,
                            _ => *k != RefKindArg::Import,
                        })
                })
                .collect();
            let filtered = hits.len() < fetched;
            let mut env = ctx.envelope(Layer::Refs);
            env.items = self.ref_items(&ctx, &hits, precision).await?;
            if let Precision::PreciseStale { from_sha } = precision {
                env.note(format!("refs from sha {from_sha}, unchanged files only"));
            }
            let kept = env.fit(ListField::Items).unwrap_or(env.items.len());
            let resume = if filtered {
                next.map(|after| StoreKey { after: Some(after), skip: 0 })
            } else {
                resume_key(&page_key, kept, fetched, next)
            };
            env.next_cursor = resume.map(|page| {
                cursors.encode(&serde_json::to_string(&RefsKey::Scip { symbol: symbol.clone(), page }).unwrap_or_default())
            });
            return Ok(env);
        }

        // Name path.
        ctx.require(Layer::Symbols, "use search_code")?;
        let Some(def) = def else {
            return Err(ToolError::not_found("no definition matches; try find_symbol or search_code"));
        };
        let offset = match &resumed {
            Some(RefsKey::Name { o }) => *o as usize,
            _ => 0,
        };
        let kinds: Vec<RefKind> = input.kinds.iter().flat_map(|k| k.name_kinds().iter().copied()).collect();
        if !input.kinds.is_empty() && kinds.is_empty() {
            let mut env = ctx.envelope(Layer::Symbols);
            env.note("name-level references do not classify writes; use search_code");
            return Ok(env);
        }
        let (refs, capped) = self.name_level_refs(&ctx, &def, &kinds, input.include_tests).await?;
        let page: Vec<RefHit> = refs.iter().skip(offset).take(input.limit as usize).cloned().collect();
        let mut env = ctx.envelope(Layer::Symbols);
        let reason = if ctx.scope.layer_disabled(Layer::Refs) {
            "layer_disabled"
        } else if index.is_some() {
            "symbol not found in the SCIP index"
        } else {
            "no SCIP index for this sha"
        };
        env.degraded = Some(Degraded::new(reason, Layer::Refs));
        if capped {
            env.note(format!("first {MAX_NAME_REFS} name matches only"));
        }
        env.items = self.ref_items(&ctx, &page, &Precision::Name).await?;
        let kept = env.fit(ListField::Items).unwrap_or(env.items.len());
        if offset + kept < refs.len() {
            let next = RefsKey::Name { o: u32::try_from(offset + kept).unwrap_or(u32::MAX) };
            env.next_cursor = Some(cursors.encode(&serde_json::to_string(&next).unwrap_or_default()));
        }
        Ok(env)
    }

    /// Refs named like `def`, narrowed to the definition's package (Go) or to files that import
    /// its namespace (Clojure/CLJS). Sorted by path, then position.
    async fn name_level_refs(&self, ctx: &Ctx<'_>, def: &SymbolHit, kinds: &[RefKind], include_tests: bool) -> Result<(Vec<RefHit>, bool), ToolError> {
        let (org, key) = (ctx.org(), ctx.key());
        let collect = |qualifier: Option<String>, name: String, kinds: Vec<RefKind>| async move {
            let q = NameRefQuery { name, qualifier, kinds, include_tests };
            let mut out = Vec::new();
            let mut page = PageReq::first(500);
            loop {
                let got = self.knowledge.name_refs(org, &key, &q, &page).await?;
                out.extend(got.items);
                match got.next {
                    Some(next) if out.len() < MAX_NAME_REFS => page.after = Some(next),
                    Some(_) => return Ok::<_, ToolError>((out, true)),
                    None => return Ok((out, false)),
                }
            }
        };
        let name = def.def.name.clone();
        let lang = language_of(&def.at.path);
        let module = || async {
            let file = self.knowledge.symbols_in_file(org, &key, &def.at.path).await?;
            Ok::<_, ToolError>(file.into_iter().find(|s| s.def.kind == SymbolKind::Module).map(|s| s.def.name))
        };
        let (mut refs, capped) = match lang {
            Some(Language::Go) if !matches!(def.def.kind, SymbolKind::Method | SymbolKind::Field) => {
                let dir = def.at.path.parent().unwrap_or("").to_owned();
                let package = module().await?.unwrap_or_else(|| dir.rsplit('/').next().unwrap_or("").to_owned());
                let (local, capped_local) = collect(None, name.clone(), kinds.to_vec()).await?;
                let (qualified, capped_qualified) = collect(Some(package), name, kinds.to_vec()).await?;
                let mut refs: Vec<RefHit> = local.into_iter().filter(|r| r.path.parent().unwrap_or("") == dir).collect();
                refs.extend(qualified);
                (refs, capped_local || capped_qualified)
            }
            Some(Language::Clojure | Language::ClojureScript | Language::Cljc) => {
                let (all, capped) = collect(None, name, kinds.to_vec()).await?;
                match module().await? {
                    Some(ns) => {
                        let (imports, _) = collect(None, ns, vec![RefKind::Import]).await?;
                        let importers: BTreeSet<RepoPath> = imports.into_iter().map(|r| r.path).collect();
                        (all.into_iter().filter(|r| r.path == def.at.path || importers.contains(&r.path)).collect(), capped)
                    }
                    None => (all, capped),
                }
            }
            _ => collect(None, name, kinds.to_vec()).await?,
        };
        refs.sort_by(|a, b| a.path.cmp(&b.path).then(a.pos.cmp(&b.pos)));
        refs.dedup_by(|a, b| a.path == b.path && a.pos == b.pos);
        Ok((refs, capped))
    }

    /// Drops hits in Test files unless asked (SCIP hits carry no file class).
    async fn filter_tests(&self, ctx: &Ctx<'_>, hits: Vec<RefHit>, include_tests: bool) -> Result<Vec<RefHit>, ToolError> {
        if include_tests || hits.is_empty() {
            return Ok(hits);
        }
        let paths: Vec<RepoPath> = hits.iter().map(|h| h.path.clone()).collect::<BTreeSet<_>>().into_iter().collect();
        let entries = self.knowledge.manifest_entries(ctx.org(), &ctx.key(), &paths).await?;
        let tests: BTreeSet<RepoPath> = entries.into_iter().filter(|e| e.class == FileClass::Test).map(|e| e.path).collect();
        Ok(hits.into_iter().filter(|h| !tests.contains(&h.path)).collect())
    }

    async fn ref_items(&self, ctx: &Ctx<'_>, hits: &[RefHit], precision: &Precision) -> Result<Vec<Value>, ToolError> {
        let enclosing = self.names_of(ctx, hits.iter().filter_map(|h| h.enclosing.clone()).collect()).await?;
        let mut texts: BTreeMap<RepoPath, Option<Vec<String>>> = BTreeMap::new();
        for path in hits.iter().map(|h| &h.path) {
            if !texts.contains_key(path) {
                let lines = self.source.read_all(ctx.org(), &ctx.key(), path).await.ok().map(|s| s.lines);
                texts.insert(path.clone(), lines);
            }
        }
        Ok(hits
            .iter()
            .map(|h| {
                let line_text = texts
                    .get(&h.path)
                    .and_then(|lines| lines.as_ref())
                    .and_then(|lines| lines.get(h.pos.0.saturating_sub(1) as usize))
                    .map(|l| head(l, 240));
                let mut item = json!({
                    "citation": ctx.cite(&h.path, h.pos.0, h.pos.0),
                    "line_text": line_text,
                    "kind": h.kind,
                    "precision": precision_name(precision),
                });
                if let Some(e) = h.enclosing.as_ref().and_then(|e| enclosing.get(e)) {
                    item["enclosing"] = symbol_brief(e);
                }
                item
            })
            .collect())
    }

    pub(crate) async fn call_graph(&self, scope: &KnowledgeScope, input: CallGraphInput) -> Result<Envelope, ToolError> {
        check_scope(scope, input.repo.as_deref(), input.sha.as_deref())?;
        check_range("depth", input.depth, 1, 3)?;
        check_range("limit", input.limit, 1, 100)?;
        let ctx = self.ctx(scope).await?;
        ctx.require(Layer::Symbols, "use search_code")?;
        let (org, key) = (ctx.org(), ctx.key());
        let Some(root) = self.resolve_symbol(&ctx, input.symbol.as_deref(), input.path.as_deref(), input.line).await? else {
            return Err(ToolError::not_found("no symbol at that position or with that name"));
        };
        let mut precision = match self.scip_index(&ctx).await? {
            Some((_, p)) => p,
            None => Precision::Name,
        };
        let directions: &[CallDirection] = match input.direction {
            DirectionArg::Callers => &[CallDirection::Callers],
            DirectionArg::Callees => &[CallDirection::Callees],
            DirectionArg::Both => &[CallDirection::Callers, CallDirection::Callees],
        };
        let limit = input.limit as usize;
        let mut ids: BTreeMap<SymbolRef, usize> = BTreeMap::from([(root.at.clone(), 0)]);
        let mut order = vec![root.at.clone()];
        let mut edges: BTreeMap<(usize, usize), Vec<(RepoPath, u32)>> = BTreeMap::new();
        let mut frontier = vec![root.at.clone()];
        let mut fell_back = false;
        'levels: for _ in 0..input.depth {
            let mut discovered = Vec::new();
            for dir in directions {
                let remaining = limit.saturating_sub(order.len());
                let budget = u32::try_from((remaining * 4).max(limit)).unwrap_or(u32::MAX);
                let found = match self.knowledge.call_edges(org, &key, &frontier, *dir, &precision, budget).await {
                    Err(mage_core::ports::knowledge::KnowledgeError::NotFound) if precision != Precision::Name => {
                        precision = Precision::Name;
                        fell_back = true;
                        self.knowledge.call_edges(org, &key, &frontier, *dir, &precision, budget).await?
                    }
                    other => other?,
                };
                for edge in found {
                    let mut endpoint = |r: &SymbolRef, order: &mut Vec<SymbolRef>, discovered: &mut Vec<SymbolRef>| -> Option<usize> {
                        if let Some(id) = ids.get(r) {
                            return Some(*id);
                        }
                        if order.len() >= limit {
                            return None;
                        }
                        ids.insert(r.clone(), order.len());
                        order.push(r.clone());
                        discovered.push(r.clone());
                        Some(order.len() - 1)
                    };
                    let (Some(from), Some(to)) = (endpoint(&edge.from, &mut order, &mut discovered), endpoint(&edge.to, &mut order, &mut discovered))
                    else {
                        continue;
                    };
                    edges.entry((from, to)).or_default().extend(edge.sites);
                }
                if order.len() >= limit {
                    break 'levels;
                }
            }
            if discovered.is_empty() {
                break;
            }
            frontier = discovered;
        }
        let info = self.names_of(&ctx, order.clone()).await?;
        let nodes: Vec<Value> = order
            .iter()
            .enumerate()
            .map(|(id, r)| match info.get(r) {
                Some(hit) => json!({ "id": id, "name": hit.def.name, "kind": hit.def.kind, "citation": ctx.cite_symbol(hit) }),
                None => json!({ "id": id, "name": Value::Null, "kind": Value::Null, "citation": ctx.cite(&r.path, 1, 1) }),
            })
            .collect();
        let edges: Vec<Value> = edges
            .into_iter()
            .map(|((from, to), mut sites)| {
                sites.sort();
                sites.dedup();
                let sites: Vec<Value> = sites.iter().take(3).map(|(path, line)| ctx.cite(path, *line, *line)).collect();
                json!({ "from": from, "to": to, "sites": sites })
            })
            .collect();
        let layer = if precision == Precision::Name { Layer::Symbols } else { Layer::Refs };
        let mut env = ctx.envelope(layer);
        if precision == Precision::Name {
            let reason = if fell_back {
                "SCIP index not usable for this sha".to_owned()
            } else if ctx.scope.layer_disabled(Layer::Refs) {
                "layer_disabled".to_owned()
            } else {
                "no SCIP index for this sha".to_owned()
            };
            env.degraded = Some(Degraded::new(reason, Layer::Refs));
        }
        if let Precision::PreciseStale { from_sha } = &precision {
            env.note(format!("refs from sha {from_sha}, unchanged files only"));
        }
        if order.len() >= limit {
            env.note(format!("stopped at {limit} nodes"));
        }
        env.set("precision", json!(precision_name(&precision)));
        set_graph(&mut env, nodes, edges);
        Ok(env)
    }
}

/// Puts `nodes`/`edges` on the envelope and enforces the size cap by dropping trailing nodes and
/// the edges that touch them.
pub(crate) fn set_graph(env: &mut Envelope, nodes: Vec<Value>, edges: Vec<Value>) {
    env.set("nodes", Value::Array(nodes));
    env.set("edges", Value::Array(edges));
    if let Some(kept) = env.fit(ListField::Extra("nodes")) {
        let ids: BTreeSet<u64> = env.extra["nodes"].as_array().into_iter().flatten().filter_map(|n| n["id"].as_u64()).collect();
        if let Some(Value::Array(edges)) = env.extra.get_mut("edges") {
            edges.retain(|e| e["from"].as_u64().is_some_and(|f| ids.contains(&f)) && e["to"].as_u64().is_some_and(|t| ids.contains(&t)));
        }
        env.note(format!("graph cut to {kept} nodes to fit the result size"));
        // Edges alone can still be too large.
        env.fit(ListField::Extra("edges"));
    }
}

/// Language of a path by extension (enough for package / namespace rules).
fn language_of(path: &RepoPath) -> Option<Language> {
    match path.extension()? {
        "go" => Some(Language::Go),
        "clj" => Some(Language::Clojure),
        "cljs" => Some(Language::ClojureScript),
        "cljc" => Some(Language::Cljc),
        _ => None,
    }
}
