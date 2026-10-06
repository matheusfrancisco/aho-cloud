//! `repo_overview`, `search_code`, `read_file` (layer Files, knowledge.md).

use std::collections::{BTreeMap, BTreeSet};

use mage_core::domain::recipe::RecipeStatus;
use mage_core::domain::{EntryKind, Layer, LineRange, RepoPath};
use mage_core::ports::knowledge::{DirEntryKind, ManifestEntry, NameMatch, PageReq, PathMatch, SymbolKind, SymbolQuery};
use mage_core::wire::indexer::{CaseMode, PatternKind, TextHits, TextQuery};
use serde_json::{Value, json};

use super::super::envelope::{CursorScope, Degraded, Envelope, ListField, StoreKey, ToolError, ToolErrorCode};
use super::super::errors::prepare;
use super::super::inputs::{
    CaseArg, ReadFileInput, RepoOverviewInput, SearchCodeInput, SearchMode, check_len, check_range, parse_lang, parse_path_or_root,
};
use super::super::scope::KnowledgeScope;
use super::super::service::{Ctx, Service, check_scope};
use super::super::source::{MAX_TEXT_BYTES, SourceReadError};
use super::super::text::{cut_line, language_globs};
use super::{resume_key, store_page, symbol_brief};
use crate::ports::indexer_link::IndexerLinkError;

/// `repo_overview` output budget.
const OVERVIEW_MAX_BYTES: usize = 8 * 1024;
/// `read_file` window.
const READ_MAX_LINES: u32 = 400;
/// Bytes of `text` per `read_file` result (the envelope around it stays under 24 KiB).
const READ_MAX_TEXT_BYTES: usize = 22 * 1024;
const READ_MAX_LINE_CHARS: usize = 2000;
/// Directory listing page.
const DIR_PAGE: u32 = 200;
/// Root files reported as entry points (`*` = prefix).
const WELL_KNOWN: [&str; 10] =
    ["go.mod", "package.json", "deps.edn", "shadow-cljs.edn", "Dockerfile", "docker-compose*", "Makefile", "README*", "ARCHITECTURE*", "Cargo.toml"];

fn well_known(name: &str) -> bool {
    WELL_KNOWN.iter().any(|pattern| match pattern.strip_suffix('*') {
        Some(prefix) => name.starts_with(prefix),
        None => name == *pattern,
    })
}

fn recipe_status(status: &RecipeStatus) -> &'static str {
    match status {
        RecipeStatus::Draft => "draft",
        RecipeStatus::Validated { .. } => "validated",
        RecipeStatus::Broken { .. } => "broken",
    }
}

