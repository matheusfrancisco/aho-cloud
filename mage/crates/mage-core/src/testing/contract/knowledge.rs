//! `KnowledgeStore` contract (knowledge.md "Testing"): tenant isolation, the sha view, rev
//! filtering, write-once puts, every read's order and paging, SCIP ingest, error matching, the app
//! graph, the glossary, manifest diffs, history and GC. Scenarios go through the `KnowledgeStore`
//! and `RepoStore` traits only, so real adapters run them unchanged.

use std::collections::BTreeSet;
use std::fmt::Debug;
use std::sync::Arc;
use std::time::Duration;

use uuid::Uuid;

use super::KnowledgeFixture;
use crate::domain::ids::{ArtifactId, BlobOid, CommitSha, InstallationId, OrgId, RepoId, RepoVersionKey, TreeOid};
use crate::domain::path::{FileClass, Language, LineRange, RepoPath};
use crate::domain::repo::{IndexRevs, Repo, RepoAccess, VersionReason};
use crate::ports::knowledge::{
    AppEdge, AppEdgeKind, AppFact, AppFactRecord, AppGraph, AppMapRev, AppNode, AppNodeKind, BlobMeta, BlobStage, CallDirection, CallEdge,
    CatalogEntryRecord, CommitRecord, Construct, DirEntry, DirEntryKind, EntryKind, ErrorMatch, ErrorQuery, ErrorSiteKind, ErrorSiteRecord,
    ErrorsRev, FUZZY_THRESHOLD, FactCounts, FrameworkId, GlossaryHit, GlossarySource, HistoryCursor, KnowledgeError, KnowledgeStore,
    LinkLookup, LinkSource, LitPart, LiteralContext, LiteralSite, ManifestChange, ManifestEntry, NameMatch, NameRef, NameRefQuery, PageReq,
    Paged, ParsedBlob, ParserRev, PathMatch, Precision, PullRequestRecord, RefHit, RefKind, ScipDocRows, ScipIndexInfo, ScipMetadata,
    ScipOccurrence, ScipRange, ScipRoles, ScipSymbolHit, ScipSymbolInfo, StageStatus, SymbolDef, SymbolHit, SymbolKind, SymbolQuery,
    SymbolRef, TableOp, TicketLink, VersionStats, trigram_similarity,
};
use crate::ports::repos::RepoStore;
use crate::testing::{FakeClock, join_all};
use crate::wire::indexer::{ChangeKind, CommitInfo, FileChange};

/// Runs every `KnowledgeStore` scenario, each against a fresh `make(clock)` fixture.
pub async fn contract_knowledge_store<F, Fut>(make: F)
where
    F: Fn(Arc<FakeClock>) -> Fut,
    Fut: Future<Output = KnowledgeFixture>,
{
    paging_rejects_limit_zero(fresh(&make).await).await;
    tenants_never_see_each_other(fresh(&make).await).await;
    a_version_sees_the_facts_of_its_blobs(fresh(&make).await).await;
    reads_follow_the_version_revs(fresh(&make).await).await;
    puts_are_write_once(fresh(&make).await).await;
    missing_and_blob_meta_are_by_oid(fresh(&make).await).await;
    literal_sites_skip_failed_parses(fresh(&make).await).await;
    manifest_reads(fresh(&make).await).await;
    version_stats_summarize_the_version(fresh(&make).await).await;
    find_symbols_match_rank_and_page(fresh(&make).await).await;
    symbol_lookups_by_position_file_and_ref(fresh(&make).await).await;
    name_refs_filter_and_order(fresh(&make).await).await;
    call_edges_by_name(fresh(&make).await).await;
    call_edges_from_scip(fresh(&make).await).await;
    scip_ingest_and_reads(fresh(&make).await).await;
    match_errors_contain_verify_and_fall_back(fresh(&make).await).await;
    match_catalog_verifies_entries(fresh(&make).await).await;
    i18n_sites_and_sites_in_a_symbol(fresh(&make).await).await;
    app_graph_replace_and_traverse(fresh(&make).await).await;
    glossary_hits_and_top(fresh(&make).await).await;
    manifest_diff_joins_two_versions(fresh(&make).await).await;
    history_round_trips(fresh(&make).await).await;
    delete_version_facts_drops_one_sha(fresh(&make).await).await;
    sweep_respects_grace_and_references(fresh(&make).await).await;
}

// ---------------------------------------------------------------------------------------------
// Harness

struct Env {
    k: Arc<dyn KnowledgeStore>,
    repos: Arc<dyn RepoStore>,
    clock: Arc<FakeClock>,
}

async fn fresh<F, Fut>(make: &F) -> Env
where
    F: Fn(Arc<FakeClock>) -> Fut,
    Fut: Future<Output = KnowledgeFixture>,
{
    let clock = Arc::new(FakeClock::new());
    let fixture = make(clock.clone()).await;
    Env { k: fixture.knowledge, repos: fixture.repos, clock }
}

trait Must<T> {
    fn must(self, what: &str) -> T;
}

impl<T, E: Debug> Must<T> for Result<T, E> {
    #[track_caller]
    fn must(self, what: &str) -> T {
        match self {
            Ok(value) => value,
            Err(e) => panic!("{what} failed: {e:?}"),
        }
    }
}

#[track_caller]
fn assert_invalid<T: Debug>(result: Result<T, KnowledgeError>, what: &str) {
    assert!(matches!(result, Err(KnowledgeError::Invalid(_))), "{what}: expected Invalid, got {result:?}");
}

#[track_caller]
fn assert_not_found<T: Debug>(result: Result<T, KnowledgeError>, what: &str) {
    assert!(matches!(result, Err(KnowledgeError::NotFound)), "{what}: expected NotFound, got {result:?}");
}

#[track_caller]
fn assert_close(actual: f32, expected: f32, what: &str) {
    assert!((actual - expected).abs() < 1e-5, "{what}: score {actual}, expected {expected}");
}

/// Large enough for one page to hold every item of a scenario.
const ALL: u32 = 1000;

fn page(limit: u32) -> PageReq {
    PageReq::first(limit)
}

/// Follows `next` cursors from the first page; every page followed by a cursor is full.
async fn all_pages<T, F, Fut>(limit: u32, mut fetch: F) -> Vec<T>
where
    F: FnMut(PageReq) -> Fut,
    Fut: Future<Output = Result<Paged<T>, KnowledgeError>>,
{
    let mut items = Vec::new();
    let mut after = None;
    for _ in 0..ALL {
        let got = fetch(PageReq { limit, after: after.take() }).await.must("page");
        assert!(got.items.len() <= limit as usize, "a page holds at most `limit` items");
        let Some(next) = got.next else {
            items.extend(got.items);
            return items;
        };
        assert_eq!(got.items.len(), limit as usize, "a page followed by a cursor is full");
        items.extend(got.items);
        after = Some(next);
    }
    panic!("paging did not terminate");
}

fn org(n: u8) -> OrgId {
    OrgId::from_uuid(Uuid::from_u128(0x0a00 + u128::from(n)))
}

fn oid(n: u8) -> BlobOid {
    BlobOid::from_bytes([n; 20])
}

fn sha(n: u8) -> CommitSha {
    CommitSha::from_bytes([n; 20])
}

fn art(n: u8) -> ArtifactId {
    ArtifactId::from_uuid(Uuid::from_u128(0x0b00 + u128::from(n)))
}

#[track_caller]
fn path(s: &str) -> RepoPath {
    RepoPath::new(s).unwrap_or_else(|e| panic!("bad test path {s:?}: {e}"))
}

#[track_caller]
fn lines(start: u32, end: u32) -> LineRange {
    LineRange::new(start, end).unwrap_or_else(|e| panic!("bad test range {start}-{end}: {e}"))
}

fn revs(n: u32) -> IndexRevs {
    IndexRevs { parser: ParserRev(n), errors: ErrorsRev(n), appmap: AppMapRev(n) }
}

fn sref(p: &str, blob: BlobOid, ord: u32) -> SymbolRef {
    SymbolRef { path: path(p), blob, ord }
}

impl Env {
    async fn repo(&self, org: OrgId, n: u8) -> RepoId {
        let seed = org.as_uuid().as_u128().wrapping_mul(1000) + u128::from(n);
        let repo = Repo {
            id: RepoId::from_uuid(Uuid::from_u128(seed)),
            org_id: org,
            external_id: seed as u64,
            owner: "acme".into(),
            name: format!("repo-{n}"),
            default_branch: "main".into(),
            installation_id: InstallationId(7),
            access: RepoAccess::Granted,
            created_at: FakeClock::DEFAULT_START,
        };
        self.repos.upsert_repo(&repo, &[]).await.must("upsert_repo").id
    }

    /// Version `sha` of repo `n` of `org`, with `revs` set when given.
    async fn version(&self, org: OrgId, n: u8, sha: CommitSha, revs: Option<IndexRevs>) -> RepoVersionKey {
        let repo = self.repo(org, n).await;
        self.repos.add_version_reason(org, repo, sha, VersionReason::Release(sha.to_hex())).await.must("add_version_reason");
        let key = RepoVersionKey::new(repo, sha);
        if let Some(revs) = revs {
            self.set_revs(org, key, revs).await;
        }
        key
    }

    async fn set_revs(&self, org: OrgId, key: RepoVersionKey, revs: IndexRevs) {
        self.repos.set_index_revs(org, key, &revs).await.must("set_index_revs");
    }

    async fn manifest(&self, org: OrgId, key: &RepoVersionKey, entries: &[ManifestEntry]) {
        self.k.put_manifest(org, key, entries).await.must("put_manifest");
    }

    async fn parse(&self, org: OrgId, rev: u32, blobs: Vec<(BlobOid, Result<ParsedBlob, String>)>) {
        self.k.put_parsed(org, ParserRev(rev), &blobs).await.must("put_parsed");
    }

    async fn sites(&self, org: OrgId, rev: u32, blobs: Vec<(BlobOid, Vec<ErrorSiteRecord>)>) {
        self.k.put_error_sites(org, ErrorsRev(rev), &blobs).await.must("put_error_sites");
    }

    async fn graph(&self, org: OrgId, key: &RepoVersionKey, nodes: Vec<AppNode>, edges: Vec<AppEdge>) {
        self.k.replace_app_graph(org, key, &AppGraph { nodes, edges }).await.must("replace_app_graph");
    }

    async fn names_in(&self, org: OrgId, key: &RepoVersionKey, file: &str) -> Vec<String> {
        self.k.symbols_in_file(org, key, &path(file)).await.must("symbols_in_file").into_iter().map(|hit| hit.def.name).collect()
    }

    async fn find(&self, org: OrgId, key: &RepoVersionKey, q: &SymbolQuery) -> Vec<SymbolHit> {
        self.k.find_symbols(org, key, q, &page(ALL)).await.must("find_symbols").items
    }

    async fn missing(&self, org: OrgId, stage: BlobStage, oids: &[BlobOid]) -> Vec<BlobOid> {
        self.k.missing(org, stage, oids).await.must("missing")
    }

    async fn meta_oids(&self, org: OrgId, oids: &[BlobOid]) -> Vec<BlobOid> {
        self.k.blob_meta(org, oids).await.must("blob_meta").into_iter().map(|meta| meta.oid).collect()
    }

    async fn ingest(&self, org: OrgId, key: &RepoVersionKey, artifact: ArtifactId, docs: &[ScipDocRows]) {
        self.k.begin_scip_index(org, key, artifact, &scip_meta()).await.must("begin_scip_index");
        self.k.put_scip_documents(org, artifact, docs).await.must("put_scip_documents");
        self.k.put_scip_symbols(org, artifact, &scip_symbols()).await.must("put_scip_symbols");
        let documents = u32::try_from(docs.len()).unwrap_or(u32::MAX);
        self.k.finish_scip_index(org, artifact, &["go".to_owned()], documents).await.must("finish_scip_index");
    }
}

// ---------------------------------------------------------------------------------------------
// Fact builders

fn entry(p: &str, blob: BlobOid, lang: Option<Language>, class: FileClass, size: u64) -> ManifestEntry {
    ManifestEntry { path: path(p), kind: EntryKind::File, executable: false, blob, lang, class, size, is_text: false, line_count: 0 }
}

/// A Go source file.
fn file(p: &str, blob: BlobOid) -> ManifestEntry {
    entry(p, blob, Some(Language::Go), FileClass::Source, 10)
}

fn meta(blob: BlobOid, line_count: u32, content_stored: bool) -> BlobMeta {
    BlobMeta { oid: blob, size: u64::from(line_count) * 10, line_count, is_text: true, content_stored }
}

/// Exported symbol whose name sits on its first line, column 6.
fn sym(ord: u32, name: &str, kind: SymbolKind, (start, end): (u32, u32)) -> SymbolDef {
    SymbolDef {
        ord,
        name: name.into(),
        qualified: name.into(),
        kind,
        container: None,
        span: lines(start, end),
        name_pos: (start, 6),
        signature: None,
        doc: None,
        exported: true,
    }
}

fn func(ord: u32, name: &str, span: (u32, u32)) -> SymbolDef {
    sym(ord, name, SymbolKind::Function, span)
}

fn reference(ord: u32, name: &str, kind: RefKind, pos: (u32, u32), enclosing: Option<u32>) -> NameRef {
    NameRef { ord, name: name.into(), qualifier: None, kind, pos, enclosing }
}

fn parsed(symbols: Vec<SymbolDef>, refs: Vec<NameRef>) -> Result<ParsedBlob, String> {
    Ok(ParsedBlob { lang: Language::Go, status: StageStatus::Ok, symbols, refs, literals: Vec::new() })
}

fn literal(ord: u32, text: &str) -> LiteralSite {
    LiteralSite {
        ord,
        parts: vec![LitPart::Text(text.into())],
        pos: (ord + 1, 1),
        enclosing: None,
        context: LiteralContext {
            callee: Some("errors.New".into()),
            arg_index: Some(0),
            siblings: Vec::new(),
            construct: Construct::Call,
            exception_type: None,
            map_key: None,
            attribute: None,
        },
    }
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| (*item).to_owned()).collect()
}

