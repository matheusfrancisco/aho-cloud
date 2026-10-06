//! `MemKnowledgeStore`: in-memory `KnowledgeStore` with the semantics of `ports::knowledge`.
//! Blob-keyed facts are stored once per `(org, oid, rev)`; a version's view joins its manifest to
//! them at the revs its [`MemRepoStore`] holds, like the Postgres join on `repo_versions.revs`.
//! Every call holds one lock, so each is atomic like the transaction it mirrors.

use std::cmp::Reverse;
use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use regex::RegexBuilder;
use time::OffsetDateTime;

use crate::domain::fix::Glob;
use crate::domain::ids::{ArtifactId, BlobOid, CommitSha, OrgId, RepoId, RepoVersionKey};
use crate::domain::path::{FileClass, Language, RepoPath};
use crate::domain::repo::IndexRevs;
use crate::ports::clock::Clock;
use crate::ports::knowledge::{
    AppFactAt, AppFactRecord, AppGraph, AppMapRev, AppNode, AppNodeKind, BlobMeta, BlobStage, CallDirection, CallEdge, CatalogEntryRecord,
    CatalogMatch, CommitRecord, DirEntry, DirEntryKind, EntryKind, ErrorMatch, ErrorQuery, ErrorSiteKind, ErrorSiteRecord, ErrorsRev,
    FALLBACK_OVERLAP, FUZZY_THRESHOLD, FactCounts, GlossaryHit, GlossarySource, HistoryCursor, KnowledgeError, KnowledgeStore,
    LOW_SPECIFICITY, LinkLookup, LiteralSite, ManifestChange, ManifestEntry, NameMatch, NameRef, NameRefQuery, PageReq, Paged, ParsedBlob,
    ParserRev, PathMatch, Precision, PullRequestRecord, RefHit, RefKind, ScipDocRows, ScipIndexInfo, ScipMetadata, ScipOccurrence,
    ScipRange, ScipRoles, ScipSymbolHit, ScipSymbolInfo, StageStatus, SymbolDef, SymbolHit, SymbolKind, SymbolQuery, SymbolRef, TicketLink,
    VersionStats, trigram_similarity,
};
use crate::testing::MemRepoStore;
use crate::testing::page::keyset_page;
use crate::wire::indexer::{ChangeKind, CommitInfo, FileChange};

pub struct MemKnowledgeStore {
    clock: Arc<dyn Clock>,
    repos: Arc<MemRepoStore>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// `blobs`, with the insertion time the sweep's grace is measured from.
    blobs: BTreeMap<(OrgId, BlobOid), BlobRow>,
    /// `blob_stages`.
    stages: HashMap<(OrgId, BlobOid, BlobStage), Marker>,
    /// `symbols`, `name_refs`, `literal_sites`.
    parsed: BTreeMap<(OrgId, BlobOid, ParserRev), ParsedRows>,
    error_sites: BTreeMap<(OrgId, BlobOid, ErrorsRev), BTreeMap<u32, ErrorSiteRecord>>,
    /// `catalog_entries`, keyed by catalog key.
    catalog: BTreeMap<(OrgId, BlobOid, ErrorsRev), BTreeMap<String, CatalogEntryRecord>>,
    app_facts: BTreeMap<(OrgId, BlobOid, AppMapRev), BTreeMap<u32, AppFactRecord>>,
    manifests: BTreeMap<(OrgId, RepoVersionKey), BTreeMap<RepoPath, ManifestEntry>>,
    /// Nodes sorted by id.
    graphs: BTreeMap<(OrgId, RepoVersionKey), AppGraph>,
    scip: BTreeMap<(OrgId, ArtifactId), ScipIndex>,
    commits: BTreeMap<(OrgId, RepoId, CommitSha), StoredCommit>,
    prs: BTreeMap<(OrgId, RepoId, u32), PullRequestRecord>,
    links: BTreeSet<(OrgId, RepoId, TicketLink)>,
    cursors: BTreeMap<(OrgId, RepoId), HistoryCursor>,
}

struct BlobRow {
    meta: BlobMeta,
    created_at: OffsetDateTime,
}

struct Marker {
    lang: Option<Language>,
    status: StageStatus,
}

#[derive(Default)]
struct ParsedRows {
    symbols: BTreeMap<u32, SymbolDef>,
    refs: BTreeMap<u32, NameRef>,
    literals: BTreeMap<u32, LiteralSite>,
}

/// `scip_indexes` row with its `scip_occurrences` (per path) and `scip_symbols`.
struct ScipIndex {
    repo: RepoId,
    sha: CommitSha,
    tool: String,
    created_at: OffsetDateTime,
    ready: bool,
    languages: Vec<String>,
    documents: u32,
    docs: BTreeMap<RepoPath, ScipDoc>,
    symbols: BTreeMap<String, ScipSymbolInfo>,
}

struct ScipDoc {
    blob: BlobOid,
    /// By ord (position in the document).
    occurrences: BTreeMap<u32, ScipOccurrence>,
}

/// `commits` row (`info.files` always `None`) and its `commit_files` rows by path.
struct StoredCommit {
    info: CommitInfo,
    files: BTreeMap<RepoPath, FileChange>,
}

impl StoredCommit {
    fn record(&self) -> CommitRecord {
        let files = (!self.files.is_empty()).then(|| self.files.values().cloned().collect());
        CommitRecord { info: CommitInfo { files, ..self.info.clone() } }
    }
}

impl State {
    /// Newest ready index of `repo` at `sha`.
    fn ready_index_at(&self, org: OrgId, repo: RepoId, sha: CommitSha) -> Option<(ArtifactId, &ScipIndex)> {
        self.ready_indexes(org, repo).filter(|(_, index)| index.sha == sha).max_by_key(|(artifact, index)| (index.created_at, *artifact))
    }

    fn ready_indexes(&self, org: OrgId, repo: RepoId) -> impl Iterator<Item = (ArtifactId, &ScipIndex)> {
        self.scip
            .iter()
            .filter(move |((o, _), index)| *o == org && index.ready && index.repo == repo)
            .map(|((_, artifact), index)| (*artifact, index))
    }

    fn ready_index(&self, org: OrgId, artifact: ArtifactId) -> Option<&ScipIndex> {
        self.scip.get(&(org, artifact)).filter(|index| index.ready)
    }

    fn ingesting_index(&mut self, org: OrgId, artifact: ArtifactId) -> Result<&mut ScipIndex, KnowledgeError> {
        self.scip.get_mut(&(org, artifact)).ok_or(KnowledgeError::NotFound)
    }
}

/// A version's view: its manifest joined to blob-keyed facts at its revs.
struct View<'a> {
    state: &'a State,
    org: OrgId,
    manifest: Option<&'a BTreeMap<RepoPath, ManifestEntry>>,
    graph: Option<&'a AppGraph>,
    revs: Option<IndexRevs>,
}

impl<'a> View<'a> {
    fn new(state: &'a State, org: OrgId, key: &RepoVersionKey, revs: Option<IndexRevs>) -> Self {
        Self { state, org, manifest: state.manifests.get(&(org, *key)), graph: state.graphs.get(&(org, *key)), revs }
    }

    fn entries(&self) -> impl Iterator<Item = &'a ManifestEntry> + use<'a> {
        self.manifest.into_iter().flat_map(BTreeMap::values)
    }

