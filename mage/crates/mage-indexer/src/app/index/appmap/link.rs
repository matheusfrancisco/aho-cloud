//! App-map linker (knowledge.md "Layer 4: App map", UC `index.app_map` step 4): turns a version's
//! app facts plus symbol lookups into the `app_nodes` / `app_edges` graph. Canonicalization and
//! segment matching are `mage_core::domain::knowledge`'s, so keys match what the server computes.
//!
//! Route grouping follows the extractor conventions (`adapters::driven::treesitter::app_map`):
//!
//! - `RouteGroup { var, prefix, parent }` binds `var` to `parent`'s prefix + `prefix` in the same
//!   file; the nearest declaration at or before the use wins (else the first after it).
//! - `parent = Some("F()")` binds a router parameter (or a router created inside `F`) to whatever
//!   callers pass to `F`; a pass site `RouteGroup { var: "F()", prefix, parent: Some(arg) }` (any
//!   file) says "`arg` (+ `prefix`) is passed to `F`". Each pass site contributes one prefix, so a
//!   function mounted twice yields two endpoints. At most [`MAX_CROSSINGS`] function boundaries
//!   are followed (knowledge.md says one level; deeper chains such as `main → setup → Register`
//!   are common in Go services, and the cycle guard keeps it bounded).
//!
//! Nodes get ids `1..` in creation order (deterministic for the same facts and store contents).
//! Edges are unique per `(from, to, kind)`, keeping the highest confidence.

use std::collections::{BTreeMap, HashMap, HashSet};

use mage_core::domain::knowledge::{MethodMatch, RouteKey, RoutePattern, RouteSegment, UrlPart, UrlTemplate, route_match, segment_match};
use mage_core::domain::{OrgId, RepoPath, RepoVersionKey};
use mage_core::ports::knowledge::{
    AppEdge, AppEdgeKind, AppFact, AppFactAt, AppGraph, AppNode, AppNodeKind, CallDirection, FrameworkId, HandlerRef, KnowledgeError, KnowledgeStore,
    NameMatch, NameRefQuery, PageReq, Precision, RefKind, SymbolHit, SymbolKind, SymbolQuery, SymbolRef, TableOp,
};

/// Function boundaries followed when resolving route prefixes.
pub const MAX_CROSSINGS: u8 = 3;
/// Prefixes kept per route declaration.
pub const MAX_PREFIXES: usize = 8;
/// Minimum path confidence of a `ClientCall --CallsEndpoint--> Endpoint` edge.
pub const MIN_CALL_CONFIDENCE: f32 = 0.5;
/// Endpoints linked per client call.
pub const MAX_CALL_LINKS: usize = 3;
/// `Component --CallsEndpoint-->`: client calls in functions the component reaches in this many calls.
pub const COMPONENT_CALL_DEPTH: u8 = 2;
/// `Handler --Reads/Writes--> Table`: call depth of the BFS from the handler.
pub const TABLE_CALL_DEPTH: u8 = 3;
/// Re-frame registrations considered per version.
pub const MAX_EVENTS: usize = 2000;

const HANDLER_KINDS: &[SymbolKind] = &[SymbolKind::Function, SymbolKind::Method, SymbolKind::Variable, SymbolKind::Constant];
const COMPONENT_KINDS: &[SymbolKind] = &[SymbolKind::Function, SymbolKind::Class, SymbolKind::Variable, SymbolKind::Constant];
const CALL_EDGE_LIMIT: u32 = 10_000;