fn site(ord: u32, template: &str, pattern: &str, tokens: &[&str], specificity: u32, line: u32) -> ErrorSiteRecord {
    ErrorSiteRecord {
        ord,
        literal_ord: ord,
        kind: ErrorSiteKind::Throw,
        level: None,
        template: template.into(),
        pattern: pattern.into(),
        tokens: strings(tokens),
        specificity,
        pos: (line, 1),
        enclosing: None,
        http_status: None,
        i18n_key: None,
    }
}

/// `connection {} not found`: 18 literal chars, three tokens.
fn conn_site(ord: u32) -> ErrorSiteRecord {
    site(ord, "connection {} not found", r"\bconnection\s+.+?\s+not\s+found\b", &["connection", "found", "not"], 18, 10)
}

fn i18n_site(ord: u32, key: &str, pos: (u32, u32)) -> ErrorSiteRecord {
    ErrorSiteRecord { kind: ErrorSiteKind::I18nUse, i18n_key: Some(key.into()), pos, ..site(ord, "", "", &[], 0, pos.0) }
}

fn catalog_entry(key: &str, template: &str, pattern: &str, tokens: &[&str], specificity: u32, line: u32) -> CatalogEntryRecord {
    CatalogEntryRecord {
        key: key.into(),
        locale: Some("en".into()),
        template: template.into(),
        pattern: pattern.into(),
        tokens: strings(tokens),
        specificity,
        line,
    }
}

fn query(text: &str, tokens: &[&str], limit: u32) -> ErrorQuery {
    ErrorQuery { segments: vec![text.to_owned()], tokens: strings(tokens), max_candidates: 200, limit, include_low_specificity: false }
}

fn symbol_query(name: &str, mode: NameMatch) -> SymbolQuery {
    SymbolQuery { name: name.into(), qualifier: None, mode, kinds: Vec::new(), lang: None, path_prefix: None, include_tests: true }
}

fn exact(name: &str) -> SymbolQuery {
    symbol_query(name, NameMatch::Exact)
}

fn table_fact(ord: u32, table: &str, line: u32) -> AppFactRecord {
    AppFactRecord {
        ord,
        framework: FrameworkId::SqlStrings,
        fact: AppFact::TableRef { table: table.into(), op: TableOp::Select },
        line,
        enclosing: None,
    }
}

fn flag_fact(ord: u32, key: &str, line: u32) -> AppFactRecord {
    AppFactRecord { ord, framework: FrameworkId::GoNetHttp, fact: AppFact::FlagRead { key: key.into() }, line, enclosing: None }
}

fn node(id: u32, kind: AppNodeKind, key: &str, at: Option<(&str, u32)>, symbol: Option<SymbolRef>) -> AppNode {
    AppNode { id, kind, key: key.into(), label: key.into(), framework: None, at: at.map(|(p, line)| (path(p), line)), symbol }
}

fn edge(from: u32, to: u32, kind: AppEdgeKind) -> AppEdge {
    AppEdge { from, to, kind, confidence: 0.9, evidence: None }
}

const RUN: &str = "scip-go gomod example v1 svc/Run().";
const HELPER: &str = "scip-go gomod example v1 svc/Helper().";
const MAIN: &str = "scip-go gomod example v1 svc/Main().";
const OUTER: &str = "scip-go gomod example v1 svc/outer.";

fn occ(symbol: &str, (line, start_col, end_col): (u32, u32, u32), roles: u32) -> ScipOccurrence {
    ScipOccurrence {
        range: ScipRange { start_line: line, start_col, end_line: line, end_col },
        symbol: symbol.into(),
        roles: ScipRoles(roles),
        enclosing: None,
    }
}

fn scip_meta() -> ScipMetadata {
    ScipMetadata { tool_name: "scip-go".into(), tool_version: "0.1.0".into(), project_root: "file:///src".into() }
}

fn scip_symbols() -> Vec<ScipSymbolInfo> {
    vec![ScipSymbolInfo {
        symbol: HELPER.into(),
        display_name: Some("Helper".into()),
        kind: Some("Function".into()),
        documentation: None,
        relationships: Vec::new(),
    }]
}

/// `svc/a.go` (blob `a`): `Run` (lines 1-10) calls `Helper` (12-20) on line 3; `svc/b.go` (blob
/// `b`): imports `Helper` on line 1, `Main` (7-12) calls `Helper` on line 9 and `Run` on line 10.
fn scip_docs(a: BlobOid, b: BlobOid) -> Vec<ScipDocRows> {
    let def = ScipRoles::DEFINITION;
    let read = ScipRoles::READ_ACCESS;
    vec![
        ScipDocRows {
            path: path("svc/a.go"),
            blob: a,
            occurrences: vec![
                occ(RUN, (1, 5, 8), def),
                occ(HELPER, (3, 1, 7), read),
                occ(HELPER, (12, 5, 11), def),
                occ("local 0", (13, 1, 2), def),
                occ("local 0", (14, 1, 2), read),
                occ(OUTER, (3, 0, 30), 0),
            ],
        },
        ScipDocRows {
            path: path("svc/b.go"),
            blob: b,
            occurrences: vec![
                occ(HELPER, (1, 0, 20), ScipRoles::IMPORT),
                occ(MAIN, (7, 5, 9), def),
                occ(HELPER, (9, 1, 7), 0),
                occ(RUN, (10, 1, 4), 0),
            ],
        },
    ]
}

/// `Run` / `Helper` in blob `a`, `Main` in blob `b`, as in [`scip_docs`].
fn call_blobs(a: BlobOid, b: BlobOid) -> Vec<(BlobOid, Result<ParsedBlob, String>)> {
    vec![
        (a, parsed(vec![func(0, "Run", (1, 10)), func(1, "Helper", (12, 20))], Vec::new())),
        (b, parsed(vec![func(0, "Main", (7, 12))], Vec::new())),
    ]
}

fn paths_of(entries: &[ManifestEntry]) -> Vec<String> {
    entries.iter().map(|entry| entry.path.to_string()).collect()
}

fn located(hits: &[SymbolHit]) -> Vec<String> {
    hits.iter().map(|hit| format!("{}:{}", hit.at.path, hit.def.name)).collect()
}

// ---------------------------------------------------------------------------------------------
// Scenarios: files, symbols

async fn paging_rejects_limit_zero(env: Env) {
    let o = org(1);
    let key = env.version(o, 1, sha(1), Some(revs(1))).await;
    env.manifest(o, &key, &[file("a.go", oid(1))]).await;
    let zero = page(0);
    let k = &env.k;
    let refs = NameRefQuery { name: "a".into(), qualifier: None, kinds: Vec::new(), include_tests: true };
    assert_invalid(k.manifest_all(o, &key, &zero).await, "manifest_all with limit 0");
    assert_invalid(k.list_dir(o, &key, None, &zero).await, "list_dir with limit 0");
    assert_invalid(k.find_paths(o, &key, &PathMatch::Substring("a".into()), &zero).await, "find_paths with limit 0");
    assert_invalid(k.find_symbols(o, &key, &symbol_query("a", NameMatch::Prefix), &zero).await, "find_symbols with limit 0");
    assert_invalid(k.name_refs(o, &key, &refs, &zero).await, "name_refs with limit 0");
    assert_invalid(k.scip_occurrences(o, art(1), &key, HELPER, true, &zero).await, "scip_occurrences with limit 0");
    assert_invalid(k.sites_for_i18n_key(o, &key, ErrorsRev(1), "errors.x", &zero).await, "sites_for_i18n_key with limit 0");
    assert_invalid(k.app_nodes(o, &key, &[], &zero).await, "app_nodes with limit 0");
    assert_invalid(k.manifest_diff(o, &key, &key, None, &zero).await, "manifest_diff with limit 0");
    let one = k.manifest_all(o, &key, &page(1)).await.must("manifest_all");
    assert_eq!((paths_of(&one.items), one.next), (vec!["a.go".to_owned()], None), "no cursor after the last item");
}

async fn tenants_never_see_each_other(env: Env) {
    let (a, b) = (org(1), org(2));
    let ka = env.version(a, 1, sha(1), Some(revs(1))).await;
    let kb = env.version(b, 1, sha(1), Some(revs(1))).await;
    let shared = oid(7);
    for (o, key, name, line_count) in [(a, ka, "AlphaOnly", 11), (b, kb, "BetaOnly", 22)] {
        env.manifest(o, &key, &[file("main.go", shared)]).await;
        env.k.put_blob_meta(o, &[meta(shared, line_count, true)]).await.must("put_blob_meta");
        env.parse(o, 1, vec![(shared, parsed(vec![func(0, name, (1, 3))], Vec::new()))]).await;
    }
    env.sites(a, 1, vec![(shared, vec![conn_site(0)])]).await;

    assert_eq!(env.k.blob_meta(a, &[shared]).await.must("blob_meta"), vec![meta(shared, 11, true)], "same oid, per-org blob rows");
    assert_eq!(env.k.blob_meta(b, &[shared]).await.must("blob_meta"), vec![meta(shared, 22, true)], "same oid, per-org blob rows");
    assert_eq!(env.names_in(a, &ka, "main.go").await, ["AlphaOnly"], "org a reads its own parse of the shared oid");
    assert_eq!(env.names_in(b, &kb, "main.go").await, ["BetaOnly"], "org b reads its own parse of the shared oid");
    assert!(env.find(a, &ka, &exact("BetaOnly")).await.is_empty(), "org b's symbol is invisible to org a");
    let stage = BlobStage::ErrorSites(ErrorsRev(1));
    assert_eq!(env.missing(b, stage, &[shared]).await, [shared], "org a's marker does not count for org b");
    assert!(env.missing(a, stage, &[shared]).await.is_empty());
    let q = query("connection pg-prod not found", &["connection", "found", "not", "pg", "prod"], 10);
    assert_eq!(env.k.match_errors(a, &ka, ErrorsRev(1), &q).await.must("match_errors").len(), 1);
    assert!(env.k.match_errors(b, &kb, ErrorsRev(1), &q).await.must("match_errors").is_empty(), "org a's error sites stay in org a");

    // Org b holding org a's version key sees nothing of it.
    assert!(env.k.manifest_all(b, &ka, &page(ALL)).await.must("manifest_all").items.is_empty(), "manifest across orgs");
    assert!(env.names_in(b, &ka, "main.go").await.is_empty(), "symbols across orgs");
    assert!(env.k.match_errors(b, &ka, ErrorsRev(1), &q).await.must("match_errors").is_empty(), "error sites across orgs");
    assert_eq!(env.k.version_stats(b, &ka).await.must("version_stats").files, 0, "stats across orgs");
}

async fn a_version_sees_the_facts_of_its_blobs(env: Env) {
    let o = org(1);
    let v1 = env.version(o, 1, sha(1), Some(revs(1))).await;
    let v2 = env.version(o, 1, sha(2), Some(revs(1))).await;
    let (x, y, z) = (oid(1), oid(2), oid(3));
    env.manifest(o, &v1, &[file("a.go", x), file("b.go", y)]).await;
    env.manifest(o, &v2, &[file("a.go", x), file("b.go", z), file("c.go", x)]).await;
    let one = |name: &str| parsed(vec![func(0, name, (1, 5))], Vec::new());
    env.parse(o, 1, vec![(x, one("Shared")), (y, one("OldOnly")), (z, one("NewOnly"))]).await;
    let at = |hits: Vec<SymbolHit>| hits.into_iter().map(|hit| (hit.at.path.to_string(), hit.at.blob)).collect::<Vec<_>>();
    assert_eq!(
        at(env.find(o, &v2, &exact("Shared")).await),
        [("a.go".to_owned(), x), ("c.go".to_owned(), x)],
        "a blob at two paths shows at both"
    );
    assert_eq!(at(env.find(o, &v1, &exact("Shared")).await), [("a.go".to_owned(), x)]);
    assert_eq!(at(env.find(o, &v1, &exact("OldOnly")).await), [("b.go".to_owned(), y)]);
    assert!(env.find(o, &v2, &exact("OldOnly")).await.is_empty(), "the old blob of a changed file is not in the new version");
    assert_eq!(at(env.find(o, &v2, &exact("NewOnly")).await), [("b.go".to_owned(), z)]);
    assert!(env.find(o, &v1, &exact("NewOnly")).await.is_empty(), "the new blob is not in the old version");
}

async fn reads_follow_the_version_revs(env: Env) {
    let o = org(1);
    let v = env.version(o, 1, sha(1), Some(revs(1))).await;
    let bare = env.version(o, 1, sha(2), None).await;
    let x = oid(1);
    env.manifest(o, &v, &[file("a.go", x)]).await;
    env.manifest(o, &bare, &[file("a.go", x)]).await;
    env.parse(o, 1, vec![(x, parsed(vec![func(0, "Old", (1, 5))], Vec::new()))]).await;
    env.parse(o, 2, vec![(x, parsed(vec![func(0, "New", (1, 5)), func(1, "Extra", (6, 9))], Vec::new()))]).await;
    env.sites(o, 1, vec![(x, vec![conn_site(0)])]).await;

    assert_eq!(env.names_in(o, &v, "a.go").await, ["Old"]);
    let counts = env.k.version_stats(o, &v).await.must("version_stats").counts;
    assert_eq!((counts.symbols, counts.error_sites), (1, 1), "stats count facts at the version's revs");

    env.set_revs(o, v, revs(2)).await;
    assert_eq!(env.names_in(o, &v, "a.go").await, ["New", "Extra"], "rev 2 facts after set_index_revs");
    assert!(env.find(o, &v, &exact("Old")).await.is_empty(), "rev 1 facts are invisible at rev 2");
    let counts = env.k.version_stats(o, &v).await.must("version_stats").counts;
    assert_eq!((counts.symbols, counts.error_sites), (2, 0), "no error sites at errors rev 2");

    env.set_revs(o, v, revs(1)).await;
    assert_eq!(env.names_in(o, &v, "a.go").await, ["Old"], "switching back shows rev 1 again");

    assert!(env.names_in(o, &bare, "a.go").await.is_empty(), "a version without revs shows no symbols");
    assert!(env.find(o, &bare, &exact("Old")).await.is_empty(), "a version without revs shows no symbols");
    assert!(env.k.symbols_at(o, &bare, &path("a.go"), 2).await.must("symbols_at").is_empty());
    let stats = env.k.version_stats(o, &bare).await.must("version_stats");
    assert_eq!((stats.files, stats.counts), (1, FactCounts::default()), "the manifest is visible without revs, facts are not");
}