    fn entry(&self, path: &RepoPath) -> Option<&'a ManifestEntry> {
        self.manifest?.get(path)
    }

    fn nodes(&self) -> impl Iterator<Item = &'a AppNode> + use<'a> {
        self.graph.into_iter().flat_map(|graph| graph.nodes.iter())
    }

    /// The entry with `is_text` / `line_count` from its blob's meta.
    fn entry_out(&self, entry: &ManifestEntry) -> ManifestEntry {
        let meta = self.state.blobs.get(&(self.org, entry.blob)).map(|row| &row.meta);
        ManifestEntry { is_text: meta.is_some_and(|m| m.is_text), line_count: meta.map_or(0, |m| m.line_count), ..entry.clone() }
    }

    fn line_count(&self, blob: BlobOid) -> u32 {
        self.state.blobs.get(&(self.org, blob)).map_or(0, |row| row.meta.line_count)
    }

    /// Parser rows of `blob` at the version's parser rev.
    fn parsed(&self, blob: BlobOid) -> Option<&'a ParsedRows> {
        let rev = self.revs?.parser;
        self.state.parsed.get(&(self.org, blob, rev))
    }

    fn symbols(&self, blob: BlobOid) -> impl Iterator<Item = &'a SymbolDef> + use<'a> {
        self.parsed(blob).into_iter().flat_map(|rows| rows.symbols.values())
    }

    fn refs(&self, blob: BlobOid) -> impl Iterator<Item = &'a NameRef> + use<'a> {
        self.parsed(blob).into_iter().flat_map(|rows| rows.refs.values())
    }

    /// The symbol `r` names, when its path holds its blob at the version.
    fn resolve(&self, r: &SymbolRef) -> Option<&'a SymbolDef> {
        if self.entry(&r.path)?.blob != r.blob {
            return None;
        }
        self.parsed(r.blob)?.symbols.get(&r.ord)
    }

    /// Innermost symbol of `blob` whose span contains `line`.
    fn innermost(&self, blob: BlobOid, line: u32) -> Option<&'a SymbolDef> {
        self.symbols(blob).filter(|s| s.span.contains(line)).min_by_key(|s| (s.span.len(), Reverse(s.ord)))
    }

    fn error_sites(&self, blob: BlobOid, rev: ErrorsRev) -> impl Iterator<Item = &'a ErrorSiteRecord> + use<'a> {
        self.state.error_sites.get(&(self.org, blob, rev)).into_iter().flat_map(BTreeMap::values)
    }

    fn catalog(&self, blob: BlobOid, rev: ErrorsRev) -> impl Iterator<Item = &'a CatalogEntryRecord> + use<'a> {
        self.state.catalog.get(&(self.org, blob, rev)).into_iter().flat_map(BTreeMap::values)
    }

    /// A SCIP document is usable at the version when its blob is the manifest blob of its path.
    fn usable(&self, path: &RepoPath, doc: &ScipDoc) -> bool {
        self.entry(path).is_some_and(|entry| entry.blob == doc.blob)
    }
}

/// Parsed `PageReq`: the cursor is the position, in the method's order, of the last item served.
struct PageAt {
    after: Option<usize>,
    limit: usize,
}

impl PageAt {
    fn parse(page: &PageReq) -> Result<Self, KnowledgeError> {
        if page.limit == 0 {
            return Err(KnowledgeError::Invalid("page limit must be at least 1".into()));
        }
        let after = match &page.after {
            None => None,
            Some(cursor) => Some(cursor.parse::<usize>().map_err(|_| KnowledgeError::Invalid(format!("bad cursor {cursor:?}")))?),
        };
        Ok(Self { after, limit: to_usize(page.limit) })
    }

    /// `items` in the method's order.
    fn apply<T>(&self, items: Vec<T>) -> Paged<T> {
        let (items, next) = keyset_page(items.into_iter().enumerate().collect(), self.after.as_ref(), self.limit, usize::to_string);
        Paged { items, next: next.map(|cursor| cursor.0) }
    }
}

fn to_usize(n: u32) -> usize {
    usize::try_from(n).unwrap_or(usize::MAX)
}

fn to_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn dedup_in_order<T: Ord + Clone>(items: &[T]) -> Vec<T> {
    let mut seen = BTreeSet::new();
    items.iter().filter(|item| seen.insert((*item).clone())).cloned().collect()
}

fn hit(at: SymbolRef, def: &SymbolDef, score: f32) -> SymbolHit {
    SymbolHit { at, def: def.clone(), score }
}

/// `SymbolQuery.qualifier` rule: a segment of `qualified` (split on `.`, `/`, `::`), or the name of
/// the hit's directory.
fn qualifier_matches(qualifier: &str, qualified: &str, path: &RepoPath) -> bool {
    qualified.replace("::", ".").split(['.', '/']).any(|segment| segment == qualifier)
        || path.parent().is_some_and(|dir| dir.rsplit('/').next() == Some(qualifier))
}

fn name_matches(mode: NameMatch, name: &str, query: &str) -> bool {
    match mode {
        NameMatch::Exact => name == query,
        NameMatch::CaseInsensitive => name.to_lowercase() == query.to_lowercase(),
        NameMatch::Prefix => name.to_lowercase().starts_with(&query.to_lowercase()),
        NameMatch::Fuzzy => trigram_similarity(name, query) >= FUZZY_THRESHOLD,
    }
}

fn is_call(kind: RefKind) -> bool {
    matches!(kind, RefKind::Call | RefKind::Dispatch)
}

/// SCIP's document-local symbols (`local <n>`) never name another symbol of the version.
fn is_local(symbol: &str) -> bool {
    symbol.starts_with("local ")
}

/// Nested ranges sort innermost first.
fn range_size(range: &ScipRange) -> (u32, i64) {
    (range.end_line.saturating_sub(range.start_line), i64::from(range.end_col) - i64::from(range.start_col))
}

/// Symbol defined at `name_pos` (1-based line and column) in `doc`: the definition occurrence on
/// that line covering the column, else the first non-local definition on that line.
fn definition_at(doc: &ScipDoc, (line, col): (u32, u32)) -> Option<&str> {
    let defs = || doc.occurrences.values().filter(move |occ| occ.roles.is_definition() && occ.range.start_line == line);
    defs()
        .find(|occ| occ.range.covers(line, Some(col.saturating_sub(1))))
        .or_else(|| defs().find(|occ| !is_local(&occ.symbol)))
        .map(|occ| occ.symbol.as_str())
}

/// First definition occurrence of `symbol` in a usable document, by (path, ord).
fn usable_definition<'a>(view: &View<'_>, index: &'a ScipIndex, symbol: &str) -> Option<(&'a RepoPath, &'a ScipDoc, &'a ScipOccurrence)> {
    index.docs.iter().filter(|(path, doc)| view.usable(path, doc)).find_map(|(path, doc)| {
        doc.occurrences.values().find(|occ| occ.symbol == symbol && occ.roles.is_definition()).map(|occ| (path, doc, occ))
    })
}

type EdgeMap = BTreeMap<(SymbolRef, SymbolRef), Vec<(RepoPath, u32)>>;

fn name_edges(view: &View<'_>, targets: &[(&SymbolRef, &SymbolDef)], dir: CallDirection, edges: &mut EdgeMap) {
    match dir {
        CallDirection::Callers => {
            for (target, def) in targets {
                for entry in view.entries() {
                    for r in view.refs(entry.blob).filter(|r| is_call(r.kind) && r.name == def.name) {
                        let Some(enclosing) = r.enclosing else { continue };
                        let from = SymbolRef { path: entry.path.clone(), blob: entry.blob, ord: enclosing };
                        edges.entry((from, (*target).clone())).or_default().push((entry.path.clone(), r.pos.0));
                    }
                }
            }
        }
        CallDirection::Callees => {
            for (source, _) in targets {
                for r in view.refs(source.blob).filter(|r| is_call(r.kind) && r.enclosing == Some(source.ord)) {
                    for entry in view.entries() {
                        for callee in view.symbols(entry.blob).filter(|s| s.name == r.name) {
                            let to = SymbolRef { path: entry.path.clone(), blob: entry.blob, ord: callee.ord };
                            edges.entry(((*source).clone(), to)).or_default().push((source.path.clone(), r.pos.0));
                        }
                    }
                }
            }
        }
    }
}

fn scip_edges(view: &View<'_>, index: &ScipIndex, targets: &[(&SymbolRef, &SymbolDef)], dir: CallDirection, edges: &mut EdgeMap) {
    for (target, def) in targets {
        let Some(doc) = index.docs.get(&target.path).filter(|doc| view.usable(&target.path, doc)) else { continue };
        match dir {
            CallDirection::Callers => {
                let Some(symbol) = definition_at(doc, def.name_pos) else { continue };
                for (path, other) in index.docs.iter().filter(|(path, doc)| view.usable(path, doc)) {
                    for occ in other.occurrences.values().filter(|occ| occ.symbol == symbol && !occ.roles.is_definition()) {
                        let line = occ.range.start_line;
                        let Some(caller) = view.innermost(other.blob, line) else { continue };
                        let from = SymbolRef { path: path.clone(), blob: other.blob, ord: caller.ord };
                        edges.entry((from, (*target).clone())).or_default().push((path.clone(), line));
                    }
                }
            }
            CallDirection::Callees => {
                let calls = doc
                    .occurrences
                    .values()
                    .filter(|occ| !occ.roles.is_definition() && !is_local(&occ.symbol) && def.span.contains(occ.range.start_line));
                for occ in calls {
                    let Some((def_path, def_doc, def_occ)) = usable_definition(view, index, &occ.symbol) else { continue };
                    let Some(callee) = view.innermost(def_doc.blob, def_occ.range.start_line) else { continue };
                    let to = SymbolRef { path: def_path.clone(), blob: def_doc.blob, ord: callee.ord };
                    edges.entry(((*target).clone(), to)).or_default().push((target.path.clone(), occ.range.start_line));
                }
            }
        }
    }
}