/// App-map coverage (knowledge.md "Quality metrics"), computed at the end of `index.app_map`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Coverage {
    pub endpoints: u32,
    pub endpoints_with_handler: u32,
    pub client_calls: u32,
    pub client_calls_linked: u32,
    pub pages: u32,
    pub pages_with_component: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LinkOutcome {
    pub graph: AppGraph,
    pub coverage: Coverage,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LinkOptions {
    /// Re-frame detected: add `Event` nodes from `Registered` symbols and `Dispatches` edges.
    pub reframe: bool,
}

// ---------------------------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------------------------

/// `prefix` followed by `path`, adjacent literals merged; a `/` is inserted between two
/// non-empty literal ends that lack one.
pub fn join(prefix: &UrlTemplate, path: &UrlTemplate) -> UrlTemplate {
    let mut parts: Vec<UrlPart> = Vec::with_capacity(prefix.parts.len() + path.parts.len());
    let ends_with_slash = matches!(prefix.parts.last(), Some(UrlPart::Lit(t)) if t.ends_with('/'));
    let starts_with_slash = matches!(path.parts.first(), Some(UrlPart::Lit(t)) if t.starts_with('/'));
    let needs_slash = !prefix.parts.is_empty() && matches!(path.parts.first(), Some(UrlPart::Lit(_))) && !ends_with_slash && !starts_with_slash;
    let mut push = |part: UrlPart| match (parts.last_mut(), part) {
        (Some(UrlPart::Lit(last)), UrlPart::Lit(text)) => last.push_str(&text),
        (_, UrlPart::Lit(text)) if text.is_empty() => {}
        (_, part) => parts.push(part),
    };
    for part in &prefix.parts {
        push(part.clone());
    }
    if needs_slash {
        push(UrlPart::Lit("/".into()));
    }
    for part in &path.parts {
        push(part.clone());
    }
    UrlTemplate { parts }
}

/// Display form of a template: holes as `{name}` / `{}`.
pub fn render(t: &UrlTemplate) -> String {
    let mut out = String::new();
    for part in &t.parts {
        match part {
            UrlPart::Lit(text) => out.push_str(text),
            UrlPart::Var(Some(name)) => {
                out.push('{');
                out.push_str(name);
                out.push('}');
            }
            UrlPart::Var(None) => out.push_str("{}"),
        }
    }
    out
}

fn display_method(method: Option<&str>) -> String {
    RouteKey::new(method, RoutePattern::default()).method().unwrap_or("ANY").to_owned()
}

#[derive(Debug, Clone, Copy)]
struct GroupFact<'a> {
    var: &'a str,
    prefix: &'a UrlTemplate,
    parent: Option<&'a str>,
    line: u32,
    /// Enclosing symbol of the fact: the function scope it was declared in.
    scope: Option<u32>,
}

impl GroupFact<'_> {
    /// A router created or received in a function (`r := chi.NewRouter()`, `func F(r *gin.RouterGroup)`)
    /// says less than a mount or group of the same variable in the same scope.
    fn is_binding(&self) -> bool {
        self.prefix.parts.is_empty() && self.parent.is_some_and(is_callee)
    }
}

/// Where a router variable is used.
#[derive(Debug, Clone, Copy)]
pub struct GroupUse<'u> {
    pub file: &'u RepoPath,
    pub line: u32,
    /// Enclosing symbol of the use (`AppFactRecord.enclosing`).
    pub scope: Option<u32>,
}

/// `RouteGroup` facts of a version, indexed for prefix resolution.
pub struct RouteGroups<'a> {
    by_file: HashMap<&'a RepoPath, Vec<GroupFact<'a>>>,
    /// Pass sites by the callee's `"F()"` name.
    calls: BTreeMap<&'a str, Vec<(&'a RepoPath, GroupFact<'a>)>>,
}