async fn puts_are_write_once(env: Env) {
    let o = org(1);
    let v = env.version(o, 1, sha(1), Some(revs(1))).await;
    let (x, y, z, w) = (oid(1), oid(2), oid(3), oid(4));
    let catalog_file = entry("w.json", w, Some(Language::Json), FileClass::Data, 10);
    env.manifest(o, &v, &[file("x.go", x), file("y.go", y), file("z.go", z), catalog_file]).await;

    env.k.put_blob_meta(o, &[meta(x, 5, true), meta(x, 9, false)]).await.must("put_blob_meta");
    env.k.put_blob_meta(o, &[meta(x, 7, false)]).await.must("put_blob_meta");
    assert_eq!(env.k.blob_meta(o, &[x]).await.must("blob_meta"), vec![meta(x, 5, true)], "the first blob row wins");

    let one = |name: &str| parsed(vec![func(0, name, (1, 2))], Vec::new());
    env.parse(o, 1, vec![(x, one("First"))]).await;
    env.parse(o, 1, vec![(x, one("Second")), (x, Err("late failure".into()))]).await;
    env.parse(o, 1, vec![(y, one("Kept")), (y, one("Dropped"))]).await;
    assert_eq!(env.names_in(o, &v, "x.go").await, ["First"], "a repeated put_parsed is a no-op");
    assert_eq!(env.names_in(o, &v, "y.go").await, ["Kept"], "within one call the first row wins");
    assert_eq!(env.k.literal_sites(o, ParserRev(1), &[x]).await.must("literal_sites").len(), 1, "a later Err keeps the first Ok marker");

    env.sites(o, 1, vec![(x, vec![i18n_site(0, "errors.first", (3, 1))])]).await;
    env.sites(o, 1, vec![(x, vec![i18n_site(0, "errors.second", (3, 1))])]).await;
    let i18n = |key: &'static str| {
        let k = env.k.clone();
        async move { k.sites_for_i18n_key(o, &v, ErrorsRev(1), key, &page(ALL)).await.must("sites_for_i18n_key").items }
    };
    assert_eq!(i18n("errors.first").await.len(), 1);
    assert!(i18n("errors.second").await.is_empty(), "a repeated put_error_sites is a no-op");

    let denied = catalog_entry("errors.denied", "Access denied", r"\bAccess\s+denied\b", &["access", "denied"], 12, 1);
    let refused = catalog_entry("errors.denied", "Permission refused", r"\bPermission\s+refused\b", &["permission", "refused"], 17, 1);
    env.k.put_catalog(o, ErrorsRev(1), &[(w, vec![denied.clone()])]).await.must("put_catalog");
    env.k.put_catalog(o, ErrorsRev(1), &[(w, vec![refused])]).await.must("put_catalog");
    let first = env.k.match_catalog(o, &v, ErrorsRev(1), &query("Access denied", &["access", "denied"], 10)).await.must("match_catalog");
    assert_eq!(first.into_iter().map(|m| m.entry).collect::<Vec<_>>(), [denied], "the first catalog row wins");
    let second = env.k.match_catalog(o, &v, ErrorsRev(1), &query("Permission refused", &["permission", "refused"], 10)).await;
    assert!(second.must("match_catalog").is_empty(), "a repeated put_catalog is a no-op");

    env.k.put_app_facts(o, AppMapRev(1), &[(x, vec![table_fact(0, "users", 4)])]).await.must("put_app_facts");
    env.k.put_app_facts(o, AppMapRev(1), &[(x, vec![table_fact(0, "orders", 4)])]).await.must("put_app_facts");
    let facts = env.k.app_facts_at(o, &v, AppMapRev(1)).await.must("app_facts_at");
    assert_eq!(facts.into_iter().map(|at| at.fact).collect::<Vec<_>>(), [table_fact(0, "users", 4)], "a repeated put_app_facts is a no-op");

    env.parse(o, 1, vec![(z, parsed(Vec::new(), Vec::new()))]).await;
    env.sites(o, 1, vec![(z, Vec::new())]).await;
    env.k.put_app_facts(o, AppMapRev(1), &[(z, Vec::new())]).await.must("put_app_facts");
    for stage in [BlobStage::Parse(ParserRev(1)), BlobStage::ErrorSites(ErrorsRev(1)), BlobStage::AppFacts(AppMapRev(1))] {
        assert!(env.missing(o, stage, &[z]).await.is_empty(), "{stage:?} marker is written for an empty result");
    }
    assert!(env.missing(o, BlobStage::ErrorSites(ErrorsRev(1)), &[w]).await.is_empty(), "put_catalog writes the ErrorSites marker");
}

async fn missing_and_blob_meta_are_by_oid(env: Env) {
    let o = org(1);
    env.parse(o, 1, vec![(oid(3), parsed(Vec::new(), Vec::new())), (oid(1), Err("too big".into()))]).await;
    let parse1 = BlobStage::Parse(ParserRev(1));
    assert_eq!(env.missing(o, parse1, &[oid(4), oid(3), oid(2), oid(1), oid(2)]).await, [oid(2), oid(4)], "ordered by oid, deduplicated");
    assert_eq!(env.missing(o, BlobStage::Parse(ParserRev(2)), &[oid(3), oid(1)]).await, [oid(1), oid(3)], "markers are per rev");
    assert_eq!(env.missing(o, BlobStage::ErrorSites(ErrorsRev(1)), &[oid(1)]).await, [oid(1)], "markers are per stage");
    env.k.put_blob_meta(o, &[meta(oid(3), 1, true), meta(oid(1), 1, true)]).await.must("put_blob_meta");
    assert_eq!(env.meta_oids(o, &[oid(3), oid(2), oid(1), oid(3)]).await, [oid(1), oid(3)], "ordered by oid, deduplicated");
}

async fn literal_sites_skip_failed_parses(env: Env) {
    let o = org(1);
    let ts = ParsedBlob {
        lang: Language::TypeScript,
        status: StageStatus::Partial { error_nodes: 2 },
        symbols: Vec::new(),
        refs: Vec::new(),
        literals: vec![literal(1, "second literal"), literal(0, "first literal")],
    };
    let py = ParsedBlob { lang: Language::Python, status: StageStatus::Ok, symbols: Vec::new(), refs: Vec::new(), literals: Vec::new() };
    env.parse(o, 1, vec![(oid(2), Ok(ts)), (oid(1), Err("timeout".into())), (oid(4), Ok(py))]).await;
    let got = env.k.literal_sites(o, ParserRev(1), &[oid(4), oid(3), oid(2), oid(1), oid(2)]).await.must("literal_sites");
    assert_eq!(
        got,
        vec![
            (oid(2), Language::TypeScript, vec![literal(0, "first literal"), literal(1, "second literal")]),
            (oid(4), Language::Python, Vec::new()),
        ],
        "non-failed parses by oid, sites by ord; failed and unparsed oids are left out"
    );
    assert!(env.k.literal_sites(o, ParserRev(2), &[oid(2)]).await.must("literal_sites").is_empty(), "literal sites are per rev");
    assert!(env.missing(o, BlobStage::Parse(ParserRev(1)), &[oid(1)]).await.is_empty(), "a failed parse still writes its marker");
}

/// Files of the manifest scenarios, by path.
fn tree() -> Vec<ManifestEntry> {
    vec![
        entry("README.md", oid(1), Some(Language::Markdown), FileClass::Doc, 5),
        entry("cmd/Server.go", oid(2), Some(Language::Go), FileClass::Source, 20),
        ManifestEntry { kind: EntryKind::Symlink, ..entry("docs/link", oid(3), None, FileClass::Doc, 9) },
        entry("pkg/strings.go", oid(4), Some(Language::Go), FileClass::Source, 10),
        entry("src/main.go", oid(5), Some(Language::Go), FileClass::Source, 100),
        entry("src/util/strings.go", oid(6), Some(Language::Go), FileClass::Source, 50),
        entry("src/util/strings_test.go", oid(7), Some(Language::Go), FileClass::Test, 40),
        ManifestEntry { kind: EntryKind::Submodule, is_text: true, line_count: 9, ..entry("vendor", oid(8), None, FileClass::Vendored, 0) },
        entry("web/app.ts", oid(9), Some(Language::TypeScript), FileClass::Source, 30),
    ]
}

/// Blob rows of [`tree`]; the submodule has none.
fn tree_metas() -> Vec<BlobMeta> {
    [(1, 3), (2, 12), (3, 1), (4, 6), (5, 40), (6, 20), (7, 10), (9, 15)].into_iter().map(|(n, lines)| meta(oid(n), lines, true)).collect()
}

fn dir_entry(name: &str, kind: DirEntryKind, size: Option<u64>, lang: Option<Language>) -> DirEntry {
    DirEntry { name: name.into(), kind, size, lang }
}

async fn manifest_reads(env: Env) {
    let o = org(1);
    let v = env.version(o, 1, sha(1), None).await;
    let mut shuffled = tree();
    shuffled.reverse();
    env.manifest(o, &v, &shuffled).await;
    env.k.put_blob_meta(o, &tree_metas()).await.must("put_blob_meta");

    assert_invalid(env.k.put_manifest(o, &v, &[file("a.go", oid(1)), file("a.go", oid(2))]).await, "a duplicate manifest path");
    let all = env.k.manifest_all(o, &v, &page(ALL)).await.must("manifest_all").items;
    assert_eq!(paths_of(&all), paths_of(&tree()), "ordered by path; a rejected put_manifest changes nothing");

    let wanted = [path("src/main.go"), path("missing.go"), path("vendor"), path("src/main.go")];
    let got = env.k.manifest_entries(o, &v, &wanted).await.must("manifest_entries");
    let main = ManifestEntry { is_text: true, line_count: 40, ..entry("src/main.go", oid(5), Some(Language::Go), FileClass::Source, 100) };
    let vendor = ManifestEntry { kind: EntryKind::Submodule, ..entry("vendor", oid(8), None, FileClass::Vendored, 0) };
    assert_eq!(got, vec![main.clone(), vendor], "existing paths by path; is_text / line_count from the blob row (false / 0 without one)");

    let k = env.k.clone();
    let paged = all_pages(2, move |p| {
        let k = k.clone();
        async move { k.manifest_all(o, &v, &p).await }
    })
    .await;
    assert_eq!(paged, all, "concatenated manifest_all pages equal one unpaged read");

    env.manifest(o, &v, &[file("only.go", oid(5))]).await;
    assert_eq!(paths_of(&env.k.manifest_all(o, &v, &page(ALL)).await.must("manifest_all").items), ["only.go"], "put_manifest replaces");
    env.manifest(o, &v, &tree()).await;

    let list = |dir: Option<&'static str>| {
        let k = env.k.clone();
        async move { k.list_dir(o, &v, dir.map(path).as_ref(), &page(ALL)).await.must("list_dir").items }
    };
    let root = vec![
        dir_entry("README.md", DirEntryKind::File, Some(5), Some(Language::Markdown)),
        dir_entry("cmd", DirEntryKind::Dir, None, None),
        dir_entry("docs", DirEntryKind::Dir, None, None),
        dir_entry("pkg", DirEntryKind::Dir, None, None),
        dir_entry("src", DirEntryKind::Dir, None, None),
        dir_entry("vendor", DirEntryKind::Submodule, Some(0), None),
        dir_entry("web", DirEntryKind::Dir, None, None),
    ];
    assert_eq!(list(None).await, root, "root children by name; directories implied by deeper paths");
    assert_eq!(
        list(Some("src")).await,
        vec![dir_entry("main.go", DirEntryKind::File, Some(100), Some(Language::Go)), dir_entry("util", DirEntryKind::Dir, None, None)]
    );
    assert_eq!(list(Some("src/util")).await.into_iter().map(|e| e.name).collect::<Vec<_>>(), ["strings.go", "strings_test.go"]);
    assert_eq!(list(Some("docs")).await, vec![dir_entry("link", DirEntryKind::Symlink, Some(9), None)]);
    assert!(list(Some("nope")).await.is_empty());
    let k = env.k.clone();
    let paged = all_pages(3, move |p| {
        let k = k.clone();
        async move { k.list_dir(o, &v, None, &p).await }
    })
    .await;
    assert_eq!(paged, root, "concatenated list_dir pages equal one unpaged read");

    let find = |m: PathMatch| {
        let k = env.k.clone();
        async move { k.find_paths(o, &v, &m, &page(ALL)).await.must("find_paths").items }
    };
    assert_eq!(find(PathMatch::Substring("MAIN".into())).await, vec![main], "case-insensitive substring; meta read back");
    assert_eq!(paths_of(&find(PathMatch::Substring("server".into())).await), ["cmd/Server.go"]);
    let go_under_src = find(PathMatch::Glob("src/**/*.go".into())).await;
    assert_eq!(paths_of(&go_under_src), ["src/main.go", "src/util/strings.go", "src/util/strings_test.go"]);
    assert_eq!(paths_of(&find(PathMatch::Glob("*.md".into())).await), ["README.md"], "`*` stays within one segment");
    assert_invalid(env.k.find_paths(o, &v, &PathMatch::Glob("/abs/*.go".into()), &page(ALL)).await, "an unparsable glob");
    let k = env.k.clone();
    let paged = all_pages(2, move |p| {
        let k = k.clone();
        async move { k.find_paths(o, &v, &PathMatch::Substring("s".into()), &p).await }
    })
    .await;
    assert_eq!(paged, find(PathMatch::Substring("s".into())).await, "concatenated find_paths pages equal one unpaged read");

    let basename = |name: &'static str, limit: u16| {
        let k = env.k.clone();
        async move { k.paths_by_basename(o, &v, name, limit).await.must("paths_by_basename") }
    };
    assert_eq!(basename("strings.go", 10).await, [path("pkg/strings.go"), path("src/util/strings.go")], "by path length, then path");
    assert_eq!(basename("strings.go", 1).await, [path("pkg/strings.go")], "at most `limit`");
    assert!(basename("nope.go", 10).await.is_empty());
}