/// A stored pattern row (`error_sites`, `catalog_entries`).
trait PatternRow {
    fn tokens(&self) -> &[String];
    fn specificity(&self) -> u32;
    fn template(&self) -> &str;
    fn pattern(&self) -> &str;
}

impl PatternRow for ErrorSiteRecord {
    fn tokens(&self) -> &[String] {
        &self.tokens
    }
    fn specificity(&self) -> u32 {
        self.specificity
    }
    fn template(&self) -> &str {
        &self.template
    }
    fn pattern(&self) -> &str {
        &self.pattern
    }
}

impl PatternRow for CatalogEntryRecord {
    fn tokens(&self) -> &[String] {
        &self.tokens
    }
    fn specificity(&self) -> u32 {
        self.specificity
    }
    fn template(&self) -> &str {
        &self.template
    }
    fn pattern(&self) -> &str {
        &self.pattern
    }
}

/// A match of `rows[row]`.
struct PatternHit {
    row: usize,
    segment: u16,
    span: (u32, u32),
    verified: bool,
    score: f32,
}

/// knowledge.md "Matching" steps 2-5 over `rows`, given in the method's (path, ord | key) order.
fn match_rows<R: PatternRow>(rows: &[&R], q: &ErrorQuery) -> Vec<PatternHit> {
    let input: BTreeSet<&str> = q.tokens.iter().map(String::as_str).collect();
    let limit = to_usize(q.limit);
    let specific_enough = |row: &R| {
        let low = row.specificity() < LOW_SPECIFICITY || row.tokens().len() < 2;
        !low || q.include_low_specificity || q.segments.iter().any(|segment| segment.trim() == row.template())
    };
    let mut hits = Vec::new();
    let mut verified = BTreeSet::new();
    let candidates = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| row.tokens().iter().all(|token| input.contains(token.as_str())))
        .take(to_usize(q.max_candidates));
    for (i, row) in candidates {
        if !specific_enough(row) {
            continue;
        }
        if let Some(hit) = verify(i, *row, &q.segments) {
            verified.insert(i);
            hits.push(hit);
        }
    }
    if verified.len() < limit {
        for (i, row) in rows.iter().enumerate() {
            let tokens = row.tokens();
            if verified.contains(&i) || tokens.is_empty() || !specific_enough(row) {
                continue;
            }
            let common = tokens.iter().filter(|token| input.contains(token.as_str())).count();
            let overlap = common as f32 / tokens.len() as f32;
            if overlap >= FALLBACK_OVERLAP {
                hits.push(PatternHit { row: i, segment: 0, span: (0, 0), verified: false, score: 0.4 * overlap });
            }
        }
    }
    hits.sort_by(|a, b| b.verified.cmp(&a.verified).then(b.score.total_cmp(&a.score)).then(a.row.cmp(&b.row)));
    hits.truncate(limit);
    hits
}

/// The row's pattern on the first segment it matches (an uncompilable pattern never matches).
fn verify<R: PatternRow>(row_index: usize, row: &R, segments: &[String]) -> Option<PatternHit> {
    let regex = RegexBuilder::new(row.pattern()).size_limit(1 << 20).build().ok()?;
    segments.iter().enumerate().find_map(|(i, segment)| {
        let found = regex.find(segment)?;
        let ratio = (row.specificity() as f32 / segment.len() as f32).min(1.0);
        Some(PatternHit {
            row: row_index,
            segment: u16::try_from(i).unwrap_or(u16::MAX),
            span: (to_u32(found.start()), to_u32(found.end())),
            verified: true,
            score: 0.5 + 0.5 * ratio,
        })
    })
}

/// `glossary_top` groups: `(source, text)` → (count, first location).
type TopGroups = BTreeMap<(GlossarySource, String), (u32, (RepoPath, u32))>;

fn add_top(groups: &mut TopGroups, source: GlossarySource, text: String, at: (RepoPath, u32)) {
    match groups.entry((source, text)) {
        Entry::Occupied(mut group) => {
            let (count, first) = group.get_mut();
            *count += 1;
            if at < *first {
                *first = at;
            }
        }
        Entry::Vacant(group) => {
            group.insert((1, at));
        }
    }
}

/// Last literal path segment of an endpoint key (`"GET /api/connections/{}"` → `connections`).
fn endpoint_resource(key: &str) -> Option<&str> {
    let path = key.split_once(' ').map_or(key, |(_, path)| path);
    path.split('/').rfind(|segment| !segment.is_empty() && *segment != "{}")
}

fn glossary_node_source(kind: AppNodeKind) -> Option<GlossarySource> {
    match kind {
        AppNodeKind::Table => Some(GlossarySource::Table),
        AppNodeKind::Endpoint => Some(GlossarySource::Endpoint),
        AppNodeKind::Page => Some(GlossarySource::Page),
        AppNodeKind::Event => Some(GlossarySource::Event),
        _ => None,
    }
}

impl MemKnowledgeStore {
    pub fn new(clock: Arc<dyn Clock>, repos: Arc<MemRepoStore>) -> Self {
        Self { clock, repos, state: Mutex::new(State::default()) }
    }

    /// Read before taking the store lock (the repo store has its own).
    fn revs(&self, org: OrgId, key: &RepoVersionKey) -> Option<IndexRevs> {
        self.repos.index_revs(org, *key)
    }
}

#[async_trait]
impl KnowledgeStore for MemKnowledgeStore {
    async fn put_manifest(&self, org: OrgId, key: &RepoVersionKey, entries: &[ManifestEntry]) -> Result<(), KnowledgeError> {
        let mut manifest = BTreeMap::new();
        for entry in entries {
            if manifest.insert(entry.path.clone(), entry.clone()).is_some() {
                return Err(KnowledgeError::Invalid(format!("duplicate manifest path {}", entry.path)));
            }
        }
        self.state.lock().manifests.insert((org, *key), manifest);
        Ok(())
    }

    async fn put_blob_meta(&self, org: OrgId, metas: &[BlobMeta]) -> Result<(), KnowledgeError> {
        let now = self.clock.now();
        let mut state = self.state.lock();
        for meta in metas {
            state.blobs.entry((org, meta.oid)).or_insert(BlobRow { meta: *meta, created_at: now });
        }
        Ok(())
    }

    async fn blob_meta(&self, org: OrgId, oids: &[BlobOid]) -> Result<Vec<BlobMeta>, KnowledgeError> {
        let state = self.state.lock();
        let oids: BTreeSet<BlobOid> = oids.iter().copied().collect();
        Ok(oids.into_iter().filter_map(|oid| state.blobs.get(&(org, oid)).map(|row| row.meta)).collect())
    }

    async fn missing(&self, org: OrgId, stage: BlobStage, oids: &[BlobOid]) -> Result<Vec<BlobOid>, KnowledgeError> {
        let state = self.state.lock();
        let oids: BTreeSet<BlobOid> = oids.iter().copied().collect();
        Ok(oids.into_iter().filter(|oid| !state.stages.contains_key(&(org, *oid, stage))).collect())
    }