impl<'a> RouteGroups<'a> {
    pub fn new(facts: impl IntoIterator<Item = &'a AppFactAt>) -> Self {
        let mut by_file: HashMap<&RepoPath, Vec<GroupFact<'_>>> = HashMap::new();
        let mut calls: BTreeMap<&str, Vec<(&RepoPath, GroupFact<'_>)>> = BTreeMap::new();
        for at in facts {
            if let AppFact::RouteGroup { var, prefix, parent } = &at.fact.fact {
                let group = GroupFact { var, prefix, parent: parent.as_deref(), line: at.fact.line, scope: at.fact.enclosing };
                if is_callee(var) {
                    calls.entry(var.as_str()).or_default().push((&at.path, group));
                } else {
                    by_file.entry(&at.path).or_default().push(group);
                }
            }
        }
        RouteGroups { by_file, calls }
    }

    /// Every prefix `var` stands for at `at` (`[""]` for a root router).
    pub fn prefixes(&self, var: &str, at: GroupUse<'_>) -> Vec<UrlTemplate> {
        let mut stack = Vec::new();
        let mut out = self.resolve(var, at, false, 0, &mut stack);
        dedupe(&mut out);
        out
    }

    /// The declaration `var` refers to at `at`. Among declarations in the same scope, a group or
    /// mount beats a plain binding (`r.Mount("/static", files)` after `files := chi.NewRouter()`
    /// applies to routes declared before the mount); otherwise, file-wide, the nearest
    /// declaration at or before the line wins, else the first after it. `before`: only
    /// declarations strictly before the line (the previous binding of a reassigned variable).
    fn find(&self, var: &str, at: GroupUse<'_>, before: bool) -> Option<GroupFact<'a>> {
        let groups = self.by_file.get(at.file)?;
        let nearest = |candidates: Vec<&GroupFact<'a>>| -> Option<GroupFact<'a>> {
            let preceding = candidates.iter().filter(|g| if before { g.line < at.line } else { g.line <= at.line }).max_by_key(|g| g.line);
            let following = || candidates.iter().filter(|g| !before && g.line > at.line).min_by_key(|g| g.line);
            preceding.or_else(following).map(|g| **g)
        };
        let named: Vec<&GroupFact<'a>> = groups.iter().filter(|g| g.var == var).collect();
        if !before && at.scope.is_some() {
            let scoped: Vec<&GroupFact<'a>> = named.iter().copied().filter(|g| g.scope == at.scope).collect();
            let strong: Vec<&GroupFact<'a>> = scoped.iter().copied().filter(|g| !g.is_binding()).collect();
            if let Some(found) = nearest(strong).or_else(|| nearest(scoped)) {
                return Some(found);
            }
        }
        nearest(named)
    }

    fn resolve(&self, var: &str, at: GroupUse<'_>, before: bool, crossings: u8, stack: &mut Vec<(RepoPath, String, u32)>) -> Vec<UrlTemplate> {
        let root = || vec![UrlTemplate::default()];
        let Some(group) = self.find(var, at, before) else { return root() };
        if stack.len() > 32 || stack.iter().any(|(f, v, l)| f == at.file && v == var && *l == group.line) {
            return root();
        }
        stack.push((at.file.clone(), var.to_owned(), group.line));
        let here = GroupUse { file: at.file, line: group.line, scope: group.scope };
        let parents = match group.parent {
            None => root(),
            Some(callee) if is_callee(callee) => {
                let sites = self.calls.get(callee).map(Vec::as_slice).unwrap_or_default();
                if crossings >= MAX_CROSSINGS || sites.is_empty() {
                    root()
                } else {
                    let mut out = Vec::new();
                    for (site_file, site) in sites {
                        let above = match site.parent {
                            Some(arg) => {
                                let site_use = GroupUse { file: site_file, line: site.line, scope: site.scope };
                                self.resolve(arg, site_use, false, crossings + 1, stack)
                            }
                            None => root(),
                        };
                        out.extend(above.iter().map(|p| join(p, site.prefix)));
                    }
                    out
                }
            }
            // `r = r.Group("/v1")`: the parent is the earlier binding of the same name.
            Some(parent) if parent == group.var => self.resolve(parent, here, true, crossings, stack),
            Some(parent) => self.resolve(parent, here, false, crossings, stack),
        };
        stack.pop();
        let mut out: Vec<UrlTemplate> = parents.iter().map(|p| join(p, group.prefix)).collect();
        dedupe(&mut out);
        out
    }
}

fn is_callee(var: &str) -> bool {
    var.len() > 2 && var.ends_with("()")
}

fn dedupe(prefixes: &mut Vec<UrlTemplate>) {
    let mut seen = HashSet::new();
    prefixes.retain(|p| seen.insert(p.clone()));
    prefixes.truncate(MAX_PREFIXES);
}

/// Endpoints a client call links to (link step 4): canonical client URL segment-matched against
/// endpoint keys, confidence = matched literal segments / segments, scaled by the method relation
/// (equal 1.0, unknown 0.9, different 0.5; different methods only when nothing else matches).
/// A URL starting with a hole (base-URL variable) may also match an endpoint's path suffix at
/// 0.9 × confidence. At most [`MAX_CALL_LINKS`], best first; path confidence ≥
/// [`MIN_CALL_CONFIDENCE`]. A URL without a literal segment links nothing.
pub fn match_client(url: &UrlTemplate, method: Option<&str>, endpoints: &[(u32, RouteKey)]) -> Vec<(u32, f32)> {
    let canonical = RoutePattern::from_client_url(url);
    if !canonical.segments().iter().any(|s| matches!(s, RouteSegment::Lit(_))) {
        return Vec::new();
    }
    let request = RouteKey::new(method, canonical.clone());
    let base_var = matches!(url.parts.first(), Some(UrlPart::Var(_)));
    struct Candidate {
        id: u32,
        path: f32,
        literal: u32,
        method: MethodMatch,
    }
    let mut candidates = Vec::new();
    for (id, key) in endpoints {
        let found = match route_match(&request, key) {
            Some(m) => Some((m.path.confidence(), m.path.literal, m.method)),
            None if base_var => suffix(key.path(), canonical.segments().len()).and_then(|tail| {
                let m = segment_match(&canonical, &tail)?;
                let relation = route_match(&request, &RouteKey::new(key.method(), tail))?.method;
                Some((m.confidence() * 0.9, m.literal, relation))
            }),
            None => None,
        };
        if let Some((path, literal, method)) = found.filter(|(path, _, _)| *path >= MIN_CALL_CONFIDENCE) {
            candidates.push(Candidate { id: *id, path, literal, method });
        }
    }
    if candidates.iter().any(|c| c.method != MethodMatch::Different) {
        candidates.retain(|c| c.method != MethodMatch::Different);
    }
    let factor = |m: MethodMatch| match m {
        MethodMatch::Equal => 1.0,
        MethodMatch::Unspecified => 0.9,
        MethodMatch::Different => 0.5,
    };
    let mut scored: Vec<(u32, f32, u32)> = candidates.iter().map(|c| (c.id, c.path * factor(c.method), c.literal)).collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1).then(b.2.cmp(&a.2)).then(a.0.cmp(&b.0)));
    scored.truncate(MAX_CALL_LINKS);
    scored.into_iter().map(|(id, confidence, _)| (id, confidence)).collect()
}