async fn version_stats_summarize_the_version(env: Env) {
    let o = org(1);
    let v = env.version(o, 1, sha(1), Some(revs(1))).await;
    env.manifest(o, &v, &tree()).await;
    env.k.put_blob_meta(o, &tree_metas()).await.must("put_blob_meta");
    let main = parsed(vec![func(0, "main", (1, 10)), func(1, "run", (11, 20))], Vec::new());
    env.parse(o, 1, vec![(oid(5), main), (oid(6), parsed(vec![func(0, "Trim", (1, 5))], Vec::new()))]).await;
    env.parse(o, 2, vec![(oid(2), parsed(vec![func(0, "Serve", (1, 5))], Vec::new()))]).await;
    env.sites(o, 1, vec![(oid(5), vec![conn_site(0)])]).await;
    env.sites(o, 2, vec![(oid(6), vec![conn_site(0)])]).await;
    let nodes = vec![
        node(1, AppNodeKind::Endpoint, "GET /api/a", None, None),
        node(2, AppNodeKind::Endpoint, "GET /api/b", None, None),
        node(3, AppNodeKind::Page, "/a", None, None),
        node(4, AppNodeKind::Table, "a", None, None),
        node(5, AppNodeKind::Handler, "A", None, None),
    ];
    env.graph(o, &v, nodes, Vec::new()).await;
    assert_eq!(
        env.k.version_stats(o, &v).await.must("version_stats"),
        VersionStats {
            files: 7,
            bytes: 255,
            by_lang: vec![(Language::Go, 5, 88), (Language::TypeScript, 1, 15), (Language::Markdown, 1, 3)],
            top_dirs: vec![(path("src"), 3), (path("cmd"), 1), (path("pkg"), 1), (path("web"), 1)],
            counts: FactCounts { symbols: 3, error_sites: 1, endpoints: 2, pages: 1, tables: 1 },
        },
        "files only (no symlink / submodule), languages by files then language, dirs by count then path, facts at the version's revs"
    );
}

async fn find_symbols_match_rank_and_page(env: Env) {
    let o = org(1);
    let v = env.version(o, 1, sha(1), Some(revs(1))).await;
    let files = [
        file("a/conn.go", oid(1)),
        file("b/conn.go", oid(2)),
        entry("b/conn_test.go", oid(3), Some(Language::Go), FileClass::Test, 10),
        entry("web/conn.ts", oid(4), Some(Language::TypeScript), FileClass::Source, 10),
        file("x/parse.go", oid(5)),
    ];
    env.manifest(o, &v, &files).await;
    let def = |ord: u32, name: &str, qualified: &str, kind: SymbolKind, exported: bool| SymbolDef {
        qualified: qualified.into(),
        exported,
        ..sym(ord, name, kind, (ord * 10 + 1, ord * 10 + 5))
    };
    let (function, method) = (SymbolKind::Function, SymbolKind::Method);
    env.parse(
        o,
        1,
        vec![
            (
                oid(1),
                parsed(
                    vec![def(0, "Connect", "a.Connect", function, true), def(1, "connectAll", "a.connectAll", function, false)],
                    Vec::new(),
                ),
            ),
            (oid(2), parsed(vec![def(0, "Connect", "Server.Connect", method, false)], Vec::new())),
            (oid(3), parsed(vec![def(0, "Connect", "b.Connect", function, true)], Vec::new())),
            (
                oid(4),
                parsed(
                    vec![
                        def(0, "Connection", "Connection", SymbolKind::Class, true),
                        def(1, "CONNECT", "CONNECT", SymbolKind::Constant, false),
                    ],
                    Vec::new(),
                ),
            ),
            (oid(5), parsed(vec![def(0, "parse", "x.parse", function, true)], Vec::new())),
        ],
    )
    .await;
    let names = ["Connect", "connectAll", "Connect", "Connect", "Connection", "CONNECT", "parse"];

    let hits = env.find(o, &v, &exact("Connect")).await;
    assert_eq!(located(&hits), ["a/conn.go:Connect", "b/conn_test.go:Connect", "b/conn.go:Connect"], "exported first, then non-test");
    assert!(hits.iter().all(|hit| hit.score == 1.0), "an exact name scores 1.0");
    assert_eq!(hits[0].at, sref("a/conn.go", oid(1), 0));
    assert!(env.find(o, &v, &exact("connect")).await.is_empty(), "Exact is case-sensitive");

    let by_qualifier = |qualifier: &str| SymbolQuery { qualifier: Some(qualifier.into()), ..exact("Connect") };
    assert_eq!(
        located(&env.find(o, &v, &by_qualifier("b")).await),
        ["b/conn_test.go:Connect", "b/conn.go:Connect", "a/conn.go:Connect"],
        "a qualifier segment or the directory name ranks first"
    );
    assert_eq!(
        located(&env.find(o, &v, &by_qualifier("Server")).await),
        ["b/conn.go:Connect", "a/conn.go:Connect", "b/conn_test.go:Connect"],
        "qualifier `Server` is a segment of `Server.Connect`"
    );

    let no_tests = SymbolQuery { include_tests: false, ..exact("Connect") };
    assert_eq!(located(&env.find(o, &v, &no_tests).await), ["a/conn.go:Connect", "b/conn.go:Connect"], "include_tests = false");
    let methods = SymbolQuery { kinds: vec![SymbolKind::Method], ..exact("Connect") };
    assert_eq!(located(&env.find(o, &v, &methods).await), ["b/conn.go:Connect"], "kinds filter");
    let under_a = SymbolQuery { path_prefix: Some(path("a")), ..exact("Connect") };
    assert_eq!(located(&env.find(o, &v, &under_a).await), ["a/conn.go:Connect"], "path_prefix filter");

    let insensitive = symbol_query("connect", NameMatch::CaseInsensitive);
    assert_eq!(
        located(&env.find(o, &v, &insensitive).await),
        ["a/conn.go:Connect", "b/conn_test.go:Connect", "b/conn.go:Connect", "web/conn.ts:CONNECT"],
        "case-insensitive equality, then exported, non-test, shorter path"
    );
    let typescript = SymbolQuery { lang: Some(Language::TypeScript), ..insensitive };
    assert_eq!(located(&env.find(o, &v, &typescript).await), ["web/conn.ts:CONNECT"], "lang filter");

    let prefix = symbol_query("conn", NameMatch::Prefix);
    let prefixed = env.find(o, &v, &prefix).await;
    let mut got: Vec<&str> = prefixed.iter().map(|hit| hit.def.name.as_str()).collect();
    got.sort_unstable();
    assert_eq!(got, ["CONNECT", "Connect", "Connect", "Connect", "Connection", "connectAll"], "case-insensitive prefix");
    for pair in prefixed.windows(2) {
        assert!(pair[0].score >= pair[1].score, "ordered by score: {:?}", located(&prefixed));
    }
    for hit in &prefixed {
        assert_close(hit.score, trigram_similarity(&hit.def.name, "conn"), "score is the trigram similarity");
    }

    let fuzzy = env.find(o, &v, &symbol_query("Conect", NameMatch::Fuzzy)).await;
    let mut expected: Vec<&str> = names.into_iter().filter(|name| trigram_similarity(name, "Conect") >= FUZZY_THRESHOLD).collect();
    let mut got: Vec<&str> = fuzzy.iter().map(|hit| hit.def.name.as_str()).collect();
    expected.sort_unstable();
    got.sort_unstable();
    assert_eq!(got, expected, "fuzzy = trigram similarity >= FUZZY_THRESHOLD");
    assert!(got.contains(&"Connect") && !got.contains(&"parse"));
    for pair in fuzzy.windows(2) {
        assert!(pair[0].score >= pair[1].score, "fuzzy hits are ordered by score");
    }

    let k = env.k.clone();
    let paged = all_pages(1, move |p| {
        let (k, q) = (k.clone(), prefix.clone());
        async move { k.find_symbols(o, &v, &q, &p).await }
    })
    .await;
    assert_eq!(paged, prefixed, "concatenated find_symbols pages equal one unpaged read");
}

async fn symbol_lookups_by_position_file_and_ref(env: Env) {
    let o = org(1);
    let v = env.version(o, 1, sha(1), Some(revs(1))).await;
    let (blob, other) = (oid(1), oid(2));
    env.manifest(o, &v, &[file("svc/server.go", blob), file("svc/other.go", other)]).await;
    let closure = SymbolDef { container: Some(1), ..func(2, "func1", (8, 12)) };
    let symbols = vec![
        func(3, "helper", (22, 30)),
        closure.clone(),
        sym(0, "Server", SymbolKind::Struct, (1, 3)),
        sym(4, "Alias", SymbolKind::Constant, (22, 30)),
        sym(1, "Handle", SymbolKind::Method, (5, 20)),
    ];
    env.parse(o, 1, vec![(blob, parsed(symbols, Vec::new()))]).await;
    let at = |p: &'static str, line: u32| {
        let k = env.k.clone();
        async move { k.symbols_at(o, &v, &path(p), line).await.must("symbols_at") }
    };
    let names = |hits: &[SymbolHit]| hits.iter().map(|hit| hit.def.name.clone()).collect::<Vec<_>>();

    let chain = at("svc/server.go", 10).await;
    assert_eq!(names(&chain), ["func1", "Handle"], "innermost first");
    assert_eq!(chain[0].def, closure);
    assert!(chain.iter().all(|hit| hit.score == 1.0 && hit.at.blob == blob && hit.at.path == path("svc/server.go")));
    assert_eq!(names(&at("svc/server.go", 22).await), ["Alias", "helper"], "equal spans: higher ord first");
    assert!(at("svc/server.go", 4).await.is_empty());
    assert!(at("svc/other.go", 10).await.is_empty(), "a file without facts");
    assert!(at("missing.go", 10).await.is_empty());

    let in_file = env.k.symbols_in_file(o, &v, &path("svc/server.go")).await.must("symbols_in_file");
    assert_eq!(in_file.iter().map(|hit| hit.at.ord).collect::<Vec<_>>(), [0, 1, 2, 3, 4], "ordered by ord");

    let refs = [
        sref("svc/server.go", blob, 3),
        sref("svc/server.go", other, 0),
        sref("svc/server.go", blob, 99),
        sref("svc/other.go", blob, 0),
        sref("svc/server.go", blob, 0),
    ];
    let got = env.k.symbols_by_ref(o, &v, &refs).await.must("symbols_by_ref");
    assert_eq!(names(&got), ["helper", "Server"], "valid refs in input order; a wrong blob or an unknown ord is skipped");
    assert_eq!((got[0].at.clone(), got[1].at.clone()), (refs[0].clone(), refs[4].clone()));
}

async fn name_refs_filter_and_order(env: Env) {
    let o = org(1);
    let v = env.version(o, 1, sha(1), Some(revs(1))).await;
    let (main, test) = (oid(1), oid(2));
    let test_file = entry("a/main_test.go", test, Some(Language::Go), FileClass::Test, 10);
    env.manifest(o, &v, &[file("a/main.go", main), test_file, file("b/copy.go", main)]).await;
    let error_ref = |ord: u32, qualifier: Option<&str>, kind: RefKind, pos: (u32, u32), enclosing: Option<u32>| NameRef {
        qualifier: qualifier.map(str::to_owned),
        ..reference(ord, "Error", kind, pos, enclosing)
    };
    let main_refs = vec![
        error_ref(0, Some("log"), RefKind::Call, (5, 2), Some(0)),
        error_ref(1, Some("http"), RefKind::Call, (3, 5), Some(0)),
        error_ref(2, None, RefKind::Type, (2, 1), None),
        reference(3, "Other", RefKind::Call, (4, 1), Some(0)),
    ];
    let test_refs = vec![error_ref(0, Some("http"), RefKind::Call, (4, 1), Some(0))];
    env.parse(
        o,
        1,
        vec![(main, parsed(vec![func(0, "main", (1, 10))], main_refs)), (test, parsed(vec![func(0, "TestMain", (1, 10))], test_refs))],
    )
    .await;
    let base = NameRefQuery { name: "Error".into(), qualifier: None, kinds: Vec::new(), include_tests: true };
    let refs = |q: NameRefQuery| {
        let k = env.k.clone();
        async move { k.name_refs(o, &v, &q, &page(ALL)).await.must("name_refs").items }
    };
    let spots = |hits: &[RefHit]| hits.iter().map(|hit| (hit.path.to_string(), hit.pos)).collect::<Vec<_>>();
    let spot = |p: &str, pos: (u32, u32)| (p.to_owned(), pos);

    let all = refs(base.clone()).await;
    let expected = [
        spot("a/main.go", (2, 1)),
        spot("a/main.go", (3, 5)),
        spot("a/main.go", (5, 2)),
        spot("a/main_test.go", (4, 1)),
        spot("b/copy.go", (2, 1)),
        spot("b/copy.go", (3, 5)),
        spot("b/copy.go", (5, 2)),
    ];
    assert_eq!(spots(&all), expected, "by path, then position; a blob at two paths shows at both");
    assert_eq!(all[1], RefHit { path: path("a/main.go"), pos: (3, 5), kind: RefKind::Call, enclosing: Some(sref("a/main.go", main, 0)) });
    assert_eq!(all[0].enclosing, None);

    let http = refs(NameRefQuery { qualifier: Some("http".into()), ..base.clone() }).await;
    assert_eq!(spots(&http), [spot("a/main.go", (3, 5)), spot("a/main_test.go", (4, 1)), spot("b/copy.go", (3, 5))], "exact qualifier");
    let types = refs(NameRefQuery { kinds: vec![RefKind::Type], ..base.clone() }).await;
    assert_eq!(spots(&types), [spot("a/main.go", (2, 1)), spot("b/copy.go", (2, 1))], "kinds filter");
    let no_tests = refs(NameRefQuery { include_tests: false, ..base.clone() }).await;
    assert!(no_tests.len() == 6 && no_tests.iter().all(|hit| hit.path != path("a/main_test.go")), "include_tests = false");
    assert!(refs(NameRefQuery { name: "Nope".into(), ..base.clone() }).await.is_empty());

    let k = env.k.clone();
    let paged = all_pages(2, move |p| {
        let (k, q) = (k.clone(), base.clone());
        async move { k.name_refs(o, &v, &q, &p).await }
    })
    .await;
    assert_eq!(paged, all, "concatenated name_refs pages equal one unpaged read");
}