    async fn put_parsed(&self, org: OrgId, rev: ParserRev, blobs: &[(BlobOid, Result<ParsedBlob, String>)]) -> Result<(), KnowledgeError> {
        let mut state = self.state.lock();
        for (oid, parsed) in blobs {
            let marker = match parsed {
                Ok(parsed) => {
                    let rows = state.parsed.entry((org, *oid, rev)).or_default();
                    for symbol in &parsed.symbols {
                        rows.symbols.entry(symbol.ord).or_insert_with(|| symbol.clone());
                    }
                    for r in &parsed.refs {
                        rows.refs.entry(r.ord).or_insert_with(|| r.clone());
                    }
                    for literal in &parsed.literals {
                        rows.literals.entry(literal.ord).or_insert_with(|| literal.clone());
                    }
                    Marker { lang: Some(parsed.lang), status: parsed.status.clone() }
                }
                Err(reason) => Marker { lang: None, status: StageStatus::Failed { reason: reason.clone() } },
            };
            state.stages.entry((org, *oid, BlobStage::Parse(rev))).or_insert(marker);
        }
        Ok(())
    }

    async fn literal_sites(
        &self,
        org: OrgId,
        rev: ParserRev,
        oids: &[BlobOid],
    ) -> Result<Vec<(BlobOid, Language, Vec<LiteralSite>)>, KnowledgeError> {
        let state = self.state.lock();
        let oids: BTreeSet<BlobOid> = oids.iter().copied().collect();
        let mut out = Vec::new();
        for oid in oids {
            let Some(marker) = state.stages.get(&(org, oid, BlobStage::Parse(rev))) else { continue };
            let (Some(lang), false) = (marker.lang, matches!(marker.status, StageStatus::Failed { .. })) else { continue };
            let sites = state.parsed.get(&(org, oid, rev)).map(|rows| rows.literals.values().cloned().collect()).unwrap_or_default();
            out.push((oid, lang, sites));
        }
        Ok(out)
    }

    async fn put_error_sites(&self, org: OrgId, rev: ErrorsRev, blobs: &[(BlobOid, Vec<ErrorSiteRecord>)]) -> Result<(), KnowledgeError> {
        let mut state = self.state.lock();
        for (oid, sites) in blobs {
            let rows = state.error_sites.entry((org, *oid, rev)).or_default();
            for site in sites {
                rows.entry(site.ord).or_insert_with(|| site.clone());
            }
            state.stages.entry((org, *oid, BlobStage::ErrorSites(rev))).or_insert(Marker { lang: None, status: StageStatus::Ok });
        }
        Ok(())
    }

    async fn put_catalog(&self, org: OrgId, rev: ErrorsRev, blobs: &[(BlobOid, Vec<CatalogEntryRecord>)]) -> Result<(), KnowledgeError> {
        let mut state = self.state.lock();
        for (oid, entries) in blobs {
            let rows = state.catalog.entry((org, *oid, rev)).or_default();
            for entry in entries {
                rows.entry(entry.key.clone()).or_insert_with(|| entry.clone());
            }
            state.stages.entry((org, *oid, BlobStage::ErrorSites(rev))).or_insert(Marker { lang: None, status: StageStatus::Ok });
        }
        Ok(())
    }

    async fn put_app_facts(&self, org: OrgId, rev: AppMapRev, blobs: &[(BlobOid, Vec<AppFactRecord>)]) -> Result<(), KnowledgeError> {
        let mut state = self.state.lock();
        for (oid, facts) in blobs {
            let rows = state.app_facts.entry((org, *oid, rev)).or_default();
            for fact in facts {
                rows.entry(fact.ord).or_insert_with(|| fact.clone());
            }
            state.stages.entry((org, *oid, BlobStage::AppFacts(rev))).or_insert(Marker { lang: None, status: StageStatus::Ok });
        }
        Ok(())
    }

    async fn app_facts_at(&self, org: OrgId, key: &RepoVersionKey, rev: AppMapRev) -> Result<Vec<AppFactAt>, KnowledgeError> {
        let state = self.state.lock();
        let view = View::new(&state, org, key, None);
        let mut out = Vec::new();
        for entry in view.entries() {
            for fact in state.app_facts.get(&(org, entry.blob, rev)).into_iter().flat_map(BTreeMap::values) {
                out.push(AppFactAt { path: entry.path.clone(), blob: entry.blob, fact: fact.clone() });
            }
        }
        Ok(out)
    }

    async fn replace_app_graph(&self, org: OrgId, key: &RepoVersionKey, graph: &AppGraph) -> Result<(), KnowledgeError> {
        let mut ids = BTreeSet::new();
        for node in &graph.nodes {
            if !ids.insert(node.id) {
                return Err(KnowledgeError::Invalid(format!("duplicate app node id {}", node.id)));
            }
        }
        let mut edge_keys = BTreeSet::new();
        let mut edges = Vec::new();
        for edge in &graph.edges {
            if !ids.contains(&edge.from) || !ids.contains(&edge.to) {
                return Err(KnowledgeError::Invalid(format!("app edge {} -> {} names an unknown node", edge.from, edge.to)));
            }
            if edge_keys.insert((edge.from, edge.to, edge.kind)) {
                edges.push(edge.clone());
            }
        }
        let mut nodes = graph.nodes.clone();
        nodes.sort_by_key(|node| node.id);
        self.state.lock().graphs.insert((org, *key), AppGraph { nodes, edges });
        Ok(())
    }

    async fn begin_scip_index(
        &self,
        org: OrgId,
        key: &RepoVersionKey,
        artifact: ArtifactId,
        meta: &ScipMetadata,
    ) -> Result<(), KnowledgeError> {
        let index = ScipIndex {
            repo: key.repo_id,
            sha: key.sha,
            tool: meta.tool_name.clone(),
            created_at: self.clock.now(),
            ready: false,
            languages: Vec::new(),
            documents: 0,
            docs: BTreeMap::new(),
            symbols: BTreeMap::new(),
        };
        self.state.lock().scip.insert((org, artifact), index);
        Ok(())
    }

    async fn put_scip_documents(&self, org: OrgId, artifact: ArtifactId, docs: &[ScipDocRows]) -> Result<(), KnowledgeError> {
        let mut state = self.state.lock();
        let index = state.ingesting_index(org, artifact)?;
        for rows in docs {
            let doc = index.docs.entry(rows.path.clone()).or_insert_with(|| ScipDoc { blob: rows.blob, occurrences: BTreeMap::new() });
            for (ord, occ) in rows.occurrences.iter().enumerate() {
                doc.occurrences.entry(to_u32(ord)).or_insert_with(|| occ.clone());
            }
        }
        Ok(())
    }

    async fn put_scip_symbols(&self, org: OrgId, artifact: ArtifactId, symbols: &[ScipSymbolInfo]) -> Result<(), KnowledgeError> {
        let mut state = self.state.lock();
        let index = state.ingesting_index(org, artifact)?;
        for symbol in symbols {
            index.symbols.entry(symbol.symbol.clone()).or_insert_with(|| symbol.clone());
        }
        Ok(())
    }

    async fn finish_scip_index(
        &self,
        org: OrgId,
        artifact: ArtifactId,
        languages: &[String],
        documents: u32,
    ) -> Result<(), KnowledgeError> {
        let mut state = self.state.lock();
        let index = state.ingesting_index(org, artifact)?;
        index.ready = true;
        index.languages = languages.to_vec();
        index.documents = documents;
        Ok(())
    }

    async fn put_commits(&self, org: OrgId, repo: RepoId, commits: &[CommitRecord]) -> Result<(), KnowledgeError> {
        let mut state = self.state.lock();
        for record in commits {
            let info = CommitInfo { files: None, ..record.info.clone() };
            let stored = match state.commits.entry((org, repo, record.info.sha)) {
                Entry::Occupied(slot) => {
                    let stored = slot.into_mut();
                    stored.info = info;
                    stored
                }
                Entry::Vacant(slot) => slot.insert(StoredCommit { info, files: BTreeMap::new() }),
            };
            for file in record.info.files.iter().flatten() {
                stored.files.insert(file.path.clone(), file.clone());
            }
        }
        Ok(())
    }

    async fn put_pull_requests(&self, org: OrgId, repo: RepoId, prs: &[PullRequestRecord]) -> Result<(), KnowledgeError> {
        let mut state = self.state.lock();
        for pr in prs {
            state.prs.insert((org, repo, pr.number), pr.clone());
        }
        Ok(())
    }

    async fn put_ticket_links(&self, org: OrgId, repo: RepoId, links: &[TicketLink]) -> Result<(), KnowledgeError> {
        let mut state = self.state.lock();
        for link in links {
            state.links.insert((org, repo, link.clone()));
        }
        Ok(())
    }