impl Service {
    pub(crate) async fn repo_overview(&self, scope: &KnowledgeScope, input: RepoOverviewInput) -> Result<Envelope, ToolError> {
        check_scope(scope, input.repo.as_deref(), input.sha.as_deref())?;
        let ctx = self.ctx(scope).await?;
        let (org, key) = (ctx.org(), ctx.key());
        let stats = self.knowledge.version_stats(org, &key).await?;
        let mut env = ctx.envelope(Layer::Files);

        let mut entry_points = Vec::new();
        let root = self.knowledge.list_dir(org, &key, None, &PageReq::first(500)).await?;
        for entry in root.items.iter().filter(|e| e.kind == DirEntryKind::File && well_known(&e.name)) {
            if let Ok(path) = RepoPath::new(entry.name.clone()) {
                entry_points.push(json!({ "kind": "file", "citation": ctx.cite(&path, 1, 1) }));
            }
        }
        if let Ok(docs) = RepoPath::new("docs") {
            let listing = self.knowledge.list_dir(org, &key, Some(&docs), &PageReq::first(200)).await?;
            for entry in listing.items.iter().filter(|e| e.kind == DirEntryKind::File && e.name.starts_with("README")) {
                if let Ok(path) = RepoPath::new(format!("docs/{}", entry.name)) {
                    entry_points.push(json!({ "kind": "file", "citation": ctx.cite(&path, 1, 1) }));
                }
            }
        }
        if ctx.ready(Layer::Symbols) {
            let q = SymbolQuery {
                name: "main".into(),
                qualifier: None,
                mode: NameMatch::Exact,
                kinds: vec![SymbolKind::Function],
                lang: Some(mage_core::domain::Language::Go),
                path_prefix: None,
                include_tests: false,
            };
            let mains = self.knowledge.find_symbols(org, &key, &q, &PageReq::first(20)).await?;
            for hit in mains.items.iter().take(10) {
                let file = self.knowledge.symbols_in_file(org, &key, &hit.at.path).await?;
                if file.iter().any(|s| s.def.kind == SymbolKind::Module && s.def.name == "main") {
                    entry_points.push(json!({ "kind": "main", "name": hit.def.qualified, "citation": ctx.cite_symbol(hit) }));
                }
            }
        } else {
            env.note(format!("entry-point functions omitted: {}", ctx.not_ready_reason(Layer::Symbols)));
        }

        let mut frameworks = BTreeSet::new();
        if ctx.ready(Layer::AppMap) {
            let mut page = PageReq::first(500);
            for _ in 0..4 {
                let nodes = self.knowledge.app_nodes(org, &key, &[], &page).await?;
                frameworks.extend(nodes.items.iter().filter_map(|n| n.framework));
                match nodes.next {
                    Some(next) => page.after = Some(next),
                    None => break,
                }
            }
        }

        let recipe = self.repos.latest_recipe(org, scope.repo).await?.map(|r| {
            json!({ "status": recipe_status(&r.status), "hash": r.hash, "validated_at_sha": r.validated_at_sha })
        });
        let counts = json!({
            "files": stats.files,
            "bytes": stats.bytes,
            "symbols": ctx.ready(Layer::Symbols).then_some(stats.counts.symbols),
            "error_sites": ctx.ready(Layer::ErrorSites).then_some(stats.counts.error_sites),
            "endpoints": ctx.ready(Layer::AppMap).then_some(stats.counts.endpoints),
            "pages": ctx.ready(Layer::AppMap).then_some(stats.counts.pages),
            "tables": ctx.ready(Layer::AppMap).then_some(stats.counts.tables),
        });
        let mut languages: Vec<Value> =
            stats.by_lang.iter().map(|(lang, files, lines)| json!({ "lang": lang, "files": files, "lines": lines })).collect();
        let mut top_dirs: Vec<Value> = stats.top_dirs.iter().map(|(path, files)| json!({ "path": path, "files": files })).collect();
        if ctx.ready(Layer::Semantic) {
            env.note("repo brief: no summary is stored for this version");
        }
        loop {
            let overview = json!({
                "languages": languages,
                "top_dirs": top_dirs,
                "entry_points": entry_points,
                "frameworks": frameworks,
                "counts": counts,
                "layers": ctx.layer_states(),
                "recipe": recipe,
                "brief": Value::Null,
            });
            let size = serde_json::to_vec(&overview).map_or(0, |v| v.len());
            let longest = [languages.len(), top_dirs.len(), entry_points.len()].into_iter().max().unwrap_or(0);
            if size <= OVERVIEW_MAX_BYTES || longest <= 3 {
                env.set("overview", overview);
                if size > OVERVIEW_MAX_BYTES {
                    env.truncated = true;
                }
                break;
            }
            env.truncated = true;
            for list in [&mut languages, &mut top_dirs, &mut entry_points] {
                if list.len() == longest {
                    list.truncate(list.len() * 3 / 4);
                }
            }
        }
        Ok(env)
    }