fn call(from: &SymbolRef, to: &SymbolRef, sites: &[(&str, u32)]) -> CallEdge {
    CallEdge { from: from.clone(), to: to.clone(), sites: sites.iter().map(|(p, line)| (path(p), *line)).collect() }
}

async fn call_edges_by_name(env: Env) {
    let o = org(1);
    let v = env.version(o, 1, sha(1), Some(revs(1))).await;
    let (a, b, c) = (oid(1), oid(2), oid(3));
    env.manifest(o, &v, &[file("svc/a.go", a), file("svc/b.go", b), file("svc/c.go", c)]).await;
    let a_refs = vec![
        reference(0, "Helper", RefKind::Call, (7, 2), Some(0)),
        reference(1, "Helper", RefKind::Call, (3, 2), Some(0)),
        reference(2, "Log", RefKind::Dispatch, (14, 2), Some(1)),
        reference(3, "Helper", RefKind::Read, (5, 1), Some(0)),
        reference(4, "Helper", RefKind::Call, (25, 1), None),
    ];
    let b_refs = vec![reference(0, "Run", RefKind::Call, (8, 2), Some(1)), reference(1, "Helper", RefKind::Call, (9, 2), Some(1))];
    env.parse(
        o,
        1,
        vec![
            (a, parsed(vec![func(0, "Run", (1, 10)), func(1, "Helper", (12, 20))], a_refs)),
            (b, parsed(vec![func(0, "Log", (1, 5)), func(1, "Main", (7, 12))], b_refs)),
            (c, parsed(vec![func(0, "Log", (1, 3))], Vec::new())),
        ],
    )
    .await;
    let (run, helper, main) = (sref("svc/a.go", a, 0), sref("svc/a.go", a, 1), sref("svc/b.go", b, 1));
    let edges = |from: Vec<SymbolRef>, dir: CallDirection, limit: u32| {
        let k = env.k.clone();
        async move { k.call_edges(o, &v, &from, dir, &Precision::Name, limit).await.must("call_edges") }
    };

    let callers = edges(vec![helper.clone()], CallDirection::Callers, 10).await;
    assert_eq!(
        callers,
        vec![call(&run, &helper, &[("svc/a.go", 3), ("svc/a.go", 7)]), call(&main, &helper, &[("svc/b.go", 9)])],
        "Call/Dispatch refs named like the target, grouped by enclosing symbol, sites in source order"
    );
    assert_eq!(edges(vec![run.clone()], CallDirection::Callees, 10).await, vec![call(&run, &helper, &[("svc/a.go", 3), ("svc/a.go", 7)])]);
    assert_eq!(
        edges(vec![helper.clone()], CallDirection::Callees, 10).await,
        vec![call(&helper, &sref("svc/b.go", b, 0), &[("svc/a.go", 14)]), call(&helper, &sref("svc/c.go", c, 0), &[("svc/a.go", 14)])],
        "a callee name resolves to every def of that name at the version"
    );
    let noisy = vec![helper.clone(), sref("svc/a.go", b, 1), helper.clone()];
    assert_eq!(edges(noisy, CallDirection::Callers, 10).await, callers, "refs invalid at the version are skipped");

    let both = edges(vec![helper.clone(), run.clone()], CallDirection::Callers, 10).await;
    let expected = vec![
        call(&run, &helper, &[("svc/a.go", 3), ("svc/a.go", 7)]),
        call(&main, &run, &[("svc/b.go", 8)]),
        call(&main, &helper, &[("svc/b.go", 9)]),
    ];
    assert_eq!(both, expected, "batched, ordered by (from, to)");
    assert_eq!(edges(vec![helper.clone(), run.clone()], CallDirection::Callers, 2).await, expected[..2], "at most `limit` edges");
}

async fn call_edges_from_scip(env: Env) {
    let o = org(1);
    let v1 = env.version(o, 1, sha(1), Some(revs(1))).await;
    let v2 = env.version(o, 1, sha(2), Some(revs(1))).await;
    let (a, b, b2) = (oid(1), oid(2), oid(3));
    env.manifest(o, &v1, &[file("svc/a.go", a), file("svc/b.go", b)]).await;
    env.manifest(o, &v2, &[file("svc/a.go", a), file("svc/b.go", b2)]).await;
    let mut blobs = call_blobs(a, b);
    blobs.push((b2, parsed(vec![func(0, "Main", (7, 12))], Vec::new())));
    env.parse(o, 1, blobs).await;
    let (run, helper, main) = (sref("svc/a.go", a, 0), sref("svc/a.go", a, 1), sref("svc/b.go", b, 0));
    let edges = |key: RepoVersionKey, from: SymbolRef, dir: CallDirection, precision: Precision| {
        let k = env.k.clone();
        async move { k.call_edges(o, &key, &[from], dir, &precision, 10).await }
    };
    let (callers, callees) = (CallDirection::Callers, CallDirection::Callees);
    let stale = Precision::PreciseStale { from_sha: sha(1) };

    assert_not_found(edges(v1, helper.clone(), callers, Precision::Precise).await, "Precise without an index");
    assert_not_found(edges(v2, helper.clone(), callers, stale.clone()).await, "PreciseStale without an index");

    env.ingest(o, &v1, art(1), &scip_docs(a, b)).await;
    assert_eq!(
        edges(v1, helper.clone(), callers, Precision::Precise).await.must("call_edges"),
        vec![call(&run, &helper, &[("svc/a.go", 3)]), call(&main, &helper, &[("svc/b.go", 9)])],
        "references of the target's SCIP symbol, attributed to their innermost enclosing symbol"
    );
    assert_eq!(
        edges(v1, main.clone(), callees, Precision::Precise).await.must("call_edges"),
        vec![call(&main, &run, &[("svc/b.go", 10)]), call(&main, &helper, &[("svc/b.go", 9)])],
        "references inside the source's span, resolved to the symbol at their definition"
    );
    let local_only = edges(v1, helper.clone(), callees, Precision::Precise).await.must("call_edges");
    assert!(local_only.is_empty(), "document-local symbols are not callees: {local_only:?}");

    assert_not_found(edges(v2, helper.clone(), callers, Precision::Precise).await, "Precise needs an index of this sha");
    assert_eq!(
        edges(v2, helper.clone(), callers, stale).await.must("call_edges"),
        vec![call(&run, &helper, &[("svc/a.go", 3)])],
        "a stale index only uses documents whose blob is unchanged"
    );
    env.k.begin_scip_index(o, &v2, art(2), &scip_meta()).await.must("begin_scip_index");
    env.k.put_scip_documents(o, art(2), &scip_docs(a, b2)).await.must("put_scip_documents");
    assert_not_found(edges(v2, helper.clone(), callers, Precision::Precise).await, "an unfinished index is not used");
}

async fn scip_ingest_and_reads(env: Env) {
    let o = org(1);
    let v1 = env.version(o, 1, sha(1), Some(revs(1))).await;
    let v2 = env.version(o, 1, sha(2), Some(revs(1))).await;
    let (a, b, b2) = (oid(1), oid(2), oid(3));
    env.manifest(o, &v1, &[file("svc/a.go", a), file("svc/b.go", b)]).await;
    env.manifest(o, &v2, &[file("svc/a.go", a), file("svc/b.go", b2)]).await;
    env.parse(o, 1, call_blobs(a, b)).await;
    let (run, helper, main) = (sref("svc/a.go", a, 0), sref("svc/a.go", a, 1), sref("svc/b.go", b, 0));
    let docs = scip_docs(a, b);
    let go = ["go".to_owned()];

    assert_not_found(env.k.put_scip_documents(o, art(1), &docs).await, "put_scip_documents before begin");
    assert_not_found(env.k.put_scip_symbols(o, art(1), &scip_symbols()).await, "put_scip_symbols before begin");
    assert_not_found(env.k.finish_scip_index(o, art(1), &go, 2).await, "finish_scip_index before begin");

    env.k.begin_scip_index(o, &v1, art(1), &scip_meta()).await.must("begin_scip_index");
    env.k.put_scip_documents(o, art(1), &docs).await.must("put_scip_documents");
    env.k.put_scip_symbols(o, art(1), &scip_symbols()).await.must("put_scip_symbols");
    let symbol_at = |artifact: ArtifactId, p: &'static str, line: u32, col: Option<u32>| {
        let k = env.k.clone();
        async move { k.scip_symbol_at(o, artifact, &path(p), line, col).await.must("scip_symbol_at") }
    };
    let occurrences = |artifact: ArtifactId, key: RepoVersionKey, defs: bool| {
        let k = env.k.clone();
        async move { k.scip_occurrences(o, artifact, &key, HELPER, defs, &page(ALL)).await.must("scip_occurrences").items }
    };
    assert_eq!(env.k.scip_index_for(o, &v1).await.must("scip_index_for"), None, "an unfinished index is invisible");
    assert_eq!(symbol_at(art(1), "svc/a.go", 3, Some(2)).await, None, "an unfinished index is invisible");
    assert!(occurrences(art(1), v1, true).await.is_empty(), "an unfinished index is invisible");

    env.k.finish_scip_index(o, art(1), &go, 2).await.must("finish_scip_index");
    let info = ScipIndexInfo { artifact: art(1), sha: sha(1), tool: "scip-go".into(), languages: go.to_vec(), documents: 2 };
    assert_eq!(env.k.scip_index_for(o, &v1).await.must("scip_index_for"), Some(info.clone()));

    let helper_def = Some((path("svc/a.go"), ScipRange { start_line: 12, start_col: 5, end_line: 12, end_col: 11 }));
    let helper_hit = ScipSymbolHit { symbol: HELPER.into(), display_name: Some("Helper".into()), def: helper_def };
    assert_eq!(symbol_at(art(1), "svc/a.go", 3, Some(2)).await, Some(helper_hit.clone()), "the smallest covering range wins");
    assert_eq!(symbol_at(art(1), "svc/a.go", 3, None).await, Some(helper_hit), "no column: any column of the line");
    assert_eq!(
        symbol_at(art(1), "svc/a.go", 3, Some(25)).await,
        Some(ScipSymbolHit { symbol: OUTER.into(), display_name: None, def: None }),
        "a symbol without info and without a definition"
    );
    assert_eq!(symbol_at(art(1), "svc/a.go", 3, Some(30)).await, None, "ranges are half-open on columns");
    assert_eq!(symbol_at(art(1), "svc/a.go", 2, None).await, None);
    assert_eq!(symbol_at(art(1), "missing.go", 3, None).await, None);

    let hit = |p: &str, pos: (u32, u32), kind: RefKind, enclosing: Option<&SymbolRef>| RefHit {
        path: path(p),
        pos,
        kind,
        enclosing: enclosing.cloned(),
    };
    let with_defs = occurrences(art(1), v1, true).await;
    assert_eq!(
        with_defs,
        vec![
            hit("svc/a.go", (3, 1), RefKind::Read, Some(&run)),
            hit("svc/a.go", (12, 5), RefKind::Read, Some(&helper)),
            hit("svc/b.go", (1, 0), RefKind::Import, None),
            hit("svc/b.go", (9, 1), RefKind::Read, Some(&main)),
        ],
        "by path, then position; the Import role reads back as Import; enclosing symbol at the version"
    );
    let without_defs = vec![with_defs[0].clone(), with_defs[2].clone(), with_defs[3].clone()];
    assert_eq!(occurrences(art(1), v1, false).await, without_defs, "defs = false leaves definitions out");
    let k = env.k.clone();
    let paged = all_pages(1, move |p| {
        let k = k.clone();
        async move { k.scip_occurrences(o, art(1), &v1, HELPER, true, &p).await }
    })
    .await;
    assert_eq!(paged, with_defs, "concatenated scip_occurrences pages equal one unpaged read");
    assert_eq!(occurrences(art(1), v2, true).await, with_defs[..2], "documents whose blob changed at the version are left out");
    assert_eq!(env.k.scip_index_for(o, &v2).await.must("scip_index_for"), Some(info), "no index of this sha: the newest of the repo");

    env.clock.advance(Duration::from_secs(60));
    env.k.begin_scip_index(o, &v1, art(1), &scip_meta()).await.must("begin_scip_index");
    assert_eq!(env.k.scip_index_for(o, &v1).await.must("scip_index_for"), None, "a restarted index is ingesting again");
    env.k.finish_scip_index(o, art(1), &go, 0).await.must("finish_scip_index");
    assert!(occurrences(art(1), v1, true).await.is_empty(), "a restart drops the index's rows");
    assert_eq!(symbol_at(art(1), "svc/a.go", 3, None).await, None, "a restart drops the index's rows");

    let artifact_for = |key: RepoVersionKey| {
        let k = env.k.clone();
        async move { k.scip_index_for(o, &key).await.must("scip_index_for").map(|info| info.artifact) }
    };
    env.clock.advance(Duration::from_secs(60));
    env.ingest(o, &v1, art(2), &docs).await;
    assert_eq!(artifact_for(v1).await, Some(art(2)), "the newest index of the sha");
    env.clock.advance(Duration::from_secs(60));
    let v3 = env.version(o, 1, sha(3), Some(revs(1))).await;
    env.ingest(o, &v3, art(3), &docs).await;
    assert_eq!(artifact_for(v1).await, Some(art(2)), "an index of the exact sha beats a newer one");
    assert_eq!(artifact_for(v2).await, Some(art(3)), "otherwise the newest of the repo");
}

// ---------------------------------------------------------------------------------------------
// Scenarios: error sites

fn matched(matches: &[ErrorMatch]) -> Vec<(String, u32, bool)> {
    matches.iter().map(|m| (m.path.to_string(), m.site.ord, m.verified)).collect()
}

fn row(p: &str, ord: u32, verified: bool) -> (String, u32, bool) {
    (p.to_owned(), ord, verified)
}