/// The last `n` segments of `path` (when it is strictly longer).
fn suffix(path: &RoutePattern, n: usize) -> Option<RoutePattern> {
    let segments = path.segments();
    if n == 0 || segments.len() <= n {
        return None;
    }
    let mut display = String::new();
    for segment in &segments[segments.len() - n..] {
        display.push('/');
        match segment {
            RouteSegment::Lit(text) => display.push_str(text),
            RouteSegment::Param => display.push_str("{}"),
        }
    }
    display.parse().ok()
}

// ---------------------------------------------------------------------------------------------
// Graph builder
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
struct Builder {
    nodes: Vec<AppNode>,
    edges: BTreeMap<(u32, u32, AppEdgeKind), AppEdge>,
    /// `(kind, key, at)` → id: one node per declaration site.
    by_site: HashMap<(AppNodeKind, String, Option<(RepoPath, u32)>), u32>,
    /// Symbol-bound nodes (Handler, Component, Event) → id.
    by_symbol: HashMap<(AppNodeKind, SymbolRef), u32>,
    tables: HashMap<String, u32>,
}

impl Builder {
    fn add(&mut self, kind: AppNodeKind, key: String, label: String, framework: Option<FrameworkId>, at: Option<(RepoPath, u32)>, symbol: Option<SymbolRef>) -> u32 {
        let site = (kind, key.clone(), at.clone());
        if let Some(id) = self.by_site.get(&site) {
            return *id;
        }
        let id = self.nodes.len() as u32 + 1;
        if let Some(symbol) = &symbol {
            self.by_symbol.entry((kind, symbol.clone())).or_insert(id);
        }
        self.nodes.push(AppNode { id, kind, key, label, framework, at, symbol });
        self.by_site.insert(site, id);
        id
    }

    /// The node of `kind` bound to `hit`'s symbol (created on first use).
    fn symbol_node(&mut self, kind: AppNodeKind, hit: &SymbolHit, key: String, framework: Option<FrameworkId>) -> u32 {
        if let Some(id) = self.by_symbol.get(&(kind, hit.at.clone())) {
            return *id;
        }
        let at = Some((hit.at.path.clone(), hit.def.span.start()));
        self.add(kind, key, hit.def.name.clone(), framework, at, Some(hit.at.clone()))
    }