    pub(crate) async fn search_code(&self, scope: &KnowledgeScope, mut input: SearchCodeInput) -> Result<Envelope, ToolError> {
        check_scope(scope, input.repo.as_deref(), input.sha.as_deref())?;
        check_len("query", &input.query, 1, 512)?;
        check_range("limit", input.limit, 1, 50)?;
        check_range("context", input.context, 0, 3)?;
        if input.path_glob.len() > 8 || input.exclude_glob.len() > 8 {
            return Err(ToolError::invalid("at most 8 path_glob and 8 exclude_glob entries"));
        }
        let lang = input.lang.as_deref().map(parse_lang).transpose()?;
        let cursor = input.cursor.take();
        let cursors = CursorScope::new("search_code", &input);
        let keyset = cursors.decode_opt(cursor.as_deref())?;
        let ctx = self.ctx(scope).await?;
        let (org, version) = (ctx.org(), ctx.key());
        match input.mode {
            SearchMode::Text | SearchMode::Regex => self.search_text(&ctx, &input, lang, &cursors, keyset).await,
            SearchMode::Symbol => {
                ctx.require(Layer::Symbols, "use search_code with mode \"text\"")?;
                let key = StoreKey::parse(keyset.as_deref())?;
                let q = SymbolQuery {
                    name: input.query.clone(),
                    qualifier: None,
                    mode: NameMatch::Fuzzy,
                    kinds: Vec::new(),
                    lang,
                    path_prefix: None,
                    include_tests: true,
                };
                let (hits, next) =
                    store_page(&key, input.limit, |page| async move { self.knowledge.find_symbols(org, &version, &q, &page).await })
                        .await?;
                let mut env = ctx.envelope(Layer::Symbols);
                env.items = hits
                    .iter()
                    .map(|hit| json!({ "citation": ctx.cite_symbol(hit), "line_text": hit.def.signature, "symbol": symbol_brief(hit) }))
                    .collect();
                finish_store_page(&mut env, &cursors, &key, hits.len(), next);
                Ok(env)
            }
            SearchMode::Path => {
                let key = StoreKey::parse(keyset.as_deref())?;
                let matcher = if input.query.contains(['*', '?']) {
                    PathMatch::Glob(input.query.clone())
                } else {
                    PathMatch::Substring(input.query.clone())
                };
                let (entries, next) =
                    store_page(&key, input.limit, |page| async move { self.knowledge.find_paths(org, &version, &matcher, &page).await })
                        .await?;
                let mut env = ctx.envelope(Layer::Files);
                // The language filter keeps page positions (cursor math) by emitting only matches.
                let fetched = entries.len();
                env.items = entries
                    .iter()
                    .filter(|e| lang.is_none_or(|l| e.lang == Some(l)))
                    .map(|e| json!({ "citation": ctx.cite(&e.path, 1, 1), "lang": e.lang, "class": e.class }))
                    .collect();
                if env.items.len() < fetched {
                    // Filtered pages cannot be cut item-exactly; resume after the whole page.
                    env.next_cursor = next.map(|after| cursors.encode(&StoreKey { after: Some(after), skip: 0 }.to_keyset()));
                } else {
                    finish_store_page(&mut env, &cursors, &key, fetched, next);
                }
                Ok(env)
            }
        }
    }