fn byte_span(text: &str, start: &str, end: &str) -> (u32, u32) {
    let (Some(from), Some(to)) = (text.find(start), text.find(end)) else { panic!("{start:?} / {end:?} not in {text:?}") };
    (from as u32, (to + end.len()) as u32)
}

async fn match_errors_contain_verify_and_fall_back(env: Env) {
    let o = org(1);
    let v = env.version(o, 1, sha(1), Some(revs(1))).await;
    let (conn, auth, web) = (oid(1), oid(2), oid(3));
    let ts = entry("web/err.ts", web, Some(Language::TypeScript), FileClass::Source, 10);
    env.manifest(o, &v, &[file("svc/auth.go", auth), file("svc/conn.go", conn), ts]).await;
    let fetching = |ord| {
        let tokens = ["connection", "failed", "fetching"];
        site(ord, "failed fetching connection {}: {w}", r"\bfailed\s+fetching\s+connection\s+.+?:\s+", &tokens, 25, 20)
    };
    let auth_sites = vec![
        site(0, "denied", r"\bdenied\b", &["denied"], 6, 3),
        site(1, "token {} expired", r"\btoken\s+.+?\s+expired\b", &["expired", "token"], 12, 5),
        site(2, "connection found {}", r"\bconnection\s+found\b", &["connection", "found"], 15, 7),
    ];
    let web_sites = vec![
        site(0, "connection {} not found for zebra", r"\bconnection\b", &["connection", "found", "not", "zebra"], 27, 1),
        site(
            1,
            "retry connection {} not found again later",
            r"\bretry\b",
            &["again", "connection", "found", "later", "not", "retry"],
            34,
            2,
        ),
        site(2, "connection not found (", "(", &["connection", "found", "not"], 19, 3),
    ];
    env.sites(o, 1, vec![(conn, vec![conn_site(0), fetching(1)]), (auth, auth_sites), (web, web_sites), (oid(9), vec![conn_site(0)])])
        .await;
    let lost = site(0, "connection {} lost", r"\bconnection\s+.+?\s+lost\b", &["connection", "lost"], 14, 4);
    env.sites(o, 2, vec![(conn, vec![lost])]).await;
    let run = |rev: u32, q: ErrorQuery| {
        let k = env.k.clone();
        async move { k.match_errors(o, &v, ErrorsRev(rev), &q).await.must("match_errors") }
    };

    let text = r#"level=error msg="connection pg-prod not found" denied"#;
    let q = query(text, &["connection", "denied", "error", "found", "level", "msg", "not", "pg", "prod"], 10);
    let got = run(1, q.clone()).await;
    assert_eq!(
        matched(&got),
        [row("svc/conn.go", 0, true), row("svc/auth.go", 2, false), row("web/err.ts", 2, false), row("web/err.ts", 0, false)],
        "verified first; candidates failing their pattern (or an uncompilable one) fall back; a site with a word missing from the \
         input is never a candidate; low-specificity sites are dropped; overlap 0.5 is too low; sites off the manifest never show"
    );
    let verified = &got[0];
    assert_eq!((verified.blob, &verified.site, verified.segment), (conn, &conn_site(0), 0));
    assert_eq!(verified.span, byte_span(text, "connection", "found"), "span = the match's byte range in the segment");
    assert_close(verified.score, 0.5 + 0.5 * (18.0 / text.len() as f32), "verified score");
    assert_close(got[1].score, 0.4, "fallback score at overlap 1.0");
    assert_close(got[3].score, 0.3, "fallback score at overlap 0.75");
    assert_eq!((got[1].span, got[1].segment), ((0, 0), 0), "an unverified match has no span");

    let low = run(1, ErrorQuery { include_low_specificity: true, ..q.clone() }).await;
    assert_eq!(
        matched(&low),
        [
            row("svc/conn.go", 0, true),
            row("svc/auth.go", 0, true),
            row("svc/auth.go", 2, false),
            row("web/err.ts", 2, false),
            row("web/err.ts", 0, false)
        ],
        "include_low_specificity keeps low-specificity sites; verified ones rank by score"
    );
    assert_eq!(matched(&run(1, ErrorQuery { limit: 1, ..q.clone() }).await), [row("svc/conn.go", 0, true)], "at most `limit`");
    assert_eq!(
        matched(&run(1, ErrorQuery { max_candidates: 1, ..q.clone() }).await),
        [row("svc/auth.go", 2, false), row("svc/conn.go", 0, false), row("web/err.ts", 2, false), row("web/err.ts", 0, false)],
        "only the first `max_candidates` containment candidates (by path, ord) are verified"
    );

    let exact_input = ErrorQuery { segments: vec![" denied ".into()], ..query(" denied ", &["denied"], 10) };
    let got = run(1, exact_input).await;
    assert_eq!(matched(&got), [row("svc/auth.go", 0, true)], "a low-specificity site whose template equals a trimmed segment");
    assert_eq!(got[0].span, (1, 7));
    assert_close(got[0].score, 0.5 + 0.5 * (6.0 / 8.0), "verified score");

    let segments =
        ErrorQuery { segments: vec!["xyz".into(), "connection a not found".into()], ..query("", &["connection", "found", "not"], 1) };
    let got = run(1, segments).await;
    assert_eq!((matched(&got), got[0].segment, got[0].span), (vec![row("svc/conn.go", 0, true)], 1, (0, 22)), "the first matching segment");
    assert_close(got[0].score, 1.0 - 0.5 * (1.0 - 18.0 / 22.0), "score uses the matching segment's length");

    let lost_q = query("connection db lost", &["connection", "db", "lost"], 10);
    assert!(run(1, lost_q.clone()).await.is_empty(), "sites of another rev are invisible");
    assert_eq!(matched(&run(2, lost_q).await), [row("svc/conn.go", 0, true)], "sites at the requested rev");
}

async fn match_catalog_verifies_entries(env: Env) {
    let o = org(1);
    let v = env.version(o, 1, sha(1), Some(revs(1))).await;
    let (en, de) = (oid(4), oid(5));
    let json = |p: &str, blob| entry(p, blob, Some(Language::Json), FileClass::Data, 10);
    env.manifest(o, &v, &[json("web/locales/en.json", en), json("web/locales/de.json", de)]).await;
    let conn = catalog_entry(
        "errors.conn_not_found",
        "Connection {} not found",
        r"\bConnection\s+.+?\s+not\s+found\b",
        &["connection", "found", "not"],
        18,
        3,
    );
    let zebra = catalog_entry(
        "errors.zebra",
        "Connection zebra not found",
        r"\bConnection\s+zebra\b",
        &["connection", "found", "not", "zebra"],
        23,
        5,
    );
    let denied = catalog_entry("errors.denied", "Access denied", r"\bAccess\s+denied\b", &["access", "denied"], 12, 4);
    let german = CatalogEntryRecord {
        locale: Some("de".into()),
        ..catalog_entry(
            "errors.conn_not_found",
            "Verbindung {} nicht gefunden",
            r"\bVerbindung\s+.+?\s+nicht\s+gefunden\b",
            &["gefunden", "nicht", "verbindung"],
            24,
            3,
        )
    };
    env.k
        .put_catalog(o, ErrorsRev(1), &[(en, vec![zebra.clone(), conn.clone(), denied]), (de, vec![german.clone()])])
        .await
        .must("put_catalog");
    let run = |q: ErrorQuery| {
        let k = env.k.clone();
        async move { k.match_catalog(o, &v, ErrorsRev(1), &q).await.must("match_catalog") }
    };

    let text = "Error: Connection pg-prod not found";
    let got = run(query(text, &["connection", "error", "found", "not", "pg", "prod"], 10)).await;
    let summary: Vec<(String, String, bool)> = got.iter().map(|m| (m.path.to_string(), m.entry.key.clone(), m.verified)).collect();
    assert_eq!(
        summary,
        [
            ("web/locales/en.json".to_owned(), "errors.conn_not_found".to_owned(), true),
            ("web/locales/en.json".to_owned(), "errors.zebra".to_owned(), false)
        ],
        "containment, verification, then the overlap fallback"
    );
    assert_eq!((&got[0].entry, got[0].segment, got[0].span), (&conn, 0, byte_span(text, "Connection", "found")));
    assert_close(got[0].score, 0.5 + 0.5 * (18.0 / text.len() as f32), "verified score");
    assert_eq!((&got[1].entry, got[1].span), (&zebra, (0, 0)));
    assert_close(got[1].score, 0.3, "fallback score at overlap 0.75");

    let got = run(query("Verbindung db nicht gefunden", &["db", "gefunden", "nicht", "verbindung"], 10)).await;
    assert_eq!(
        got.into_iter().map(|m| (m.path.to_string(), m.entry, m.verified)).collect::<Vec<_>>(),
        [("web/locales/de.json".to_owned(), german, true)]
    );
    assert!(
        env.k.match_catalog(o, &v, ErrorsRev(2), &query(text, &["connection", "found", "not"], 10)).await.must("match_catalog").is_empty()
    );
}

async fn i18n_sites_and_sites_in_a_symbol(env: Env) {
    let o = org(1);
    let v = env.version(o, 1, sha(1), Some(revs(1))).await;
    let (conn, web, tsx) = (oid(1), oid(3), oid(5));
    let ts = |p: &str, blob| entry(p, blob, Some(Language::Tsx), FileClass::Source, 10);
    env.manifest(o, &v, &[file("svc/conn.go", conn), ts("web/err.ts", web), ts("web/a.tsx", tsx)]).await;
    env.parse(o, 1, vec![(conn, parsed(vec![func(0, "Fetch", (1, 10)), func(1, "Other", (11, 20))], Vec::new()))]).await;
    let at = |ord: u32, pos: (u32, u32)| ErrorSiteRecord { pos, ..conn_site(ord) };
    env.sites(
        o,
        1,
        vec![
            (conn, vec![at(0, (12, 1)), at(1, (5, 3)), at(2, (3, 1)), at(3, (5, 1))]),
            (web, vec![i18n_site(0, "errors.conn", (40, 3)), conn_site(1)]),
            (tsx, vec![i18n_site(1, "errors.conn", (7, 1)), i18n_site(0, "errors.conn", (2, 1)), i18n_site(2, "errors.other", (9, 1))]),
        ],
    )
    .await;

    let uses = |rev: u32, key: &'static str| {
        let k = env.k.clone();
        async move { k.sites_for_i18n_key(o, &v, ErrorsRev(rev), key, &page(ALL)).await.must("sites_for_i18n_key").items }
    };
    let got = uses(1, "errors.conn").await;
    let spots: Vec<(String, (u32, u32), BlobOid)> = got.iter().map(|m| (m.path.to_string(), m.site.pos, m.blob)).collect();
    assert_eq!(
        spots,
        [("web/a.tsx".to_owned(), (2, 1), tsx), ("web/a.tsx".to_owned(), (7, 1), tsx), ("web/err.ts".to_owned(), (40, 3), web)],
        "I18nUse sites of the key, by path then position"
    );
    assert!(got.iter().all(|m| m.verified && m.score == 1.0 && m.segment == 0 && m.span == (0, 0)), "verified, score 1.0");
    let k = env.k.clone();
    let paged = all_pages(2, move |p| {
        let k = k.clone();
        async move { k.sites_for_i18n_key(o, &v, ErrorsRev(1), "errors.conn", &p).await }
    })
    .await;
    assert_eq!(paged, got, "concatenated sites_for_i18n_key pages equal one unpaged read");
    assert!(uses(1, "errors.none").await.is_empty());
    assert!(uses(2, "errors.conn").await.is_empty(), "sites of another rev are invisible");

    let inside = |symbol: SymbolRef, rev: u32, limit: u32| {
        let k = env.k.clone();
        async move { k.error_sites_in(o, &v, ErrorsRev(rev), &symbol, limit).await.must("error_sites_in") }
    };
    let fetch = sref("svc/conn.go", conn, 0);
    let got = inside(fetch.clone(), 1, 10).await;
    assert_eq!(
        got.iter().map(|m| (m.site.ord, m.site.pos)).collect::<Vec<_>>(),
        [(2, (3, 1)), (3, (5, 1)), (1, (5, 3))],
        "inside the span, by position"
    );
    assert!(got.iter().all(|m| m.path == path("svc/conn.go") && m.blob == conn && m.verified && m.score == 1.0));
    assert_eq!(inside(fetch.clone(), 1, 2).await.len(), 2, "at most `limit`");
    assert_eq!(inside(sref("svc/conn.go", conn, 1), 1, 10).await.into_iter().map(|m| m.site.ord).collect::<Vec<_>>(), [0]);
    assert!(inside(sref("svc/conn.go", tsx, 0), 1, 10).await.is_empty(), "a ref invalid at the version");
    assert!(inside(fetch, 2, 10).await.is_empty(), "sites of another rev are invisible");
}

// ---------------------------------------------------------------------------------------------
// Scenarios: app map, glossary

fn ids(nodes: &[AppNode]) -> Vec<u32> {
    nodes.iter().map(|node| node.id).collect()
}

fn edge_ends(graph: &AppGraph) -> Vec<(u32, u32)> {
    graph.edges.iter().map(|edge| (edge.from, edge.to)).collect()
}