    async fn history_cursor(&self, org: OrgId, repo: RepoId) -> Result<HistoryCursor, KnowledgeError> {
        Ok(self.state.lock().cursors.get(&(org, repo)).cloned().unwrap_or_default())
    }

    async fn merge_history_cursor(&self, org: OrgId, repo: RepoId, cursor: &HistoryCursor) -> Result<(), KnowledgeError> {
        let mut state = self.state.lock();
        let stored = state.cursors.entry((org, repo)).or_default();
        *stored = stored.merged(cursor);
        Ok(())
    }

    async fn manifest_entries(&self, org: OrgId, key: &RepoVersionKey, paths: &[RepoPath]) -> Result<Vec<ManifestEntry>, KnowledgeError> {
        let state = self.state.lock();
        let view = View::new(&state, org, key, None);
        let paths: BTreeSet<&RepoPath> = paths.iter().collect();
        Ok(paths.into_iter().filter_map(|path| view.entry(path)).map(|entry| view.entry_out(entry)).collect())
    }

    async fn manifest_all(&self, org: OrgId, key: &RepoVersionKey, page: &PageReq) -> Result<Paged<ManifestEntry>, KnowledgeError> {
        let page = PageAt::parse(page)?;
        let state = self.state.lock();
        let view = View::new(&state, org, key, None);
        Ok(page.apply(view.entries().map(|entry| view.entry_out(entry)).collect()))
    }

    async fn list_dir(
        &self,
        org: OrgId,
        key: &RepoVersionKey,
        dir: Option<&RepoPath>,
        page: &PageReq,
    ) -> Result<Paged<DirEntry>, KnowledgeError> {
        let page = PageAt::parse(page)?;
        let prefix = dir.map(|dir| format!("{dir}/"));
        let state = self.state.lock();
        let view = View::new(&state, org, key, None);
        let mut children: BTreeMap<&str, DirEntry> = BTreeMap::new();
        for entry in view.entries() {
            let rest = match &prefix {
                None => entry.path.as_str(),
                Some(prefix) => match entry.path.as_str().strip_prefix(prefix.as_str()) {
                    Some(rest) => rest,
                    None => continue,
                },
            };
            match rest.split_once('/') {
                Some((name, _)) => {
                    children.entry(name).or_insert_with(|| DirEntry {
                        name: name.to_owned(),
                        kind: DirEntryKind::Dir,
                        size: None,
                        lang: None,
                    });
                }
                None => {
                    let kind = match entry.kind {
                        EntryKind::File => DirEntryKind::File,
                        EntryKind::Symlink => DirEntryKind::Symlink,
                        EntryKind::Submodule => DirEntryKind::Submodule,
                    };
                    children.insert(rest, DirEntry { name: rest.to_owned(), kind, size: Some(entry.size), lang: entry.lang });
                }
            }
        }
        Ok(page.apply(children.into_values().collect()))
    }

    async fn find_paths(
        &self,
        org: OrgId,
        key: &RepoVersionKey,
        m: &PathMatch,
        page: &PageReq,
    ) -> Result<Paged<ManifestEntry>, KnowledgeError> {
        let page = PageAt::parse(page)?;
        enum Matcher {
            Substring(String),
            Glob(Glob),
        }
        let matcher = match m {
            PathMatch::Substring(needle) => Matcher::Substring(needle.to_ascii_lowercase()),
            PathMatch::Glob(pattern) => {
                Matcher::Glob(Glob::new(pattern.clone()).map_err(|e| KnowledgeError::Invalid(format!("glob {pattern:?}: {e}")))?)
            }
        };
        let state = self.state.lock();
        let view = View::new(&state, org, key, None);
        let items = view
            .entries()
            .filter(|entry| match &matcher {
                Matcher::Substring(needle) => entry.path.as_str().to_ascii_lowercase().contains(needle.as_str()),
                Matcher::Glob(glob) => glob.matches(entry.path.as_str()),
            })
            .map(|entry| view.entry_out(entry))
            .collect();
        Ok(page.apply(items))
    }

    async fn paths_by_basename(
        &self,
        org: OrgId,
        key: &RepoVersionKey,
        basename: &str,
        limit: u16,
    ) -> Result<Vec<RepoPath>, KnowledgeError> {
        let state = self.state.lock();
        let view = View::new(&state, org, key, None);
        let mut paths: Vec<&RepoPath> = view.entries().map(|entry| &entry.path).filter(|path| path.file_name() == basename).collect();
        paths.sort_by_key(|path| (path.as_str().len(), *path));
        Ok(paths.into_iter().take(usize::from(limit)).cloned().collect())
    }

    async fn version_stats(&self, org: OrgId, key: &RepoVersionKey) -> Result<VersionStats, KnowledgeError> {
        let revs = self.revs(org, key);
        let state = self.state.lock();
        let view = View::new(&state, org, key, revs);
        let (mut files, mut bytes) = (0u32, 0u64);
        let mut by_lang: BTreeMap<Language, (u32, u64)> = BTreeMap::new();
        let mut dirs: BTreeMap<&str, u32> = BTreeMap::new();
        let mut counts = FactCounts::default();
        for entry in view.entries() {
            if entry.kind == EntryKind::File {
                files += 1;
                bytes += entry.size;
                if let Some(lang) = entry.lang {
                    let (n, lines) = by_lang.entry(lang).or_default();
                    *n += 1;
                    *lines += u64::from(view.line_count(entry.blob));
                }
                if let Some((first, _)) = entry.path.as_str().split_once('/') {
                    *dirs.entry(first).or_default() += 1;
                }
            }
            counts.symbols += to_u32(view.symbols(entry.blob).count());
            if let Some(revs) = revs {
                counts.error_sites += to_u32(view.error_sites(entry.blob, revs.errors).count());
            }
        }
        for node in view.nodes() {
            match node.kind {
                AppNodeKind::Endpoint => counts.endpoints += 1,
                AppNodeKind::Page => counts.pages += 1,
                AppNodeKind::Table => counts.tables += 1,
                _ => {}
            }
        }
        let mut by_lang: Vec<(Language, u32, u64)> = by_lang.into_iter().map(|(lang, (n, lines))| (lang, n, lines)).collect();
        by_lang.sort_by_key(|(lang, n, _)| (Reverse(*n), *lang));
        let mut top_dirs: Vec<(RepoPath, u32)> =
            dirs.into_iter().filter_map(|(dir, n)| RepoPath::new(dir).ok().map(|dir| (dir, n))).collect();
        top_dirs.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        top_dirs.truncate(VersionStats::TOP_DIRS);
        Ok(VersionStats { files, bytes, by_lang, top_dirs, counts })
    }

    async fn find_symbols(
        &self,
        org: OrgId,
        key: &RepoVersionKey,
        q: &SymbolQuery,
        page: &PageReq,
    ) -> Result<Paged<SymbolHit>, KnowledgeError> {
        let page = PageAt::parse(page)?;
        let revs = self.revs(org, key);
        let state = self.state.lock();
        let view = View::new(&state, org, key, revs);
        struct Ranked {
            hit: SymbolHit,
            qualified: bool,
            non_test: bool,
        }
        let mut ranked = Vec::new();
        for entry in view.entries() {
            if q.lang.is_some_and(|lang| entry.lang != Some(lang))
                || q.path_prefix.as_ref().is_some_and(|prefix| !entry.path.is_within(prefix.as_str()))
                || (!q.include_tests && entry.class == FileClass::Test)
            {
                continue;
            }
            for def in view.symbols(entry.blob) {
                if (!q.kinds.is_empty() && !q.kinds.contains(&def.kind)) || !name_matches(q.mode, &def.name, &q.name) {
                    continue;
                }
                let score = if def.name == q.name { 1.0 } else { trigram_similarity(&def.name, &q.name) };
                let qualified = q.qualifier.as_deref().is_some_and(|qualifier| qualifier_matches(qualifier, &def.qualified, &entry.path));
                let at = SymbolRef { path: entry.path.clone(), blob: entry.blob, ord: def.ord };
                ranked.push(Ranked { hit: hit(at, def, score), qualified, non_test: entry.class != FileClass::Test });
            }
        }
        ranked.sort_by(|a, b| {
            b.hit
                .score
                .total_cmp(&a.hit.score)
                .then(b.qualified.cmp(&a.qualified))
                .then(b.hit.def.exported.cmp(&a.hit.def.exported))
                .then(b.non_test.cmp(&a.non_test))
                .then(a.hit.at.path.as_str().len().cmp(&b.hit.at.path.as_str().len()))
                .then_with(|| a.hit.at.path.cmp(&b.hit.at.path))
                .then(a.hit.at.ord.cmp(&b.hit.at.ord))
        });
        Ok(page.apply(ranked.into_iter().map(|r| r.hit).collect()))
    }