    async fn search_text(
        &self,
        ctx: &Ctx<'_>,
        input: &SearchCodeInput,
        lang: Option<mage_core::domain::Language>,
        cursors: &CursorScope,
        keyset: Option<String>,
    ) -> Result<Envelope, ToolError> {
        let kind = if input.mode == SearchMode::Regex { PatternKind::Regex } else { PatternKind::Literal };
        if kind == PatternKind::Regex {
            regex::RegexBuilder::new(&input.query)
                .size_limit(1 << 20)
                .build()
                .map_err(|e| ToolError::invalid(format!("invalid regex (Rust `regex` syntax): {e}")))?;
        }
        let after = match keyset.as_deref() {
            None => None,
            Some(k) => Some(serde_json::from_str::<(RepoPath, u32)>(k).map_err(|_| ToolError::bad_cursor())?),
        };
        let mut query = TextQuery::new(input.query.clone(), kind);
        query.case = match input.case {
            CaseArg::Smart => CaseMode::Smart,
            CaseArg::Sensitive => CaseMode::Sensitive,
            CaseArg::Insensitive => CaseMode::Insensitive,
        };
        query.include = input.path_glob.clone();
        query.exclude = input.exclude_glob.clone();
        if let (Some(lang), true) = (lang, input.path_glob.is_empty()) {
            query.include = language_globs(lang).iter().map(|g| (*g).to_owned()).collect();
        }
        query.context = input.context;
        query.max_hits = input.limit;
        query.after = after;
        let found = self.indexer.search(ctx.org(), &ctx.key(), &query, input.include_generated).await;
        let hits = match found {
            Ok(hits) => hits,
            Err(IndexerLinkError::Unavailable(_) | IndexerLinkError::Timeout) if kind == PatternKind::Literal => {
                return self.search_fallback(ctx, input, lang).await;
            }
            Err(e) => return Err(e.into()),
        };
        let TextHits { hits, next, truncated, .. } = hits;
        let hits: Vec<_> = match (lang, input.path_glob.is_empty()) {
            (Some(lang), false) => {
                let globs = language_globs(lang);
                hits.into_iter().filter(|h| globs.iter().any(|g| h.path.as_str().ends_with(g.trim_start_matches("**/*")))).collect()
            }
            _ => hits,
        };
        let mut env = ctx.envelope(Layer::Files);
        let enrich = ctx.ready(Layer::Symbols);
        let mut enclosing: BTreeMap<(RepoPath, u32), Value> = BTreeMap::new();
        if enrich {
            for hit in &hits {
                if enclosing.contains_key(&(hit.path.clone(), hit.line)) {
                    continue;
                }
                let chain = self.knowledge.symbols_at(ctx.org(), &ctx.key(), &hit.path, hit.line).await?;
                if let Some(inner) = chain.first() {
                    enclosing.insert((hit.path.clone(), hit.line), symbol_brief(inner));
                }
            }
        }
        env.items = hits
            .iter()
            .map(|hit| {
                let mut item = json!({
                    "citation": ctx.cite(&hit.path, hit.line, hit.line),
                    "line_text": hit.text,
                    "before": hit.before,
                    "after": hit.after,
                });
                if let Some(symbol) = enclosing.get(&(hit.path.clone(), hit.line)) {
                    item["symbol"] = symbol.clone();
                }
                item
            })
            .collect();
        if truncated == Some(mage_core::wire::indexer::Truncation::Timeout) {
            env.truncated = true;
            env.note("search timed out on the indexer; results are partial");
        }
        let resume = match env.fit(ListField::Items) {
            Some(kept) => hits.get(kept - 1).map(|h| (h.path.clone(), h.line)),
            None => next,
        };
        env.next_cursor = resume.map(|k| cursors.encode(&serde_json::to_string(&k).unwrap_or_default()));
        Ok(env)
    }

    /// Indexer down in `text` mode: symbol names, paths and error templates that contain the query.
    async fn search_fallback(
        &self,
        ctx: &Ctx<'_>,
        input: &SearchCodeInput,
        lang: Option<mage_core::domain::Language>,
    ) -> Result<Envelope, ToolError> {
        let (org, key) = (ctx.org(), ctx.key());
        let limit = input.limit as usize;
        let mut items = Vec::new();
        let layer = if ctx.ready(Layer::Symbols) { Layer::Symbols } else { Layer::Files };
        let mut env = ctx.envelope(layer);
        if ctx.ready(Layer::Symbols) {
            let q = SymbolQuery {
                name: input.query.clone(),
                qualifier: None,
                mode: NameMatch::Fuzzy,
                kinds: Vec::new(),
                lang,
                path_prefix: None,
                include_tests: true,
            };
            let hits = self.knowledge.find_symbols(org, &key, &q, &PageReq::first(input.limit)).await?;
            items.extend(hits.items.iter().map(|hit| json!({ "citation": ctx.cite_symbol(hit), "line_text": hit.def.signature, "symbol": symbol_brief(hit) })));
        }
        let paths = self.knowledge.find_paths(org, &key, &PathMatch::Substring(input.query.clone()), &PageReq::first(input.limit)).await?;
        items.extend(
            paths
                .items
                .iter()
                .filter(|e| lang.is_none_or(|l| e.lang == Some(l)))
                .map(|e| json!({ "citation": ctx.cite(&e.path, 1, 1), "lang": e.lang, "class": e.class })),
        );
        match ctx.version.revs {
            Some(revs) if ctx.ready(Layer::ErrorSites) => {
                let q = prepare(&input.query, input.limit, true);
                let matches = self.knowledge.match_errors(org, &key, revs.errors, &q).await?;
                items.extend(matches.iter().map(|m| {
                    json!({ "citation": ctx.cite(&m.path, m.site.pos.0, m.site.pos.0), "line_text": m.site.template, "error_site": m.site.kind })
                }));
            }
            _ => {}
        }
        items.truncate(limit);
        env.items = items;
        env.degraded = Some(Degraded::new("text_search_unavailable", Layer::Files));
        env.note("the indexer's live search is unavailable: these are symbol, path and error-template matches, not text hits");
        Ok(env)
    }