async fn app_graph_replace_and_traverse(env: Env) {
    let o = org(1);
    let v = env.version(o, 1, sha(1), Some(revs(1))).await;
    let other = env.version(o, 1, sha(2), Some(revs(1))).await;
    let (routes, conn) = (oid(1), oid(2));
    env.manifest(o, &v, &[file("svc/routes.go", routes), file("svc/conn.go", conn)]).await;
    let endpoint_symbol = sref("svc/routes.go", routes, 0);
    let handler_symbol = sref("svc/conn.go", conn, 1);
    let nodes = vec![
        node(4, AppNodeKind::Handler, "ListConnections", Some(("svc/conn.go", 20)), Some(handler_symbol.clone())),
        node(1, AppNodeKind::Page, "/connections", Some(("web/page.tsx", 3)), None),
        node(6, AppNodeKind::Flag, "beta", None, None),
        node(3, AppNodeKind::Endpoint, "GET /api/connections", Some(("svc/routes.go", 10)), Some(endpoint_symbol.clone())),
        node(5, AppNodeKind::Table, "connections", Some(("svc/conn.go", 25)), None),
        node(2, AppNodeKind::ClientCall, "GET /api/connections", Some(("web/api.ts", 5)), None),
    ];
    let edges = vec![
        edge(1, 2, AppEdgeKind::Renders),
        edge(2, 3, AppEdgeKind::CallsEndpoint),
        edge(3, 4, AppEdgeKind::HandledBy),
        edge(4, 5, AppEdgeKind::Reads),
        edge(4, 6, AppEdgeKind::Checks),
    ];
    env.graph(o, &v, vec![node(9, AppNodeKind::Job, "old", None, None)], Vec::new()).await;
    env.graph(o, &v, nodes.clone(), edges.clone()).await;
    let duplicate = AppGraph { nodes: vec![nodes[0].clone(), nodes[0].clone()], edges: Vec::new() };
    assert_invalid(env.k.replace_app_graph(o, &v, &duplicate).await, "duplicate node ids");
    let dangling = AppGraph { nodes: nodes.clone(), edges: vec![edge(1, 99, AppEdgeKind::Renders)] };
    assert_invalid(env.k.replace_app_graph(o, &v, &dangling).await, "an edge to an unknown node");

    let list = |kinds: Vec<AppNodeKind>| {
        let k = env.k.clone();
        async move { k.app_nodes(o, &v, &kinds, &page(ALL)).await.must("app_nodes").items }
    };
    let all = list(Vec::new()).await;
    assert_eq!(ids(&all), [1, 2, 3, 4, 5, 6], "replaced (node 9 is gone), ordered by id; rejected graphs change nothing");
    assert_eq!(all[2], nodes[3], "nodes read back as written");
    assert_eq!(ids(&list(vec![AppNodeKind::Table, AppNodeKind::Endpoint]).await), [3, 5], "kinds filter");
    let k = env.k.clone();
    let paged = all_pages(4, move |p| {
        let k = k.clone();
        async move { k.app_nodes(o, &v, &[], &p).await }
    })
    .await;
    assert_eq!(paged, all, "concatenated app_nodes pages equal one unpaged read");
    assert!(env.k.app_nodes(o, &other, &[], &page(ALL)).await.must("app_nodes").items.is_empty(), "graphs are per version");

    let around = |focus: Vec<u32>, depth: u8, max_nodes: u32| {
        let k = env.k.clone();
        async move { k.app_neighborhood(o, &v, &focus, depth, max_nodes).await.must("app_neighborhood") }
    };
    let one_hop = around(vec![3], 1, 10).await;
    assert_eq!((ids(&one_hop.nodes), edge_ends(&one_hop)), (vec![2, 3, 4], vec![(2, 3), (3, 4)]), "both directions, edges among the nodes");
    assert_eq!(one_hop.edges[0], edges[1], "edges read back as written");
    let two_hops = around(vec![3], 2, 10).await;
    assert_eq!((ids(&two_hops.nodes), two_hops.edges), (vec![1, 2, 3, 4, 5, 6], edges.clone()));
    assert_eq!(around(vec![99, 3], 1, 10).await, one_hop, "unknown focus ids are ignored");
    let capped = around(vec![3], 1, 2).await;
    assert_eq!((ids(&capped.nodes), edge_ends(&capped)), (vec![2, 3], vec![(2, 3)]), "at most max_nodes, nearest first");
    let focus_only = around(vec![3], 0, 10).await;
    assert_eq!((ids(&focus_only.nodes), focus_only.edges.len()), (vec![3], 0));
    assert_eq!(around(vec![99], 2, 10).await, AppGraph::default());

    let bound = |symbols: Vec<SymbolRef>| {
        let k = env.k.clone();
        async move { ids(&k.app_nodes_for_symbols(o, &v, &symbols).await.must("app_nodes_for_symbols")) }
    };
    assert_eq!(bound(vec![handler_symbol.clone(), sref("svc/conn.go", conn, 7)]).await, [4]);
    assert_eq!(bound(vec![handler_symbol, endpoint_symbol]).await, [3, 4], "ordered by id");

    let facts = |blob, records| (blob, records);
    env.k
        .put_app_facts(
            o,
            AppMapRev(1),
            &[
                facts(conn, vec![table_fact(1, "connections", 25), flag_fact(0, "beta", 22)]),
                facts(routes, vec![table_fact(0, "users", 3)]),
                facts(oid(7), vec![table_fact(0, "ghost", 1)]),
            ],
        )
        .await
        .must("put_app_facts");
    env.k.put_app_facts(o, AppMapRev(2), &[facts(routes, vec![flag_fact(0, "beta2", 1)])]).await.must("put_app_facts");
    let at_rev = |rev: u32| {
        let k = env.k.clone();
        async move {
            let facts = k.app_facts_at(o, &v, AppMapRev(rev)).await.must("app_facts_at");
            facts.into_iter().map(|at| (at.path.to_string(), at.blob, at.fact)).collect::<Vec<_>>()
        }
    };
    assert_eq!(
        at_rev(1).await,
        [
            ("svc/conn.go".to_owned(), conn, flag_fact(0, "beta", 22)),
            ("svc/conn.go".to_owned(), conn, table_fact(1, "connections", 25)),
            ("svc/routes.go".to_owned(), routes, table_fact(0, "users", 3)),
        ],
        "facts of the manifest blobs at the rev, by path then ord"
    );
    assert_eq!(at_rev(2).await, [("svc/routes.go".to_owned(), routes, flag_fact(0, "beta2", 1))]);
}

fn hit_rows(hits: &[GlossaryHit]) -> Vec<(GlossarySource, String, (String, u32))> {
    hits.iter().map(|hit| (hit.source, hit.text.clone(), (hit.at.0.to_string(), hit.at.1))).collect()
}

fn glossary_row(source: GlossarySource, text: &str, p: &str, line: u32) -> (GlossarySource, String, (String, u32)) {
    (source, text.to_owned(), (p.to_owned(), line))
}

async fn glossary_hits_and_top(env: Env) {
    let o = org(1);
    let v = env.version(o, 1, sha(1), Some(revs(1))).await;
    let (conn, catalog, user) = (oid(1), oid(2), oid(3));
    let en = entry("web/locales/en.json", catalog, Some(Language::Json), FileClass::Data, 10);
    env.manifest(o, &v, &[file("svc/conn.go", conn), file("svc/user.go", user), en]).await;
    let private = |def: SymbolDef| SymbolDef { exported: false, ..def };
    let conn_symbols = vec![
        sym(0, "ConnectionPool", SymbolKind::Struct, (3, 8)),
        private(func(1, "openConnection", (10, 15))),
        sym(2, "Session", SymbolKind::Struct, (20, 25)),
        sym(3, "ConnectionPool", SymbolKind::Field, (3, 3)),
        sym(4, "User", SymbolKind::Struct, (70, 75)),
    ];
    let user_symbols = vec![sym(0, "User", SymbolKind::Struct, (2, 4)), sym(1, "UserConnection", SymbolKind::Interface, (5, 9))];
    env.parse(o, 1, vec![(conn, parsed(conn_symbols, Vec::new())), (user, parsed(user_symbols, Vec::new()))]).await;
    let catalog_rows = vec![
        catalog_entry(
            "errors.connection_lost",
            "Connection lost for session",
            r"\bConnection\s+lost\b",
            &["connection", "for", "lost", "session"],
            24,
            2,
        ),
        catalog_entry("errors.session", "Session expired", r"\bSession\s+expired\b", &["expired", "session"], 14, 3),
    ];
    env.k.put_catalog(o, ErrorsRev(1), &[(catalog, catalog_rows)]).await.must("put_catalog");
    let nodes = vec![
        node(1, AppNodeKind::Table, "connections", Some(("svc/conn.go", 30)), None),
        node(2, AppNodeKind::Table, "users", Some(("svc/user.go", 9)), None),
        node(3, AppNodeKind::Endpoint, "GET /api/connections/{}", Some(("svc/conn.go", 40)), None),
        node(4, AppNodeKind::Endpoint, "POST /api/connections", Some(("svc/conn.go", 50)), None),
        node(5, AppNodeKind::Page, "/connections", Some(("web/pages/conn.tsx", 1)), None),
        node(6, AppNodeKind::Event, ":connection/open", None, None),
        node(7, AppNodeKind::Event, ":session/close", Some(("svc/conn.go", 60)), None),
        node(8, AppNodeKind::Handler, "openConnectionHandler", Some(("svc/conn.go", 70)), None),
    ];
    env.graph(o, &v, nodes, Vec::new()).await;
    let hits = |words: &[&str], limit: u32| {
        let (k, words) = (env.k.clone(), strings(words));
        async move { k.glossary_hits(o, &v, &words, limit).await.must("glossary_hits") }
    };
    use GlossarySource::{CatalogKey, CatalogText, Endpoint, Event, Page, Symbol, Table};

    let got = hits(&["connection"], 50).await;
    assert_eq!(
        hit_rows(&got),
        [
            glossary_row(Symbol, "ConnectionPool", "svc/conn.go", 3),
            glossary_row(Symbol, "UserConnection", "svc/user.go", 5),
            glossary_row(Symbol, "openConnection", "svc/conn.go", 10),
            glossary_row(Table, "connections", "svc/conn.go", 30),
            glossary_row(Endpoint, "GET /api/connections/{}", "svc/conn.go", 40),
            glossary_row(Endpoint, "POST /api/connections", "svc/conn.go", 50),
            glossary_row(Page, "/connections", "web/pages/conn.tsx", 1),
            glossary_row(CatalogKey, "errors.connection_lost", "web/locales/en.json", 2),
            glossary_row(CatalogText, "Connection lost for session", "web/locales/en.json", 2),
        ],
        "case-insensitive containment over every source, identical hits once, nodes without a location skipped; by source, text"
    );
    assert!(got.iter().all(|hit| hit.weight == 1.0), "every word contained: weight 1.0");

    let two = hits(&["connection", "session"], 50).await;
    assert_eq!(two.len(), 13);
    assert_eq!(
        hit_rows(&two[..1]),
        [glossary_row(CatalogText, "Connection lost for session", "web/locales/en.json", 2)],
        "both words first"
    );
    assert_close(two[0].weight, 1.0, "weight = words contained / words");
    assert!(two[1..].iter().all(|hit| (hit.weight - 0.5).abs() < 1e-6), "one of two words: weight 0.5");
    assert!(hit_rows(&two).contains(&glossary_row(Event, ":session/close", "svc/conn.go", 60)));
    assert_eq!(hits(&["connection", "session"], 3).await, two[..3], "at most `limit`");
    assert!(hits(&[], 50).await.is_empty());

    let top = env.k.glossary_top(o, &v, 10).await.must("glossary_top");
    let rows: Vec<_> = top.iter().map(|hit| (hit.source, hit.text.as_str(), hit.weight, hit.at.0.to_string(), hit.at.1)).collect();
    let at = |source, text, weight, p: &str, line| (source, text, weight, p.to_owned(), line);
    assert_eq!(
        rows,
        [
            at(Symbol, "User", 2.0, "svc/conn.go", 70),
            at(Endpoint, "connections", 2.0, "svc/conn.go", 40),
            at(Symbol, "ConnectionPool", 1.0, "svc/conn.go", 3),
            at(Symbol, "Session", 1.0, "svc/conn.go", 20),
            at(Symbol, "UserConnection", 1.0, "svc/user.go", 5),
            at(Table, "connections", 1.0, "svc/conn.go", 30),
            at(Table, "users", 1.0, "svc/user.go", 9),
        ],
        "tables, endpoint resources and exported type names by count, then text; first location"
    );
    assert_eq!(env.k.glossary_top(o, &v, 3).await.must("glossary_top"), top[..3], "at most `limit`");
}

// ---------------------------------------------------------------------------------------------
// Scenarios: diff, history, GC

fn change(p: &str, change: ChangeKind, old: Option<u8>, new: Option<u8>) -> ManifestChange {
    ManifestChange { path: path(p), change, old: old.map(oid), new: new.map(oid) }
}

async fn manifest_diff_joins_two_versions(env: Env) {
    let o = org(1);
    let v1 = env.version(o, 1, sha(1), None).await;
    let v2 = env.version(o, 1, sha(2), None).await;
    let elsewhere = env.version(o, 2, sha(3), None).await;
    let link = ManifestEntry { kind: EntryKind::Symlink, ..file("lib", oid(4)) };
    env.manifest(o, &v1, &[file("a.go", oid(1)), file("b.go", oid(2)), file("c.go", oid(3)), file("docs/x.md", oid(5)), link]).await;
    env.manifest(o, &v2, &[file("a.go", oid(1)), file("b.go", oid(9)), file("d.go", oid(6)), file("lib", oid(4))]).await;
    let diff = |from: RepoVersionKey, to: RepoVersionKey, prefix: Option<&'static str>| {
        let k = env.k.clone();
        async move { k.manifest_diff(o, &from, &to, prefix.map(path).as_ref(), &page(ALL)).await.must("manifest_diff").items }
    };
    let forward = diff(v1, v2, None).await;
    assert_eq!(
        forward,
        vec![
            change("b.go", ChangeKind::Modified, Some(2), Some(9)),
            change("c.go", ChangeKind::Deleted, Some(3), None),
            change("d.go", ChangeKind::Added, None, Some(6)),
            change("docs/x.md", ChangeKind::Deleted, Some(5), None),
            change("lib", ChangeKind::TypeChanged, Some(4), Some(4)),
        ],
        "paths whose entry differs, by path"
    );
    assert_eq!(
        diff(v2, v1, None).await,
        vec![
            change("b.go", ChangeKind::Modified, Some(9), Some(2)),
            change("c.go", ChangeKind::Added, None, Some(3)),
            change("d.go", ChangeKind::Deleted, Some(6), None),
            change("docs/x.md", ChangeKind::Added, None, Some(5)),
            change("lib", ChangeKind::TypeChanged, Some(4), Some(4)),
        ]
    );
    assert_eq!(diff(v1, v2, Some("docs")).await, forward[3..4], "prefix filter");
    assert_eq!(diff(v1, v2, Some("b.go")).await, forward[..1], "a prefix equal to a path");
    assert!(diff(v1, v2, Some("do")).await.is_empty(), "prefixes match whole segments");
    assert!(diff(v1, v1, None).await.is_empty());
    let k = env.k.clone();
    let paged = all_pages(2, move |p| {
        let k = k.clone();
        async move { k.manifest_diff(o, &v1, &v2, None, &p).await }
    })
    .await;
    assert_eq!(paged, forward, "concatenated manifest_diff pages equal one unpaged read");
    assert_invalid(env.k.manifest_diff(o, &v1, &elsewhere, None, &page(ALL)).await, "manifest_diff across repos");
}