    async fn symbols_at(&self, org: OrgId, key: &RepoVersionKey, path: &RepoPath, line: u32) -> Result<Vec<SymbolHit>, KnowledgeError> {
        let revs = self.revs(org, key);
        let state = self.state.lock();
        let view = View::new(&state, org, key, revs);
        let Some(entry) = view.entry(path) else { return Ok(Vec::new()) };
        let mut defs: Vec<&SymbolDef> = view.symbols(entry.blob).filter(|def| def.span.contains(line)).collect();
        defs.sort_by_key(|def| (def.span.len(), Reverse(def.ord)));
        Ok(defs.into_iter().map(|def| hit(SymbolRef { path: path.clone(), blob: entry.blob, ord: def.ord }, def, 1.0)).collect())
    }

    async fn symbols_in_file(&self, org: OrgId, key: &RepoVersionKey, path: &RepoPath) -> Result<Vec<SymbolHit>, KnowledgeError> {
        let revs = self.revs(org, key);
        let state = self.state.lock();
        let view = View::new(&state, org, key, revs);
        let Some(entry) = view.entry(path) else { return Ok(Vec::new()) };
        Ok(view.symbols(entry.blob).map(|def| hit(SymbolRef { path: path.clone(), blob: entry.blob, ord: def.ord }, def, 1.0)).collect())
    }

    async fn symbols_by_ref(&self, org: OrgId, key: &RepoVersionKey, refs: &[SymbolRef]) -> Result<Vec<SymbolHit>, KnowledgeError> {
        let revs = self.revs(org, key);
        let state = self.state.lock();
        let view = View::new(&state, org, key, revs);
        Ok(refs.iter().filter_map(|r| view.resolve(r).map(|def| hit(r.clone(), def, 1.0))).collect())
    }

    async fn name_refs(&self, org: OrgId, key: &RepoVersionKey, q: &NameRefQuery, page: &PageReq) -> Result<Paged<RefHit>, KnowledgeError> {
        let page = PageAt::parse(page)?;
        let revs = self.revs(org, key);
        let state = self.state.lock();
        let view = View::new(&state, org, key, revs);
        let mut hits = Vec::new();
        for entry in view.entries().filter(|entry| q.include_tests || entry.class != FileClass::Test) {
            let matching = view.refs(entry.blob).filter(|r| {
                r.name == q.name
                    && q.qualifier.as_ref().is_none_or(|qualifier| r.qualifier.as_ref() == Some(qualifier))
                    && (q.kinds.is_empty() || q.kinds.contains(&r.kind))
            });
            for r in matching {
                let enclosing = r.enclosing.map(|ord| SymbolRef { path: entry.path.clone(), blob: entry.blob, ord });
                hits.push(RefHit { path: entry.path.clone(), pos: r.pos, kind: r.kind, enclosing });
            }
        }
        hits.sort_by(|a, b| a.path.cmp(&b.path).then(a.pos.cmp(&b.pos)));
        Ok(page.apply(hits))
    }

    async fn call_edges(
        &self,
        org: OrgId,
        key: &RepoVersionKey,
        from: &[SymbolRef],
        dir: CallDirection,
        precision: &Precision,
        limit: u32,
    ) -> Result<Vec<CallEdge>, KnowledgeError> {
        let revs = self.revs(org, key);
        let state = self.state.lock();
        let view = View::new(&state, org, key, revs);
        let index = match precision {
            Precision::Name => None,
            Precision::Precise => Some(state.ready_index_at(org, key.repo_id, key.sha).ok_or(KnowledgeError::NotFound)?.1),
            Precision::PreciseStale { from_sha } => {
                Some(state.ready_index_at(org, key.repo_id, *from_sha).ok_or(KnowledgeError::NotFound)?.1)
            }
        };
        let mut seen = BTreeSet::new();
        let targets: Vec<(&SymbolRef, &SymbolDef)> =
            from.iter().filter(|r| seen.insert(*r)).filter_map(|r| view.resolve(r).map(|def| (r, def))).collect();
        let mut edges = EdgeMap::new();
        match index {
            None => name_edges(&view, &targets, dir, &mut edges),
            Some(index) => scip_edges(&view, index, &targets, dir, &mut edges),
        }
        Ok(edges
            .into_iter()
            .take(to_usize(limit))
            .map(|((from, to), mut sites)| {
                sites.sort();
                CallEdge { from, to, sites }
            })
            .collect())
    }

    async fn scip_index_for(&self, org: OrgId, key: &RepoVersionKey) -> Result<Option<ScipIndexInfo>, KnowledgeError> {
        let state = self.state.lock();
        let found = state
            .ready_index_at(org, key.repo_id, key.sha)
            .or_else(|| state.ready_indexes(org, key.repo_id).max_by_key(|(artifact, index)| (index.created_at, *artifact)));
        Ok(found.map(|(artifact, index)| ScipIndexInfo {
            artifact,
            sha: index.sha,
            tool: index.tool.clone(),
            languages: index.languages.clone(),
            documents: index.documents,
        }))
    }

    async fn scip_symbol_at(
        &self,
        org: OrgId,
        index: ArtifactId,
        path: &RepoPath,
        line: u32,
        col: Option<u32>,
    ) -> Result<Option<ScipSymbolHit>, KnowledgeError> {
        let state = self.state.lock();
        let Some(index) = state.ready_index(org, index) else { return Ok(None) };
        let Some(doc) = index.docs.get(path) else { return Ok(None) };
        let Some((_, occ)) =
            doc.occurrences.iter().filter(|(_, occ)| occ.range.covers(line, col)).min_by_key(|(ord, occ)| (range_size(&occ.range), **ord))
        else {
            return Ok(None);
        };
        let symbol = occ.symbol.clone();
        let display_name = index.symbols.get(&symbol).and_then(|info| info.display_name.clone());
        let def = index.docs.iter().find_map(|(path, doc)| {
            doc.occurrences.values().find(|o| o.symbol == symbol && o.roles.is_definition()).map(|o| (path.clone(), o.range))
        });
        Ok(Some(ScipSymbolHit { symbol, display_name, def }))
    }

    async fn scip_occurrences(
        &self,
        org: OrgId,
        index: ArtifactId,
        key: &RepoVersionKey,
        symbol: &str,
        defs: bool,
        page: &PageReq,
    ) -> Result<Paged<RefHit>, KnowledgeError> {
        let page = PageAt::parse(page)?;
        let revs = self.revs(org, key);
        let state = self.state.lock();
        let view = View::new(&state, org, key, revs);
        let mut hits = Vec::new();
        if let Some(index) = state.ready_index(org, index) {
            for (path, doc) in index.docs.iter().filter(|(path, doc)| view.usable(path, doc)) {
                for occ in doc.occurrences.values().filter(|occ| occ.symbol == symbol && (defs || !occ.roles.is_definition())) {
                    let kind = if occ.roles.contains(ScipRoles::IMPORT) { RefKind::Import } else { RefKind::Read };
                    let line = occ.range.start_line;
                    let enclosing =
                        view.innermost(doc.blob, line).map(|def| SymbolRef { path: path.clone(), blob: doc.blob, ord: def.ord });
                    hits.push(RefHit { path: path.clone(), pos: (line, occ.range.start_col), kind, enclosing });
                }
            }
        }
        hits.sort_by(|a, b| a.path.cmp(&b.path).then(a.pos.cmp(&b.pos)));
        Ok(page.apply(hits))
    }