    pub(crate) async fn read_file(&self, scope: &KnowledgeScope, mut input: ReadFileInput) -> Result<Envelope, ToolError> {
        check_scope(scope, input.repo.as_deref(), input.sha.as_deref())?;
        if input.start_line == 0 {
            return Err(ToolError::invalid("start_line is 1-based"));
        }
        if let Some(end) = input.end_line {
            if end < input.start_line {
                return Err(ToolError::invalid("end_line is before start_line"));
            }
        }
        let path = parse_path_or_root(&input.path)?;
        let cursor = input.cursor.take();
        let cursors = CursorScope::new("read_file", &input);
        let keyset = cursors.decode_opt(cursor.as_deref())?;
        let ctx = self.ctx(scope).await?;
        let Some(path) = path else { return self.list_directory(&ctx, None, &cursors, keyset).await };
        let entry = match self.source.entry(ctx.org(), &ctx.key(), &path).await {
            Ok(entry) => entry,
            Err(SourceReadError::NotFound) => return self.list_directory(&ctx, Some(path), &cursors, keyset).await,
            Err(e) => return Err(source_error(e)),
        };
        match entry.kind {
            EntryKind::Submodule => Err(ToolError::new(ToolErrorCode::NotText, format!("{path} is a submodule"))),
            EntryKind::Symlink => {
                let target = self.source.blob_bytes(ctx.org(), entry.blob).await.map_err(source_error)?;
                let mut env = ctx.envelope(Layer::Files);
                file_fields(&mut env, &entry);
                env.set("text", Value::String(String::from_utf8_lossy(&target).into_owned()));
                env.note("symlink: text is the link target");
                Ok(env)
            }
            EntryKind::File => {
                if !entry.is_text {
                    return Err(ToolError::new(ToolErrorCode::NotText, format!("{path} is binary")));
                }
                if entry.size > MAX_TEXT_BYTES {
                    return Err(ToolError::new(ToolErrorCode::TooLarge, format!("{path} is larger than 2 MiB; use search_code")));
                }
                let start = match keyset {
                    Some(k) => k.parse::<u32>().map_err(|_| ToolError::bad_cursor())?,
                    None => input.start_line,
                };
                self.read_window(&ctx, &entry, start, input.end_line, &cursors).await
            }
        }
    }