    fn table(&mut self, name: &str, at: (RepoPath, u32)) -> u32 {
        if let Some(id) = self.tables.get(name) {
            return *id;
        }
        let id = self.add(AppNodeKind::Table, name.to_owned(), name.to_owned(), Some(FrameworkId::SqlStrings), Some(at), None);
        self.tables.insert(name.to_owned(), id);
        id
    }

    fn edge(&mut self, from: u32, to: u32, kind: AppEdgeKind, confidence: f32, evidence: Option<(RepoPath, u32)>) {
        if from == to {
            return;
        }
        self.edges
            .entry((from, to, kind))
            .and_modify(|e| {
                if confidence > e.confidence {
                    e.confidence = confidence;
                }
            })
            .or_insert(AppEdge { from, to, kind, confidence, evidence });
    }

    fn finish(self) -> (AppGraph, Coverage) {
        let sources: HashSet<(u32, AppEdgeKind)> = self.edges.keys().map(|(from, _, kind)| (*from, *kind)).collect();
        let count = |kind| self.nodes.iter().filter(|n| n.kind == kind).count() as u32;
        let with = |kind, edge| self.nodes.iter().filter(|n| n.kind == kind && sources.contains(&(n.id, edge))).count() as u32;
        let coverage = Coverage {
            endpoints: count(AppNodeKind::Endpoint),
            endpoints_with_handler: with(AppNodeKind::Endpoint, AppEdgeKind::HandledBy),
            client_calls: count(AppNodeKind::ClientCall),
            client_calls_linked: with(AppNodeKind::ClientCall, AppEdgeKind::CallsEndpoint),
            pages: count(AppNodeKind::Page),
            pages_with_component: with(AppNodeKind::Page, AppEdgeKind::Renders),
        };
        (AppGraph { nodes: self.nodes, edges: self.edges.into_values().collect() }, coverage)
    }
}

// ---------------------------------------------------------------------------------------------
// Symbol lookups (cached)
// ---------------------------------------------------------------------------------------------

struct Symbols<'s> {
    store: &'s dyn KnowledgeStore,
    org: OrgId,
    key: &'s RepoVersionKey,
    found: HashMap<(String, Option<String>, &'static [SymbolKind]), Vec<SymbolHit>>,
    by_ref: HashMap<SymbolRef, Option<SymbolHit>>,
    in_file: HashMap<RepoPath, Vec<SymbolHit>>,
    callees: HashMap<SymbolRef, Vec<SymbolRef>>,
}

impl<'s> Symbols<'s> {
    fn new(store: &'s dyn KnowledgeStore, org: OrgId, key: &'s RepoVersionKey) -> Self {
        Symbols { store, org, key, found: HashMap::new(), by_ref: HashMap::new(), in_file: HashMap::new(), callees: HashMap::new() }
    }

    async fn find(&mut self, name: &str, qualifier: Option<&str>, kinds: &'static [SymbolKind]) -> Result<Vec<SymbolHit>, KnowledgeError> {
        let cache_key = (name.to_owned(), qualifier.map(str::to_owned), kinds);
        if let Some(hits) = self.found.get(&cache_key) {
            return Ok(hits.clone());
        }
        let query = SymbolQuery {
            name: name.to_owned(),
            qualifier: qualifier.map(str::to_owned),
            mode: NameMatch::Exact,
            kinds: kinds.to_vec(),
            lang: None,
            path_prefix: None,
            include_tests: false,
        };
        let hits = self.store.find_symbols(self.org, self.key, &query, &PageReq::first(50)).await?.items;
        self.found.insert(cache_key, hits.clone());
        Ok(hits)
    }

    async fn get(&mut self, at: &SymbolRef) -> Result<Option<SymbolHit>, KnowledgeError> {
        if let Some(hit) = self.by_ref.get(at) {
            return Ok(hit.clone());
        }
        let hit = self.store.symbols_by_ref(self.org, self.key, std::slice::from_ref(at)).await?.into_iter().next();
        self.by_ref.insert(at.clone(), hit.clone());
        Ok(hit)
    }