fn commit(n: u8, subject: &str, files: Option<Vec<FileChange>>) -> CommitRecord {
    let at = FakeClock::DEFAULT_START;
    CommitRecord {
        info: CommitInfo {
            sha: sha(n),
            tree: TreeOid::from_bytes([n; 20]),
            parents: vec![sha(n.wrapping_sub(1))],
            author_name: "Ada".into(),
            author_time: at,
            committer_time: at,
            subject: subject.into(),
            body: "body".into(),
            files,
        },
    }
}

fn file_change(p: &str, change: ChangeKind) -> FileChange {
    FileChange { path: path(p), old_path: None, change, adds: Some(1), dels: Some(0) }
}

fn pr(number: u32, merge_commit: Option<CommitSha>, title: &str) -> PullRequestRecord {
    PullRequestRecord {
        number,
        title: title.into(),
        body_head: "body".into(),
        head_ref: format!("feature-{number}"),
        base_ref: "main".into(),
        merge_commit,
        merged_at: merge_commit.map(|_| FakeClock::DEFAULT_START),
        updated_at: FakeClock::DEFAULT_START,
        url: format!("https://github.com/acme/repo/pull/{number}"),
    }
}

fn link(identifier: &str, source: LinkSource, commit: Option<CommitSha>, pr: Option<u32>) -> TicketLink {
    TicketLink { identifier: identifier.into(), source, commit, pr }
}

async fn history_round_trips(env: Env) {
    let (o, stranger) = (org(1), org(2));
    let repo = env.repo(o, 1).await;
    let other_repo = env.repo(o, 2).await;
    let k = &env.k;

    let files = vec![file_change("b.go", ChangeKind::Modified), file_change("a.go", ChangeKind::Added)];
    let first = commit(1, "first", Some(files.clone()));
    let sorted = commit(1, "first", Some(vec![files[1].clone(), files[0].clone()]));
    k.put_commits(o, repo, &[first, commit(2, "second", None), commit(3, "third", Some(Vec::new()))]).await.must("put_commits");
    let got = k.commits(o, repo, &[sha(3), sha(9), sha(1), sha(3)]).await.must("commits");
    assert_eq!(got, vec![commit(3, "third", None), sorted.clone()], "stored commits in input order; files by path, None without rows");
    assert!(k.has_commit(o, repo, sha(1)).await.must("has_commit"));
    assert!(!k.has_commit(o, repo, sha(9)).await.must("has_commit"));
    assert!(!k.has_commit(o, other_repo, sha(1)).await.must("has_commit"), "commits are per repo");
    assert!(!k.has_commit(stranger, repo, sha(1)).await.must("has_commit"), "commits are per org");
    k.put_commits(o, repo, &[commit(1, "first, amended", Some(files))]).await.must("put_commits");
    assert_eq!(k.commits(o, repo, &[sha(1)]).await.must("commits"), vec![commit(1, "first, amended", sorted.info.files)], "upsert by sha");

    k.put_pull_requests(o, repo, &[pr(11, Some(sha(1)), "b"), pr(10, Some(sha(1)), "a"), pr(12, None, "open"), pr(13, Some(sha(2)), "c")])
        .await
        .must("put_pull_requests");
    k.put_pull_requests(o, repo, &[pr(10, Some(sha(1)), "a, retitled")]).await.must("put_pull_requests");
    let got = k.pull_requests_for_commits(o, repo, &[sha(2), sha(1), sha(9), sha(2)]).await.must("pull_requests_for_commits");
    assert_eq!(
        got,
        vec![(sha(2), pr(13, Some(sha(2)), "c")), (sha(1), pr(10, Some(sha(1)), "a, retitled")), (sha(1), pr(11, Some(sha(1)), "b"))],
        "input sha order, then PR number; upsert by number"
    );
    assert!(k.pull_requests_for_commits(stranger, repo, &[sha(1)]).await.must("pull_requests_for_commits").is_empty());

    let l1 = link("ENG-1", LinkSource::CommitSubject, Some(sha(1)), None);
    let l2 = link("ENG-1", LinkSource::PrTitle, None, Some(10));
    let l3 = link("eng-2", LinkSource::CommitBody, Some(sha(2)), None);
    k.put_ticket_links(o, repo, &[l2.clone(), l1.clone(), l3.clone(), l1.clone()]).await.must("put_ticket_links");
    k.put_ticket_links(o, repo, std::slice::from_ref(&l1)).await.must("put_ticket_links");
    let links = |by: LinkLookup| {
        let k = env.k.clone();
        async move { k.ticket_links(o, repo, &by).await.must("ticket_links") }
    };
    assert_eq!(links(LinkLookup::Identifier("eng-1".into())).await, [l1.clone(), l2.clone()], "case-insensitive, inserted once");
    assert_eq!(links(LinkLookup::Identifier("ENG-2".into())).await, std::slice::from_ref(&l3));
    assert_eq!(links(LinkLookup::Commits(vec![sha(2), sha(1)])).await, [l1, l3], "ordered by identifier, source, commit, pr");
    assert_eq!(links(LinkLookup::Prs(vec![10, 99])).await, [l2]);
    assert!(links(LinkLookup::Identifier("ENG-3".into())).await.is_empty());
    assert!(k.ticket_links(stranger, repo, &LinkLookup::Identifier("ENG-1".into())).await.must("ticket_links").is_empty());

    assert_eq!(k.history_cursor(o, repo).await.must("history_cursor"), HistoryCursor::default(), "the empty default");
    let later = FakeClock::DEFAULT_START + Duration::from_secs(3600);
    let a = HistoryCursor { walked_heads: vec![sha(1)], prs_updated_after: Some(later) };
    let b = HistoryCursor { walked_heads: vec![sha(2), sha(1)], prs_updated_after: Some(FakeClock::DEFAULT_START) };
    k.merge_history_cursor(o, repo, &a).await.must("merge_history_cursor");
    k.merge_history_cursor(o, repo, &b).await.must("merge_history_cursor");
    assert_eq!(k.history_cursor(o, repo).await.must("history_cursor"), a.merged(&b), "HistoryCursor::merged");
    let merges = (20..28).map(|n| {
        let k = env.k.clone();
        async move {
            let head = HistoryCursor { walked_heads: vec![sha(n)], prs_updated_after: None };
            k.merge_history_cursor(o, repo, &head).await
        }
    });
    for result in join_all(merges).await {
        result.must("concurrent merge_history_cursor");
    }
    let heads: BTreeSet<CommitSha> = k.history_cursor(o, repo).await.must("history_cursor").walked_heads.into_iter().collect();
    let expected: BTreeSet<CommitSha> = [1, 2, 20, 21, 22, 23, 24, 25, 26, 27].into_iter().map(sha).collect();
    assert_eq!(heads, expected, "concurrent merges lose no head");
    assert_eq!(k.history_cursor(o, other_repo).await.must("history_cursor"), HistoryCursor::default(), "cursors are per repo");
}

async fn delete_version_facts_drops_one_sha(env: Env) {
    let o = org(1);
    let v1 = env.version(o, 1, sha(1), Some(revs(1))).await;
    let v2 = env.version(o, 1, sha(2), Some(revs(1))).await;
    let (a, b) = (oid(1), oid(2));
    for (key, artifact) in [(v1, art(1)), (v2, art(2))] {
        env.manifest(o, &key, &[file("svc/a.go", a), file("svc/b.go", b)]).await;
        env.graph(o, &key, vec![node(1, AppNodeKind::Table, "users", None, None)], Vec::new()).await;
        env.clock.advance(Duration::from_secs(60));
        env.ingest(o, &key, artifact, &scip_docs(a, b)).await;
    }
    env.parse(o, 1, call_blobs(a, b)).await;
    let occurrences = |artifact: ArtifactId, key: RepoVersionKey| {
        let k = env.k.clone();
        async move { k.scip_occurrences(o, artifact, &key, HELPER, true, &page(ALL)).await.must("scip_occurrences").items.len() }
    };
    assert_eq!(occurrences(art(1), v2).await, 4);

    env.k.delete_version_facts(o, &v1).await.must("delete_version_facts");
    assert!(env.k.manifest_all(o, &v1, &page(ALL)).await.must("manifest_all").items.is_empty(), "manifest deleted");
    assert!(env.k.app_nodes(o, &v1, &[], &page(ALL)).await.must("app_nodes").items.is_empty(), "app graph deleted");
    let index = env.k.scip_index_for(o, &v1).await.must("scip_index_for").map(|info| info.artifact);
    assert_eq!(index, Some(art(2)), "the index of the deleted sha is gone");
    assert_eq!(occurrences(art(1), v2).await, 0, "its rows are gone");

    assert_eq!(env.k.manifest_all(o, &v2, &page(ALL)).await.must("manifest_all").items.len(), 2, "other versions keep their facts");
    assert_eq!(ids(&env.k.app_nodes(o, &v2, &[], &page(ALL)).await.must("app_nodes").items), [1]);
    assert_eq!(occurrences(art(2), v2).await, 4);
    assert_eq!(env.names_in(o, &v2, "svc/a.go").await, ["Run", "Helper"], "blob facts are untouched");
    env.k.delete_version_facts(o, &v1).await.must("delete_version_facts again");
}

async fn sweep_respects_grace_and_references(env: Env) {
    let (o, other) = (org(1), org(2));
    let grace = Duration::from_secs(3600);
    let (b1, b2, b3, b4, b5) = (oid(1), oid(2), oid(3), oid(4), oid(5));
    let v = env.version(o, 1, sha(1), Some(revs(1))).await;
    env.manifest(o, &v, &[file("keep.go", b3)]).await;
    env.k.put_blob_meta(o, &[meta(b1, 3, true), meta(b2, 3, false), meta(b3, 3, true), meta(b4, 3, true)]).await.must("put_blob_meta");
    env.parse(o, 1, vec![(b1, parsed(vec![func(0, "Gone", (1, 2))], Vec::new())), (b2, parsed(Vec::new(), Vec::new()))]).await;
    env.parse(o, 2, vec![(b1, parsed(Vec::new(), Vec::new()))]).await;
    env.sites(o, 1, vec![(b1, vec![conn_site(0)])]).await;
    env.k.put_catalog(o, ErrorsRev(2), &[(b1, Vec::new())]).await.must("put_catalog");
    env.k.put_app_facts(o, AppMapRev(1), &[(b1, vec![table_fact(0, "users", 1)])]).await.must("put_app_facts");
    let w = env.version(other, 1, sha(1), Some(revs(1))).await;
    env.manifest(other, &w, &[file("uses_b4.go", b4)]).await;
    env.k.put_blob_meta(other, &[meta(b1, 3, true)]).await.must("put_blob_meta");
    env.parse(other, 1, vec![(b1, parsed(Vec::new(), Vec::new()))]).await;

    env.clock.advance(Duration::from_secs(30 * 60));
    assert!(env.k.sweep_unreferenced_blobs(o, grace, 100).await.must("sweep").is_empty(), "nothing is older than the grace yet");
    assert_eq!(env.meta_oids(o, &[b1, b2, b3, b4]).await, [b1, b2, b3, b4]);
    env.clock.advance(Duration::from_secs(60 * 60));
    env.k.put_blob_meta(o, &[meta(b5, 3, true)]).await.must("put_blob_meta");
    env.clock.advance(Duration::from_secs(30 * 60));

    assert_eq!(env.k.sweep_unreferenced_blobs(o, grace, 1).await.must("sweep"), [b1], "by oid, at most `max`");
    assert_eq!(
        env.k.sweep_unreferenced_blobs(o, grace, 100).await.must("sweep"),
        [b4],
        "b2 is swept without content to delete; b3 is referenced; b4 is referenced only in another org; b5 is too young"
    );
    assert_eq!(env.meta_oids(o, &[b1, b2, b3, b4, b5]).await, [b3, b5]);
    let stages = [
        BlobStage::Parse(ParserRev(1)),
        BlobStage::Parse(ParserRev(2)),
        BlobStage::ErrorSites(ErrorsRev(1)),
        BlobStage::ErrorSites(ErrorsRev(2)),
        BlobStage::AppFacts(AppMapRev(1)),
    ];
    for stage in stages {
        assert_eq!(env.missing(o, stage, &[b1]).await, [b1], "{stage:?} marker of a swept blob is deleted");
    }
    assert_eq!(env.missing(o, BlobStage::Parse(ParserRev(1)), &[b2]).await, [b2]);
    assert!(env.k.literal_sites(o, ParserRev(1), &[b1]).await.must("literal_sites").is_empty());
    assert_eq!(env.meta_oids(other, &[b1]).await, [b1], "another org's row of the same oid stays");
    assert!(env.missing(other, BlobStage::Parse(ParserRev(1)), &[b1]).await.is_empty(), "another org's facts stay");

    let again = env.version(o, 1, sha(2), Some(revs(1))).await;
    env.manifest(o, &again, &[file("back.go", b1)]).await;
    env.parse(o, 1, vec![(b1, parsed(vec![func(0, "Reborn", (1, 2))], Vec::new()))]).await;
    assert_eq!(env.names_in(o, &again, "back.go").await, ["Reborn"], "a swept blob's facts are gone, so a new parse is stored");
    env.sites(o, 1, vec![(b1, Vec::new())]).await;
    let q = query("connection pg-prod not found", &["connection", "found", "not", "pg", "prod"], 10);
    assert!(env.k.match_errors(o, &again, ErrorsRev(1), &q).await.must("match_errors").is_empty(), "a swept blob's error sites are gone");
    assert!(env.k.app_facts_at(o, &again, AppMapRev(1)).await.must("app_facts_at").is_empty(), "a swept blob's app facts are gone");
}