    async fn read_window(
        &self,
        ctx: &Ctx<'_>,
        entry: &ManifestEntry,
        start: u32,
        end_line: Option<u32>,
        cursors: &CursorScope,
    ) -> Result<Envelope, ToolError> {
        let window_end = end_line.unwrap_or(u32::MAX).min(start.saturating_add(READ_MAX_LINES - 1));
        let range = LineRange::new(start, window_end.max(start)).map_err(|e| ToolError::invalid(e.to_string()))?;
        let mut env = ctx.envelope(Layer::Files);
        file_fields(&mut env, entry);
        let read = match self.source.read_lines(ctx.org(), &ctx.key(), &entry.path, range).await {
            Ok(read) => read,
            Err(SourceReadError::OutOfRange { total_lines: 0 }) if start == 1 => {
                env.set("total_lines", json!(0));
                env.set("text", json!(""));
                env.note("empty file");
                return Ok(env);
            }
            Err(SourceReadError::OutOfRange { total_lines }) => {
                return Err(ToolError::invalid(format!("start_line {start} is past the end of the file ({total_lines} lines)")));
            }
            Err(e) => return Err(source_error(e)),
        };
        let mut text = String::new();
        let mut last = start - 1;
        for (offset, line) in read.lines.iter().enumerate() {
            let number = read.first_line + u32::try_from(offset).unwrap_or(u32::MAX);
            let formatted = format!("{:>6}\t{}", number, cut_line(line, READ_MAX_LINE_CHARS));
            let added = formatted.len() + usize::from(!text.is_empty());
            if !text.is_empty() && text.len() + added > READ_MAX_TEXT_BYTES {
                break;
            }
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(&formatted);
            last = number;
        }
        let wanted_end = end_line.unwrap_or(read.total_lines).min(read.total_lines);
        if last < wanted_end {
            env.next_cursor = Some(cursors.encode(&(last + 1).to_string()));
            if last < window_end.min(read.total_lines) {
                env.truncated = true;
            }
        }
        env.set("blob", json!(read.blob));
        env.set("total_lines", json!(read.total_lines));
        env.set("start_line", json!(start));
        env.set("end_line", json!(last));
        env.set("text", Value::String(text));
        env.set("citation", ctx.cite(&entry.path, start, last.max(start)));
        Ok(env)
    }

    async fn list_directory(
        &self,
        ctx: &Ctx<'_>,
        dir: Option<RepoPath>,
        cursors: &CursorScope,
        keyset: Option<String>,
    ) -> Result<Envelope, ToolError> {
        let key = StoreKey::parse(keyset.as_deref())?;
        let (entries, next) = store_page(&key, DIR_PAGE, |page| {
            let dir = dir.clone();
            async move { self.knowledge.list_dir(ctx.org(), &ctx.key(), dir.as_ref(), &page).await }
        })
        .await?;
        if entries.is_empty() && next.is_none() && key == StoreKey::default() {
            if let Some(dir) = &dir {
                return Err(ToolError::not_found(format!("no file or directory {dir} at this sha")));
            }
        }
        let mut env = ctx.envelope(Layer::Files);
        env.set("path", json!(dir.as_ref().map_or("", |d| d.as_str())));
        env.set(
            "entries",
            Value::Array(entries.iter().map(|e| json!({ "name": e.name, "kind": e.kind, "size": e.size, "lang": e.lang })).collect()),
        );
        let kept = env.fit(ListField::Extra("entries")).unwrap_or(entries.len());
        env.next_cursor = resume_key(&key, kept, entries.len(), next).map(|k| cursors.encode(&k.to_keyset()));
        Ok(env)
    }
}

/// `next_cursor` of a store-paged item list after the size cap.
pub(crate) fn finish_store_page(env: &mut Envelope, cursors: &CursorScope, key: &StoreKey, fetched: usize, next: Option<String>) {
    let kept = env.fit(ListField::Items).unwrap_or(env.items.len());
    env.next_cursor = resume_key(key, kept, fetched, next).map(|k| cursors.encode(&k.to_keyset()));
}

fn file_fields(env: &mut Envelope, entry: &ManifestEntry) {
    env.set("path", json!(entry.path));
    env.set("blob", json!(entry.blob));
    env.set("lang", json!(entry.lang));
    env.set("class", json!(entry.class));
}

pub(crate) fn source_error(e: SourceReadError) -> ToolError {
    match e {
        SourceReadError::NotFound => ToolError::not_found("path not found at this sha"),
        SourceReadError::NotText => ToolError::new(ToolErrorCode::NotText, "not a text file"),
        SourceReadError::TooLarge => ToolError::new(ToolErrorCode::TooLarge, "file larger than 2 MiB; use search_code"),
        SourceReadError::OutOfRange { total_lines } => ToolError::invalid(format!("line range outside the file ({total_lines} lines)")),
        SourceReadError::Unavailable(m) => ToolError::unavailable(m),
    }
}