    async fn match_errors(
        &self,
        org: OrgId,
        key: &RepoVersionKey,
        rev: ErrorsRev,
        q: &ErrorQuery,
    ) -> Result<Vec<ErrorMatch>, KnowledgeError> {
        let state = self.state.lock();
        let view = View::new(&state, org, key, None);
        let rows: Vec<(&ManifestEntry, &ErrorSiteRecord)> =
            view.entries().flat_map(|entry| view.error_sites(entry.blob, rev).map(move |site| (entry, site))).collect();
        let records: Vec<&ErrorSiteRecord> = rows.iter().map(|(_, site)| *site).collect();
        Ok(match_rows(&records, q)
            .into_iter()
            .map(|m| {
                let (entry, site) = rows[m.row];
                ErrorMatch {
                    path: entry.path.clone(),
                    blob: entry.blob,
                    site: site.clone(),
                    segment: m.segment,
                    span: m.span,
                    verified: m.verified,
                    score: m.score,
                }
            })
            .collect())
    }

    async fn match_catalog(
        &self,
        org: OrgId,
        key: &RepoVersionKey,
        rev: ErrorsRev,
        q: &ErrorQuery,
    ) -> Result<Vec<CatalogMatch>, KnowledgeError> {
        let state = self.state.lock();
        let view = View::new(&state, org, key, None);
        let rows: Vec<(&RepoPath, &CatalogEntryRecord)> =
            view.entries().flat_map(|entry| view.catalog(entry.blob, rev).map(move |record| (&entry.path, record))).collect();
        let records: Vec<&CatalogEntryRecord> = rows.iter().map(|(_, record)| *record).collect();
        Ok(match_rows(&records, q)
            .into_iter()
            .map(|m| {
                let (path, entry) = rows[m.row];
                CatalogMatch {
                    path: path.clone(),
                    entry: entry.clone(),
                    segment: m.segment,
                    span: m.span,
                    verified: m.verified,
                    score: m.score,
                }
            })
            .collect())
    }

    async fn sites_for_i18n_key(
        &self,
        org: OrgId,
        key: &RepoVersionKey,
        rev: ErrorsRev,
        i18n_key: &str,
        page: &PageReq,
    ) -> Result<Paged<ErrorMatch>, KnowledgeError> {
        let page = PageAt::parse(page)?;
        let state = self.state.lock();
        let view = View::new(&state, org, key, None);
        let mut hits = Vec::new();
        for entry in view.entries() {
            for site in view.error_sites(entry.blob, rev) {
                if site.kind == ErrorSiteKind::I18nUse && site.i18n_key.as_deref() == Some(i18n_key) {
                    hits.push(ErrorMatch {
                        path: entry.path.clone(),
                        blob: entry.blob,
                        site: site.clone(),
                        segment: 0,
                        span: (0, 0),
                        verified: true,
                        score: 1.0,
                    });
                }
            }
        }
        hits.sort_by(|a, b| a.path.cmp(&b.path).then(a.site.pos.cmp(&b.site.pos)));
        Ok(page.apply(hits))
    }

    async fn error_sites_in(
        &self,
        org: OrgId,
        key: &RepoVersionKey,
        rev: ErrorsRev,
        symbol: &SymbolRef,
        limit: u32,
    ) -> Result<Vec<ErrorMatch>, KnowledgeError> {
        let revs = self.revs(org, key);
        let state = self.state.lock();
        let view = View::new(&state, org, key, revs);
        let Some(def) = view.resolve(symbol) else { return Ok(Vec::new()) };
        let mut sites: Vec<&ErrorSiteRecord> = view.error_sites(symbol.blob, rev).filter(|site| def.span.contains(site.pos.0)).collect();
        sites.sort_by_key(|site| site.pos);
        Ok(sites
            .into_iter()
            .take(to_usize(limit))
            .map(|site| ErrorMatch {
                path: symbol.path.clone(),
                blob: symbol.blob,
                site: site.clone(),
                segment: 0,
                span: (0, 0),
                verified: true,
                score: 1.0,
            })
            .collect())
    }

    async fn app_nodes(
        &self,
        org: OrgId,
        key: &RepoVersionKey,
        kinds: &[AppNodeKind],
        page: &PageReq,
    ) -> Result<Paged<AppNode>, KnowledgeError> {
        let page = PageAt::parse(page)?;
        let state = self.state.lock();
        let view = View::new(&state, org, key, None);
        Ok(page.apply(view.nodes().filter(|node| kinds.is_empty() || kinds.contains(&node.kind)).cloned().collect()))
    }

    async fn app_neighborhood(
        &self,
        org: OrgId,
        key: &RepoVersionKey,
        focus: &[u32],
        depth: u8,
        max_nodes: u32,
    ) -> Result<AppGraph, KnowledgeError> {
        let state = self.state.lock();
        let Some(graph) = state.graphs.get(&(org, *key)) else { return Ok(AppGraph::default()) };
        let known: BTreeSet<u32> = graph.nodes.iter().map(|node| node.id).collect();
        let mut neighbors: BTreeMap<u32, BTreeSet<u32>> = BTreeMap::new();
        for edge in &graph.edges {
            neighbors.entry(edge.from).or_default().insert(edge.to);
            neighbors.entry(edge.to).or_default().insert(edge.from);
        }
        let max_nodes = to_usize(max_nodes);
        let mut visited: BTreeSet<u32> = BTreeSet::new();
        let mut queue: VecDeque<(u32, u8)> = VecDeque::new();
        for id in focus.iter().filter(|id| known.contains(id)) {
            if visited.len() < max_nodes && visited.insert(*id) {
                queue.push_back((*id, 0));
            }
        }
        while let Some((id, distance)) = queue.pop_front() {
            if distance >= depth {
                continue;
            }
            for next in neighbors.get(&id).into_iter().flatten() {
                if visited.len() >= max_nodes {
                    break;
                }
                if visited.insert(*next) {
                    queue.push_back((*next, distance + 1));
                }
            }
        }
        Ok(AppGraph {
            nodes: graph.nodes.iter().filter(|node| visited.contains(&node.id)).cloned().collect(),
            edges: graph.edges.iter().filter(|edge| visited.contains(&edge.from) && visited.contains(&edge.to)).cloned().collect(),
        })
    }

    async fn app_nodes_for_symbols(&self, org: OrgId, key: &RepoVersionKey, symbols: &[SymbolRef]) -> Result<Vec<AppNode>, KnowledgeError> {
        let state = self.state.lock();
        let view = View::new(&state, org, key, None);
        Ok(view.nodes().filter(|node| node.symbol.as_ref().is_some_and(|symbol| symbols.contains(symbol))).cloned().collect())
    }

    async fn glossary_hits(
        &self,
        org: OrgId,
        key: &RepoVersionKey,
        words: &[String],
        limit: u32,
    ) -> Result<Vec<GlossaryHit>, KnowledgeError> {
        if words.is_empty() {
            return Ok(Vec::new());
        }
        let revs = self.revs(org, key);
        let state = self.state.lock();
        let view = View::new(&state, org, key, revs);
        let distinct: BTreeSet<&str> = words.iter().map(String::as_str).collect();
        let weigh = |text: &str| {
            let lower = text.to_lowercase();
            let contained = distinct.iter().filter(|word| lower.contains(**word)).count();
            (contained > 0).then(|| contained as f32 / words.len() as f32)
        };
        let mut hits = Vec::new();
        let mut push = |source: GlossarySource, text: &str, at: (RepoPath, u32)| {
            if let Some(weight) = weigh(text) {
                hits.push(GlossaryHit { source, text: text.to_owned(), at, weight });
            }
        };
        for entry in view.entries() {
            for def in view.symbols(entry.blob) {
                push(GlossarySource::Symbol, &def.name, (entry.path.clone(), def.name_pos.0));
            }
            if let Some(revs) = revs {
                for record in view.catalog(entry.blob, revs.errors) {
                    push(GlossarySource::CatalogKey, &record.key, (entry.path.clone(), record.line));
                    push(GlossarySource::CatalogText, &record.template, (entry.path.clone(), record.line));
                }
            }
        }
        for node in view.nodes() {
            if let (Some(source), Some(at)) = (glossary_node_source(node.kind), &node.at) {
                push(source, &node.key, at.clone());
            }
        }
        hits.sort_by(|a, b| {
            b.weight.total_cmp(&a.weight).then(a.source.cmp(&b.source)).then_with(|| a.text.cmp(&b.text)).then_with(|| a.at.cmp(&b.at))
        });
        hits.dedup_by(|a, b| a.source == b.source && a.text == b.text && a.at == b.at);
        hits.truncate(to_usize(limit));
        Ok(hits)
    }