    async fn file(&mut self, path: &RepoPath) -> Result<Vec<SymbolHit>, KnowledgeError> {
        if let Some(hits) = self.in_file.get(path) {
            return Ok(hits.clone());
        }
        let hits = self.store.symbols_in_file(self.org, self.key, path).await?;
        self.in_file.insert(path.clone(), hits.clone());
        Ok(hits)
    }

    /// Symbols reachable from `seed` within `depth` calls (Name precision), with their depth.
    async fn reachable(&mut self, seed: &SymbolRef, depth: u8) -> Result<HashMap<SymbolRef, u8>, KnowledgeError> {
        let mut seen = HashMap::from([(seed.clone(), 0u8)]);
        let mut frontier = vec![seed.clone()];
        for level in 1..=depth {
            let uncached: Vec<SymbolRef> = frontier.iter().filter(|s| !self.callees.contains_key(*s)).cloned().collect();
            if !uncached.is_empty() {
                let edges = self.store.call_edges(self.org, self.key, &uncached, CallDirection::Callees, &Precision::Name, CALL_EDGE_LIMIT).await?;
                for from in &uncached {
                    self.callees.entry(from.clone()).or_default();
                }
                for edge in edges {
                    self.callees.entry(edge.from).or_default().push(edge.to);
                }
            }
            let mut next = Vec::new();
            for from in &frontier {
                for to in self.callees.get(from).map(Vec::as_slice).unwrap_or_default() {
                    if !seen.contains_key(to) {
                        seen.insert(to.clone(), level);
                        next.push(to.clone());
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }
        Ok(seen)
    }
}

/// Best definition for a name used at `site` (link step 3): same file first, then (for an
/// unqualified name) the same directory / package, else the store's ranking (qualifier match,
/// exported, non-test). Confidence 1.0 for a unique candidate, else 0.7.
fn pick(hits: Vec<SymbolHit>, site: &RepoPath, qualified: bool) -> Option<(SymbolHit, f32)> {
    let many = hits.len() > 1;
    let dir = site.parent();
    let rank = |hit: &SymbolHit| {
        let same_file = &hit.at.path == site;
        let same_dir = !qualified && hit.at.path.parent() == dir;
        (same_file, same_dir)
    };
    let best = hits.iter().enumerate().max_by(|(ia, a), (ib, b)| rank(a).cmp(&rank(b)).then(ib.cmp(ia))).map(|(i, _)| i)?;
    let hit = hits.into_iter().nth(best)?;
    Some((hit, if many { 0.7 } else { 1.0 }))
}

fn owner(at: &AppFactAt) -> Option<SymbolRef> {
    at.fact.enclosing.map(|ord| SymbolRef { path: at.path.clone(), blob: at.blob, ord })
}

// ---------------------------------------------------------------------------------------------
// Link
// ---------------------------------------------------------------------------------------------

/// Builds the version's app graph from its facts (any order) and the symbol layer at `key`.
pub async fn link(store: &dyn KnowledgeStore, org: OrgId, key: &RepoVersionKey, facts: &[AppFactAt], opts: LinkOptions) -> Result<LinkOutcome, KnowledgeError> {
    let mut facts: Vec<&AppFactAt> = facts.iter().collect();
    facts.sort_by(|a, b| a.path.cmp(&b.path).then(a.fact.ord.cmp(&b.fact.ord)));
    let groups = RouteGroups::new(facts.iter().copied());
    let mut symbols = Symbols::new(store, org, key);
    let mut g = Builder::default();

    // Steps 1-3: endpoints with resolved prefixes, and their handlers.
    let mut endpoints: Vec<(u32, RouteKey)> = Vec::new();
    for at in &facts {
        let AppFact::RouteDecl { method, path, group, handler, .. } = &at.fact.fact else { continue };
        let site = (at.path.clone(), at.fact.line);
        let prefixes = match group {
            Some(var) => groups.prefixes(var, GroupUse { file: &at.path, line: at.fact.line, scope: at.fact.enclosing }),
            None => vec![UrlTemplate::default()],
        };
        let resolved = match handler {
            HandlerRef::Named { name, qualifier } => {
                let hits = symbols.find(name, qualifier.as_deref(), HANDLER_KINDS).await?;
                pick(hits, &at.path, qualifier.is_some())
            }
            HandlerRef::Inline { enclosing: Some(ord), .. } => {
                let at_ref = SymbolRef { path: at.path.clone(), blob: at.blob, ord: *ord };
                symbols.get(&at_ref).await?.map(|hit| (hit, 0.9))
            }
            HandlerRef::Inline { enclosing: None, .. } | HandlerRef::Param { .. } => None,
        };
        for prefix in prefixes {
            let full = join(&prefix, path);
            let route = RouteKey::new(method.as_deref(), RoutePattern::from_route(&full));
            let label = format!("{} {}", display_method(method.as_deref()), render(&full));
            let before = g.nodes.len();
            let symbol = resolved.as_ref().map(|(hit, _)| hit.at.clone());
            let id = g.add(AppNodeKind::Endpoint, route.to_string(), label, Some(at.fact.framework), Some(site.clone()), symbol);
            if g.nodes.len() > before {
                endpoints.push((id, route));
            }
            if let Some((hit, confidence)) = &resolved {
                let handler = g.symbol_node(AppNodeKind::Handler, hit, hit.def.qualified.clone(), Some(at.fact.framework));
                g.edge(id, handler, AppEdgeKind::HandledBy, *confidence, Some(site.clone()));
            }
        }
    }

    // Step 4: client calls.
    struct CallLinks {
        owner: Option<SymbolRef>,
        links: Vec<(u32, f32)>,
        site: (RepoPath, u32),
    }
    let mut calls: Vec<CallLinks> = Vec::new();
    for at in &facts {
        let AppFact::HttpCall { method, url, .. } = &at.fact.fact else { continue };
        let site = (at.path.clone(), at.fact.line);
        let route = RouteKey::new(method.as_deref(), RoutePattern::from_client_url(url));
        let label = format!("{} {}", display_method(method.as_deref()), render(url));
        let call = g.add(AppNodeKind::ClientCall, route.to_string(), label, Some(at.fact.framework), Some(site.clone()), owner(at));
        let links = match_client(url, method.as_deref(), &endpoints);
        for (endpoint, confidence) in &links {
            g.edge(call, *endpoint, AppEdgeKind::CallsEndpoint, *confidence, Some(site.clone()));
        }
        calls.push(CallLinks { owner: owner(at), links, site });
    }

    // Step 5: pages and the components they render.
    let mut components: Vec<(u32, SymbolRef)> = Vec::new();
    for at in &facts {
        let AppFact::PageDecl { path, component } = &at.fact.fact else { continue };
        let site = (at.path.clone(), at.fact.line);
        let page = g.add(AppNodeKind::Page, RoutePattern::from_route(path).to_string(), render(path), Some(at.fact.framework), Some(site.clone()), None);
        let resolved = if component == "default" {
            symbols
                .file(&at.path)
                .await?
                .into_iter()
                .find(|h| h.def.exported && COMPONENT_KINDS.contains(&h.def.kind) && h.def.container.is_none())
                .map(|hit| (hit, 0.8))
        } else {
            let hits = symbols.find(component, None, COMPONENT_KINDS).await?;
            pick(hits, &at.path, false)
        };
        match resolved {
            Some((hit, confidence)) => {
                let node = g.symbol_node(AppNodeKind::Component, &hit, hit.def.qualified.clone(), Some(at.fact.framework));
                g.edge(page, node, AppEdgeKind::Renders, confidence, Some(site));
                components.push((node, hit.at));
            }
            None => {
                let node = g.add(AppNodeKind::Component, component.clone(), component.clone(), Some(at.fact.framework), None, None);
                g.edge(page, node, AppEdgeKind::Renders, 0.5, Some(site));
            }
        }
    }

    // Step 6: re-frame events and dispatches.
    let mut events: Vec<(u32, SymbolRef)> = Vec::new();
    if opts.reframe {
        let mut registered: Vec<SymbolHit> = Vec::new();
        let mut after = None;
        loop {
            let query = SymbolQuery {
                name: String::new(),
                qualifier: None,
                mode: NameMatch::Prefix,
                kinds: vec![SymbolKind::Registered],
                lang: None,
                path_prefix: None,
                include_tests: false,
            };
            let page = store.find_symbols(org, key, &query, &PageReq { limit: 200, after }).await?;
            registered.extend(page.items);
            if registered.len() >= MAX_EVENTS {
                registered.truncate(MAX_EVENTS);
                break;
            }
            match page.next {
                Some(next) => after = Some(next),
                None => break,
            }
        }
        let mut event_ids: HashMap<SymbolRef, u32> = HashMap::new();
        for hit in &registered {
            let event = g.symbol_node(AppNodeKind::Event, hit, hit.def.name.clone(), Some(FrameworkId::ReFrame));
            event_ids.insert(hit.at.clone(), event);
            events.push((event, hit.at.clone()));
        }
        for (hit, event) in registered.iter().zip(events.iter().map(|(id, _)| *id)) {
            let query = NameRefQuery { name: hit.def.name.clone(), qualifier: None, kinds: vec![RefKind::Dispatch], include_tests: false };
            let refs = store.name_refs(org, key, &query, &PageReq::first(200)).await?.items;
            for r in refs {
                let Some(enclosing) = r.enclosing else { continue };
                let from = if let Some(id) = event_ids.get(&enclosing) {
                    *id
                } else {
                    let Some(def) = symbols.get(&enclosing).await? else { continue };
                    let node = g.symbol_node(AppNodeKind::Component, &def, def.def.qualified.clone(), Some(FrameworkId::ReFrame));
                    if !components.iter().any(|(id, _)| *id == node) {
                        components.push((node, def.at.clone()));
                    }
                    node
                };
                g.edge(from, event, AppEdgeKind::Dispatches, 1.0, Some((r.path.clone(), r.pos.0)));
            }
        }
    }

    // Components and event handlers call the endpoints of client calls they contain or reach.
    for (component, symbol) in &components {
        let reach = symbols.reachable(symbol, COMPONENT_CALL_DEPTH).await?;
        for call in &calls {
            if call.owner.as_ref().is_some_and(|o| reach.contains_key(o)) {
                for (endpoint, confidence) in &call.links {
                    g.edge(*component, *endpoint, AppEdgeKind::CallsEndpoint, *confidence, Some(call.site.clone()));
                }
            }
        }
    }
    for (event, symbol) in &events {
        for call in calls.iter().filter(|c| c.owner.as_ref() == Some(symbol)) {
            for (endpoint, confidence) in &call.links {
                g.edge(*event, *endpoint, AppEdgeKind::CallsEndpoint, *confidence, Some(call.site.clone()));
            }
        }
    }

    // Step 7: tables read or written by handlers (BFS over name-precision call edges).
    let mut table_refs: HashMap<SymbolRef, Vec<(u32, TableOp, (RepoPath, u32))>> = HashMap::new();
    for at in &facts {
        let AppFact::TableRef { table, op } = &at.fact.fact else { continue };
        let site = (at.path.clone(), at.fact.line);
        let node = g.table(table, site.clone());
        if let Some(o) = owner(at) {
            table_refs.entry(o).or_default().push((node, *op, site));
        }
    }
    let handlers: Vec<(u32, SymbolRef)> =
        g.nodes.iter().filter(|n| n.kind == AppNodeKind::Handler).filter_map(|n| n.symbol.clone().map(|s| (n.id, s))).collect();
    if !table_refs.is_empty() {
        for (handler, symbol) in handlers {
            let reach = symbols.reachable(&symbol, TABLE_CALL_DEPTH).await?;
            let mut reached: Vec<(&SymbolRef, &u8)> = reach.iter().collect();
            reached.sort();
            for (at, depth) in reached {
                for (table, op, site) in table_refs.get(at).map(Vec::as_slice).unwrap_or_default() {
                    let kind = match op {
                        TableOp::Insert | TableOp::Update | TableOp::Delete => AppEdgeKind::Writes,
                        TableOp::Select | TableOp::Unknown => AppEdgeKind::Reads,
                    };
                    let confidence = match (depth, op) {
                        (_, TableOp::Unknown) => 0.5,
                        (0, _) => 1.0,
                        (d, _) => (1.0 - 0.2 * f32::from(*d)).max(0.5),
                    };
                    g.edge(handler, *table, kind, confidence, Some(site.clone()));
                }
            }
        }
    }

    let (graph, coverage) = g.finish();
    Ok(LinkOutcome { graph, coverage })
}

#[cfg(test)]
mod tests;