    async fn glossary_top(&self, org: OrgId, key: &RepoVersionKey, limit: u32) -> Result<Vec<GlossaryHit>, KnowledgeError> {
        let revs = self.revs(org, key);
        let state = self.state.lock();
        let view = View::new(&state, org, key, revs);
        let mut groups = BTreeMap::new();
        for node in view.nodes() {
            let Some(at) = &node.at else { continue };
            match node.kind {
                AppNodeKind::Table => add_top(&mut groups, GlossarySource::Table, node.key.clone(), at.clone()),
                AppNodeKind::Endpoint => {
                    if let Some(resource) = endpoint_resource(&node.key) {
                        add_top(&mut groups, GlossarySource::Endpoint, resource.to_owned(), at.clone());
                    }
                }
                _ => {}
            }
        }
        let type_kinds =
            [SymbolKind::Struct, SymbolKind::Class, SymbolKind::Interface, SymbolKind::Trait, SymbolKind::Enum, SymbolKind::TypeAlias];
        for entry in view.entries() {
            for def in view.symbols(entry.blob).filter(|def| def.exported && type_kinds.contains(&def.kind)) {
                add_top(&mut groups, GlossarySource::Symbol, def.name.clone(), (entry.path.clone(), def.name_pos.0));
            }
        }
        let mut top: Vec<GlossaryHit> =
            groups.into_iter().map(|((source, text), (count, at))| GlossaryHit { source, text, at, weight: count as f32 }).collect();
        top.sort_by(|a, b| b.weight.total_cmp(&a.weight).then_with(|| a.text.cmp(&b.text)).then(a.source.cmp(&b.source)));
        top.truncate(to_usize(limit));
        Ok(top)
    }

    async fn manifest_diff(
        &self,
        org: OrgId,
        from: &RepoVersionKey,
        to: &RepoVersionKey,
        prefix: Option<&RepoPath>,
        page: &PageReq,
    ) -> Result<Paged<ManifestChange>, KnowledgeError> {
        let page = PageAt::parse(page)?;
        if from.repo_id != to.repo_id {
            return Err(KnowledgeError::Invalid("manifest_diff across repos".into()));
        }
        let state = self.state.lock();
        let empty = BTreeMap::new();
        let old = state.manifests.get(&(org, *from)).unwrap_or(&empty);
        let new = state.manifests.get(&(org, *to)).unwrap_or(&empty);
        let paths: BTreeSet<&RepoPath> = old.keys().chain(new.keys()).collect();
        let changes = paths
            .into_iter()
            .filter(|path| prefix.is_none_or(|prefix| path.is_within(prefix.as_str())))
            .filter_map(|path| {
                let (before, after) = (old.get(path), new.get(path));
                let change = match (before, after) {
                    (None, None) => return None,
                    (None, Some(_)) => ChangeKind::Added,
                    (Some(_), None) => ChangeKind::Deleted,
                    (Some(a), Some(b)) if a.kind != b.kind => ChangeKind::TypeChanged,
                    (Some(a), Some(b)) if a.blob != b.blob => ChangeKind::Modified,
                    (Some(_), Some(_)) => return None,
                };
                Some(ManifestChange { path: path.clone(), change, old: before.map(|e| e.blob), new: after.map(|e| e.blob) })
            })
            .collect();
        Ok(page.apply(changes))
    }

    async fn has_commit(&self, org: OrgId, repo: RepoId, sha: CommitSha) -> Result<bool, KnowledgeError> {
        Ok(self.state.lock().commits.contains_key(&(org, repo, sha)))
    }

    async fn commits(&self, org: OrgId, repo: RepoId, shas: &[CommitSha]) -> Result<Vec<CommitRecord>, KnowledgeError> {
        let state = self.state.lock();
        Ok(dedup_in_order(shas).into_iter().filter_map(|sha| state.commits.get(&(org, repo, sha)).map(StoredCommit::record)).collect())
    }

    async fn pull_requests_for_commits(
        &self,
        org: OrgId,
        repo: RepoId,
        shas: &[CommitSha],
    ) -> Result<Vec<(CommitSha, PullRequestRecord)>, KnowledgeError> {
        let state = self.state.lock();
        let mut out = Vec::new();
        for sha in dedup_in_order(shas) {
            for ((o, r, _), pr) in &state.prs {
                if *o == org && *r == repo && pr.merge_commit == Some(sha) {
                    out.push((sha, pr.clone()));
                }
            }
        }
        Ok(out)
    }

    async fn ticket_links(&self, org: OrgId, repo: RepoId, by: &LinkLookup) -> Result<Vec<TicketLink>, KnowledgeError> {
        let state = self.state.lock();
        let selected = |link: &TicketLink| match by {
            LinkLookup::Identifier(identifier) => link.identifier.eq_ignore_ascii_case(identifier),
            LinkLookup::Commits(shas) => link.commit.is_some_and(|sha| shas.contains(&sha)),
            LinkLookup::Prs(numbers) => link.pr.is_some_and(|pr| numbers.contains(&pr)),
        };
        Ok(state.links.iter().filter(|(o, r, link)| *o == org && *r == repo && selected(link)).map(|(_, _, link)| link.clone()).collect())
    }

    async fn delete_version_facts(&self, org: OrgId, key: &RepoVersionKey) -> Result<(), KnowledgeError> {
        let mut state = self.state.lock();
        state.manifests.remove(&(org, *key));
        state.graphs.remove(&(org, *key));
        state.scip.retain(|(o, _), index| !(*o == org && index.repo == key.repo_id && index.sha == key.sha));
        Ok(())
    }

    async fn sweep_unreferenced_blobs(&self, org: OrgId, grace: Duration, max: u32) -> Result<Vec<BlobOid>, KnowledgeError> {
        let now = self.clock.now();
        let Some(cutoff) = time::Duration::try_from(grace).ok().and_then(|grace| now.checked_sub(grace)) else {
            return Ok(Vec::new());
        };
        let mut state = self.state.lock();
        let referenced: BTreeSet<BlobOid> = state
            .manifests
            .iter()
            .filter(|((o, _), _)| *o == org)
            .flat_map(|(_, manifest)| manifest.values().map(|entry| entry.blob))
            .collect();
        let doomed: Vec<(BlobOid, bool)> = state
            .blobs
            .iter()
            .filter(|((o, oid), row)| *o == org && row.created_at < cutoff && !referenced.contains(oid))
            .take(to_usize(max))
            .map(|((_, oid), row)| (*oid, row.meta.content_stored))
            .collect();
        let oids: BTreeSet<BlobOid> = doomed.iter().map(|(oid, _)| *oid).collect();
        let gone = |o: &OrgId, oid: &BlobOid| *o == org && oids.contains(oid);
        state.blobs.retain(|(o, oid), _| !gone(o, oid));
        state.stages.retain(|(o, oid, _), _| !gone(o, oid));
        state.parsed.retain(|(o, oid, _), _| !gone(o, oid));
        state.error_sites.retain(|(o, oid, _), _| !gone(o, oid));
        state.catalog.retain(|(o, oid, _), _| !gone(o, oid));
        state.app_facts.retain(|(o, oid, _), _| !gone(o, oid));
        Ok(doomed.into_iter().filter(|(_, content_stored)| *content_stored).map(|(oid, _)| oid).collect())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::MemKnowledgeStore;
    use crate::ports::clock::Clock;
    use crate::ports::ids::IdGen;
    use crate::testing::{KnowledgeFixture, MemJobQueue, MemRepoStore, SeqIdGen, contract_knowledge_store};

    #[tokio::test]
    async fn mem_knowledge_store_passes_the_contract() {
        contract_knowledge_store(|clock| async move {
            let clock: Arc<dyn Clock> = clock;
            let ids: Arc<dyn IdGen> = Arc::new(SeqIdGen::new());
            let jobs = Arc::new(MemJobQueue::new(clock.clone(), ids));
            let repos = Arc::new(MemRepoStore::new(clock.clone(), jobs));
            KnowledgeFixture { knowledge: Arc::new(MemKnowledgeStore::new(clock, repos.clone())), repos }
        })
        .await;
    }
}
