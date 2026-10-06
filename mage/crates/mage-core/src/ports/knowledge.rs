//! `KnowledgeStore` port and every fact type the indexer writes and the server reads
//! (knowledge.md "Shared types", "`KnowledgeStore`"; tables in storage.md "Knowledge"). Layer 5
//! (Semantic: chunks, summaries) is not part of v1 and has no methods or types here.
//!
//! Semantics every implementation follows (contract suite in `mage_core::testing`):
//!
//! - **Tenancy.** Every method takes `OrgId`; facts of one org are invisible to every other, even
//!   for the same blob oid.
//! - **Blob-keyed facts** (symbols, refs, literal sites, error sites, catalog entries, app facts)
//!   are keyed `(org, blob_oid, rev, ord)`, written once (`ON CONFLICT DO NOTHING`: the first
//!   write wins, a repeat is a no-op), each blob with a `blob_stages` marker for its stage and rev,
//!   written even when the blob yields no rows.
//! - **A version's view** is its manifest (`put_manifest`) joined to blob-keyed facts at the
//!   version's `RepoVersion.revs` (stored through `RepoStore::set_index_revs`). Methods without a
//!   `rev` argument read at those revs; a version without revs (or without a manifest) shows no
//!   blob-keyed facts. Methods with a `rev` argument read at that rev. A blob present at two paths
//!   contributes its facts at both.
//! - **Manifest entries** read back `is_text` / `line_count` from the blob's [`BlobMeta`]
//!   (`false` / `0` without one, e.g. submodules); the other fields are stored per path.
//! - **Paging** (`PageReq` / `Paged`): `limit >= 1` (`0` is `Invalid`); `after` is the opaque
//!   `next` of the previous page; pages follow each method's documented order.
//! - **Ordering** of oid lists (`blob_meta`, `missing`, `literal_sites`) is by oid, deduplicated.

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

pub use crate::domain::knowledge::{UrlPart, UrlTemplate};
pub use crate::domain::manifest::{EntryKind, ManifestEntry};
pub use crate::domain::repo::{AppMapRev, ErrorsRev, ParserRev};

use crate::domain::ids::{ArtifactId, BlobOid, CommitSha, OrgId, RepoId, RepoVersionKey};
use crate::domain::path::{Language, LineRange, RepoPath};
use crate::wire::indexer::{ChangeKind, CommitInfo};

// ---------------------------------------------------------------------------------------------
// Files

/// One `blobs` row: per org and oid, write-once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BlobMeta {
    pub oid: BlobOid,
    pub size: u64,
    pub line_count: u32,
    pub is_text: bool,
    /// Raw bytes stored at `BlobKey::blob_content(org, oid)` (text blobs <= 2 MiB).
    pub content_stored: bool,
}

/// A per-blob extraction stage at a revision (`blob_stages.stage`, `rev`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "stage", content = "rev", rename_all = "snake_case")]
pub enum BlobStage {
    Parse(ParserRev),
    ErrorSites(ErrorsRev),
    AppFacts(AppMapRev),
}

/// Outcome recorded in a blob's stage marker.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StageStatus {
    Ok,
    /// Parsed with syntax errors; facts outside error nodes are kept.
    Partial { error_nodes: u32 },
    Failed { reason: String },
}

/// One blob handed to the `Parser` (indexer).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobInput {
    pub oid: BlobOid,
    pub path: RepoPath,
    pub lang: Language,
    pub bytes: Vec<u8>,
}

/// Page request of the knowledge reads; `after` is an adapter keyset, opaque to the app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageReq {
    pub limit: u32,
    pub after: Option<String>,
}

impl PageReq {
    pub fn first(limit: u32) -> Self {
        Self { limit, after: None }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Paged<T> {
    pub items: Vec<T>,
    pub next: Option<String>,
}

// ---------------------------------------------------------------------------------------------
// Symbols (Parser output)

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKind {
    Module,
    Function,
    Method,
    Class,
    Struct,
    Interface,
    Trait,
    Enum,
    TypeAlias,
    Constant,
    Variable,
    Field,
    Macro,
    Test,
    /// Framework registration named by a key (re-frame event / sub / fx).
    Registered,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolDef {
    /// Index within the blob, stable for a given `ParserRev`.
    pub ord: u32,
    /// `"Connect"`, `":connections/get"`.
    pub name: String,
    /// `"Server.Connect"`, `"webapp.connections.events/fetch"`.
    pub qualified: String,
    pub kind: SymbolKind,
    /// `ord` of the enclosing `SymbolDef`.
    pub container: Option<u32>,
    /// 1-based inclusive.
    pub span: LineRange,
    /// Line, column (1-based, UTF-8 bytes).
    pub name_pos: (u32, u32),
    /// First line of the declaration, <= 200 chars.
    pub signature: Option<String>,
    /// Leading doc comment / docstring, <= 1 KiB.
    pub doc: Option<String>,
    pub exported: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefKind {
    Call,
    Type,
    Read,
    Import,
    Dispatch,
    Render,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NameRef {
    pub ord: u32,
    /// Callee / referenced identifier, or module path for `Import`.
    pub name: String,
    /// `"http"` in `http.Error`, ns alias in CLJS, import alias for `Import`.
    pub qualifier: Option<String>,
    pub kind: RefKind,
    pub pos: (u32, u32),
    /// `ord` of the enclosing `SymbolDef`.
    pub enclosing: Option<u32>,
}

/// `expr` <= 64 chars.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Hole {
    Interp { expr: String },
    Concat { expr: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum LitPart {
    Text(String),
    Hole(Hole),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Construct {
    Throw,
    Raise,
    Panic,
    Return,
    Call,
    Attribute,
    MapValue,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum ArgSummary {
    Int(i64),
    Ident(String),
    Str,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiteralContext {
    /// Normalized callee text: `"fmt.Errorf"`, `"log/error"`, `"console.error"`.
    pub callee: Option<String>,
    pub arg_index: Option<u8>,
    /// <= 4 other args of the same call.
    pub siblings: Vec<ArgSummary>,
    pub construct: Construct,
    /// `"ValueError"`, `"Error"`, `"ex-info"`.
    pub exception_type: Option<String>,
    /// When the literal is a map/object value: `"message"`, `":text"`.
    pub map_key: Option<String>,
    /// Rust: `"error"` in `#[error("...")]`.
    pub attribute: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiteralSite {
    pub ord: u32,
    /// Concatenation / interpolation already flattened.
    pub parts: Vec<LitPart>,
    pub pos: (u32, u32),
    pub enclosing: Option<u32>,
    pub context: LiteralContext,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedBlob {
    pub lang: Language,
    pub status: StageStatus,
    pub symbols: Vec<SymbolDef>,
    pub refs: Vec<NameRef>,
    pub literals: Vec<LiteralSite>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseLimits {
    pub max_bytes: u64,
    pub timeout: Duration,
    pub max_symbols: u32,
    pub max_refs: u32,
    pub max_literals: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub key: String,
    pub locale: Option<String>,
    pub message: String,
    pub line: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogFormat {
    Json,
    Yaml,
    Properties,
    Po,
    Edn,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Catalog {
    pub format: CatalogFormat,
    pub entries: Vec<CatalogEntry>,
}

// ---------------------------------------------------------------------------------------------
// App facts (Parser output)

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FrameworkId {
    GoNetHttp,
    Gin,
    Chi,
    GorillaMux,
    Echo,
    JsFetch,
    Axios,
    ReactRouter,
    NextRoutes,
    ReFrame,
    CljsHttpMaps,
    Reitit,
    SqlStrings,
}

/// `root` = directory of the `go.mod` / `package.json` / … that enabled the framework;
/// `None` = the repo root.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FrameworkScope {
    pub root: Option<RepoPath>,
    pub framework: FrameworkId,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HandlerRef {
    Named { name: String, qualifier: Option<String> },
    Inline { enclosing: Option<u32>, line: u32 },
    Param { index: u8 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TableOp {
    Select,
    Insert,
    Update,
    Delete,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobRole {
    Enqueue,
    Handle,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AppFact {
    RouteGroup { var: String, prefix: UrlTemplate, parent: Option<String> },
    RouteDecl { method: Option<String>, path: UrlTemplate, group: Option<String>, handler: HandlerRef, middleware: Vec<String> },
    PageDecl { path: UrlTemplate, component: String },
    HttpCall { method: Option<String>, url: UrlTemplate, client: String },
    TableRef { table: String, op: TableOp },
    JobRef { name: String, role: JobRole },
    FlagRead { key: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct AppFactRecord {
    pub ord: u32,
    pub framework: FrameworkId,
    pub fact: AppFact,
    pub line: u32,
    pub enclosing: Option<u32>,
}

// ---------------------------------------------------------------------------------------------
// SCIP (ScipReader output, persisted by ingest)

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScipMetadata {
    pub tool_name: String,
    pub tool_version: String,
    pub project_root: String,
}

/// Converted to 1-based lines; columns as in SCIP (0-based UTF-8 offsets).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ScipRange {
    pub start_line: u32,
    pub start_col: u32,
    pub end_line: u32,
    pub end_col: u32,
}

impl ScipRange {
    /// `(line, col)` lies in `[start, end)`; `col = None` matches any column of a covered line.
    pub fn covers(&self, line: u32, col: Option<u32>) -> bool {
        if line < self.start_line || line > self.end_line {
            return false;
        }
        match col {
            None => true,
            Some(col) => {
                let after_start = line > self.start_line || col >= self.start_col;
                let before_end = line < self.end_line || col < self.end_col;
                after_start && before_end
            }
        }
    }
}

/// `SymbolRole` bit flags of scip.proto.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ScipRoles(pub u32);

impl ScipRoles {
    pub const DEFINITION: u32 = 1;
    pub const IMPORT: u32 = 2;
    pub const WRITE_ACCESS: u32 = 4;
    pub const READ_ACCESS: u32 = 8;
    pub const GENERATED: u32 = 16;
    pub const TEST: u32 = 32;

    pub const fn contains(self, flag: u32) -> bool {
        self.0 & flag == flag
    }

    pub const fn is_definition(self) -> bool {
        self.contains(Self::DEFINITION)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ScipOccurrence {
    pub range: ScipRange,
    pub symbol: String,
    pub roles: ScipRoles,
    pub enclosing: Option<ScipRange>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ScipRelationship {
    pub symbol: String,
    pub is_reference: bool,
    pub is_implementation: bool,
    pub is_type_definition: bool,
    pub is_definition: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScipSymbolInfo {
    pub symbol: String,
    pub display_name: Option<String>,
    pub kind: Option<String>,
    pub documentation: Option<String>,
    pub relationships: Vec<ScipRelationship>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScipDocument {
    pub relative_path: String,
    pub language: String,
    pub occurrences: Vec<ScipOccurrence>,
    pub symbols: Vec<ScipSymbolInfo>,
}

// ---------------------------------------------------------------------------------------------
// KnowledgeStore read/write types

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct FactCounts {
    pub symbols: u32,
    pub error_sites: u32,
    pub endpoints: u32,
    pub pages: u32,
    pub tables: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionStats {
    /// Manifest entries of kind `File`.
    pub files: u32,
    /// Sum of their sizes.
    pub bytes: u64,
    /// `(lang, files, lines)`, most files first, then by language.
    pub by_lang: Vec<(Language, u32, u64)>,
    /// First-level directories by file count, most files first, then by path; at most
    /// [`VersionStats::TOP_DIRS`].
    pub top_dirs: Vec<(RepoPath, u32)>,
    pub counts: FactCounts,
}

impl VersionStats {
    pub const TOP_DIRS: usize = 20;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DirEntryKind {
    Dir,
    File,
    Symlink,
    Submodule,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirEntry {
    /// Last segment.
    pub name: String,
    pub kind: DirEntryKind,
    /// `None` for directories.
    pub size: Option<u64>,
    pub lang: Option<Language>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum PathMatch {
    /// Case-insensitive (ASCII) substring of the path.
    Substring(String),
    /// `mage_core::domain::fix::Glob` syntax over the whole path.
    Glob(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NameMatch {
    Exact,
    CaseInsensitive,
    /// Case-insensitive prefix.
    Prefix,
    /// Trigram similarity ([`trigram_similarity`]) >= [`FUZZY_THRESHOLD`].
    Fuzzy,
}

/// `pg_trgm`'s default `similarity_threshold`.
pub const FUZZY_THRESHOLD: f32 = 0.3;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolQuery {
    pub name: String,
    /// Ranking signal, not a filter: a hit whose `qualified` name has `qualifier` as a segment
    /// (split on `.`, `/`, `::`), or whose directory is named `qualifier` (Go package), ranks first.
    pub qualifier: Option<String>,
    pub mode: NameMatch,
    /// Empty = every kind.
    pub kinds: Vec<SymbolKind>,
    pub lang: Option<Language>,
    pub path_prefix: Option<RepoPath>,
    pub include_tests: bool,
}

/// One symbol at a version: the path it is seen at, its blob and ord.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SymbolRef {
    pub path: RepoPath,
    pub blob: BlobOid,
    pub ord: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SymbolHit {
    pub at: SymbolRef,
    pub def: SymbolDef,
    /// [`trigram_similarity`] of the name and the query (1.0 for an exact name; 1.0 for
    /// position/file/ref lookups).
    pub score: f32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NameRefQuery {
    /// Exact name.
    pub name: String,
    /// When set, only refs with exactly this qualifier.
    pub qualifier: Option<String>,
    /// Empty = every kind.
    pub kinds: Vec<RefKind>,
    pub include_tests: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefHit {
    pub path: RepoPath,
    pub pos: (u32, u32),
    pub kind: RefKind,
    pub enclosing: Option<SymbolRef>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScipIndexInfo {
    pub artifact: ArtifactId,
    pub sha: CommitSha,
    pub tool: String,
    pub languages: Vec<String>,
    pub documents: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScipSymbolHit {
    pub symbol: String,
    pub display_name: Option<String>,
    /// The definition occurrence in the index, if any.
    pub def: Option<(RepoPath, ScipRange)>,
}

/// Occurrences of one SCIP document, mapped to the manifest blob at the index's sha.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScipDocRows {
    pub path: RepoPath,
    pub blob: BlobOid,
    pub occurrences: Vec<ScipOccurrence>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallDirection {
    Callers,
    Callees,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Precision {
    /// SCIP index of exactly this sha.
    Precise,
    /// SCIP index of `from_sha`, restricted to files whose blob is unchanged.
    PreciseStale { from_sha: CommitSha },
    /// Callee names resolved to definitions at the version.
    Name,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallEdge {
    pub from: SymbolRef,
    pub to: SymbolRef,
    /// `(path, line)` of each call site, in source order.
    pub sites: Vec<(RepoPath, u32)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorSiteKind {
    Throw,
    Log,
    HttpResponse,
    UiMessage,
    I18nUse,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Error,
    Warn,
    Info,
    Debug,
    Fatal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorSiteRecord {
    pub ord: u32,
    pub literal_ord: u32,
    pub kind: ErrorSiteKind,
    pub level: Option<LogLevel>,
    /// Display form: `"connection {} not found: {w}"`.
    pub template: String,
    /// Rust `regex` syntax, unanchored.
    pub pattern: String,
    /// Lowercased literal words, sorted, deduped.
    pub tokens: Vec<String>,
    /// Non-space literal chars.
    pub specificity: u32,
    pub pos: (u32, u32),
    pub enclosing: Option<u32>,
    pub http_status: Option<u16>,
    pub i18n_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogEntryRecord {
    pub key: String,
    pub locale: Option<String>,
    pub template: String,
    pub pattern: String,
    pub tokens: Vec<String>,
    pub specificity: u32,
    pub line: u32,
}

/// Input of `match_errors` / `match_catalog`, prepared by the app (`errors::prepare`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorQuery {
    /// Whole input, each line, chain suffixes; spans in results are byte ranges of one segment.
    pub segments: Vec<String>,
    /// `mage_core::domain::knowledge::tokenize` of the input.
    pub tokens: Vec<String>,
    pub max_candidates: u32,
    pub limit: u32,
    pub include_low_specificity: bool,
}

/// A site counts as low specificity below this many literal chars (or with fewer than two tokens).
pub const LOW_SPECIFICITY: u32 = 8;

/// Minimum `|site tokens ∩ input tokens| / |site tokens|` of an unverified fallback match.
pub const FALLBACK_OVERLAP: f32 = 0.6;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ErrorMatch {
    pub path: RepoPath,
    pub blob: BlobOid,
    pub site: ErrorSiteRecord,
    /// Index into `ErrorQuery.segments`.
    pub segment: u16,
    /// Byte range of the match in that segment (`(0, 0)` when not verified by a pattern).
    pub span: (u32, u32),
    pub verified: bool,
    pub score: f32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CatalogMatch {
    pub path: RepoPath,
    pub entry: CatalogEntryRecord,
    pub segment: u16,
    pub span: (u32, u32),
    pub verified: bool,
    pub score: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppNodeKind {
    Page,
    Component,
    ClientCall,
    Endpoint,
    Handler,
    Table,
    Job,
    Flag,
    Event,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppEdgeKind {
    Renders,
    CallsEndpoint,
    HandledBy,
    Dispatches,
    Reads,
    Writes,
    Enqueues,
    Checks,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppNode {
    /// Unique within one version's graph.
    pub id: u32,
    pub kind: AppNodeKind,
    /// Canonical: `"GET /api/connections/{}"`.
    pub key: String,
    pub label: String,
    pub framework: Option<FrameworkId>,
    pub at: Option<(RepoPath, u32)>,
    pub symbol: Option<SymbolRef>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppEdge {
    pub from: u32,
    pub to: u32,
    pub kind: AppEdgeKind,
    pub confidence: f32,
    pub evidence: Option<(RepoPath, u32)>,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct AppGraph {
    pub nodes: Vec<AppNode>,
    pub edges: Vec<AppEdge>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppFactAt {
    pub path: RepoPath,
    pub blob: BlobOid,
    pub fact: AppFactRecord,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GlossarySource {
    Symbol,
    Table,
    Endpoint,
    Page,
    Event,
    CatalogKey,
    CatalogText,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GlossaryHit {
    pub source: GlossarySource,
    pub text: String,
    pub at: (RepoPath, u32),
    pub weight: f32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManifestChange {
    pub path: RepoPath,
    /// `Added`, `Deleted`, `Modified` (blob differs) or `TypeChanged` (entry kind differs);
    /// manifests carry no rename information.
    pub change: ChangeKind,
    pub old: Option<BlobOid>,
    pub new: Option<BlobOid>,
}

/// `info.files` is stored in `commit_files`; it reads back as `None` when the commit has no file
/// rows, else sorted by path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommitRecord {
    pub info: CommitInfo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequestRecord {
    pub number: u32,
    pub title: String,
    pub body_head: String,
    pub head_ref: String,
    pub base_ref: String,
    pub merge_commit: Option<CommitSha>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub merged_at: Option<OffsetDateTime>,
    #[serde(with = "time::serde::rfc3339")]
    pub updated_at: OffsetDateTime,
    pub url: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinkSource {
    CommitSubject,
    CommitBody,
    MergeBranch,
    PrTitle,
    PrBranch,
    PrBody,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct TicketLink {
    /// `ENG-123`.
    pub identifier: String,
    pub source: LinkSource,
    pub commit: Option<CommitSha>,
    pub pr: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum LinkLookup {
    /// Case-insensitive identifier.
    Identifier(String),
    Commits(Vec<CommitSha>),
    Prs(Vec<u32>),
}

/// Progress of the History layer per repo.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct HistoryCursor {
    /// Heads already walked, oldest first, at most [`HistoryCursor::MAX_HEADS`].
    pub walked_heads: Vec<CommitSha>,
    #[serde(with = "time::serde::rfc3339::option")]
    pub prs_updated_after: Option<OffsetDateTime>,
}

impl HistoryCursor {
    pub const MAX_HEADS: usize = 50;

    /// `merge_history_cursor` rule: union of heads (new ones appended in order, the newest
    /// `MAX_HEADS` kept), max of `prs_updated_after`.
    pub fn merged(&self, other: &HistoryCursor) -> HistoryCursor {
        let mut heads = self.walked_heads.clone();
        for head in &other.walked_heads {
            if !heads.contains(head) {
                heads.push(*head);
            }
        }
        let excess = heads.len().saturating_sub(Self::MAX_HEADS);
        heads.drain(..excess);
        let prs_updated_after = match (self.prs_updated_after, other.prs_updated_after) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        HistoryCursor { walked_heads: heads, prs_updated_after }
    }
}

/// `pg_trgm` similarity of two strings: each lowercased word (runs of alphanumerics) padded with
/// two leading and one trailing space contributes its 3-character windows; the result is
/// `|common| / |union|` of the two trigram sets (0.0 when both are empty).
pub fn trigram_similarity(a: &str, b: &str) -> f32 {
    let (ta, tb) = (trigrams(a), trigrams(b));
    if ta.is_empty() && tb.is_empty() {
        return 0.0;
    }
    let common = ta.intersection(&tb).count();
    let union = ta.len() + tb.len() - common;
    common as f32 / union as f32
}

fn trigrams(s: &str) -> std::collections::BTreeSet<[char; 3]> {
    let mut out = std::collections::BTreeSet::new();
    for word in s.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()) {
        let padded: Vec<char> = "  ".chars().chain(word.chars().flat_map(char::to_lowercase)).chain(" ".chars()).collect();
        for window in padded.windows(3) {
            out.insert([window[0], window[1], window[2]]);
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KnowledgeError {
    #[error("not found")]
    NotFound,
    #[error("conflict")]
    Conflict,
    #[error("invalid knowledge request: {0}")]
    Invalid(String),
    #[error("knowledge store unavailable: {0}")]
    Unavailable(String),
    #[error("knowledge query timed out")]
    Timeout,
}

#[async_trait]
pub trait KnowledgeStore: Send + Sync {
    // ---- Files (indexer writes) ----
    /// Replaces the manifest of `key` in one transaction; a duplicate path is `Invalid`.
    async fn put_manifest(&self, org: OrgId, key: &RepoVersionKey, entries: &[ManifestEntry]) -> Result<(), KnowledgeError>;
    /// Insert-if-absent per oid.
    async fn put_blob_meta(&self, org: OrgId, metas: &[BlobMeta]) -> Result<(), KnowledgeError>;
    /// Rows of the oids that have one.
    async fn blob_meta(&self, org: OrgId, oids: &[BlobOid]) -> Result<Vec<BlobMeta>, KnowledgeError>;
    /// Oids from `oids` with no blob_stages row for (stage, rev).
    async fn missing(&self, org: OrgId, stage: BlobStage, oids: &[BlobOid]) -> Result<Vec<BlobOid>, KnowledgeError>;

    // ---- Symbols (indexer writes) ----
    /// Writes symbols, name_refs, literal_sites and the `Parse(rev)` marker per blob (`lang` and
    /// `status` from the parse); an `Err(reason)` writes only a `Failed { reason }` marker.
    async fn put_parsed(&self, org: OrgId, rev: ParserRev, blobs: &[(BlobOid, Result<ParsedBlob, String>)]) -> Result<(), KnowledgeError>;
    /// Every oid with a non-`Failed` `Parse(rev)` marker, with its language and literal sites in
    /// `ord` order (empty when it has none).
    async fn literal_sites(&self, org: OrgId, rev: ParserRev, oids: &[BlobOid]) -> Result<Vec<(BlobOid, Language, Vec<LiteralSite>)>, KnowledgeError>;

    // ---- ErrorSites (indexer writes) ----
    /// Rows plus the `ErrorSites(rev)` marker per blob.
    async fn put_error_sites(&self, org: OrgId, rev: ErrorsRev, blobs: &[(BlobOid, Vec<ErrorSiteRecord>)]) -> Result<(), KnowledgeError>;
    /// Rows (keyed by catalog key) plus the `ErrorSites(rev)` marker per blob.
    async fn put_catalog(&self, org: OrgId, rev: ErrorsRev, blobs: &[(BlobOid, Vec<CatalogEntryRecord>)]) -> Result<(), KnowledgeError>;

    // ---- AppMap (indexer writes) ----
    /// Rows plus the `AppFacts(rev)` marker per blob.
    async fn put_app_facts(&self, org: OrgId, rev: AppMapRev, blobs: &[(BlobOid, Vec<AppFactRecord>)]) -> Result<(), KnowledgeError>;
    /// Facts of the manifest blobs at `rev`, ordered by path, then ord.
    async fn app_facts_at(&self, org: OrgId, key: &RepoVersionKey, rev: AppMapRev) -> Result<Vec<AppFactAt>, KnowledgeError>;
    /// Replaces the graph of `key`; duplicate node ids or edges to unknown nodes are `Invalid`.
    async fn replace_app_graph(&self, org: OrgId, key: &RepoVersionKey, graph: &AppGraph) -> Result<(), KnowledgeError>;

    // ---- Refs (indexer writes) ----
    /// Starts (or restarts, dropping its rows) the ingest of `artifact`'s index for `key`.
    async fn begin_scip_index(&self, org: OrgId, key: &RepoVersionKey, artifact: ArtifactId, meta: &ScipMetadata) -> Result<(), KnowledgeError>;
    /// `NotFound` without `begin_scip_index`. Occurrence `ord` = position in the document.
    async fn put_scip_documents(&self, org: OrgId, artifact: ArtifactId, docs: &[ScipDocRows]) -> Result<(), KnowledgeError>;
    async fn put_scip_symbols(&self, org: OrgId, artifact: ArtifactId, symbols: &[ScipSymbolInfo]) -> Result<(), KnowledgeError>;
    /// Marks the index ready; only ready indexes are read.
    async fn finish_scip_index(&self, org: OrgId, artifact: ArtifactId, languages: &[String], documents: u32) -> Result<(), KnowledgeError>;

    // ---- History (indexer writes) ----
    /// Upsert by sha (commit and its file rows).
    async fn put_commits(&self, org: OrgId, repo: RepoId, commits: &[CommitRecord]) -> Result<(), KnowledgeError>;
    /// Upsert by number.
    async fn put_pull_requests(&self, org: OrgId, repo: RepoId, prs: &[PullRequestRecord]) -> Result<(), KnowledgeError>;
    /// Insert-if-absent by the whole link.
    async fn put_ticket_links(&self, org: OrgId, repo: RepoId, links: &[TicketLink]) -> Result<(), KnowledgeError>;
    /// The stored cursor, or the empty default.
    async fn history_cursor(&self, org: OrgId, repo: RepoId) -> Result<HistoryCursor, KnowledgeError>;
    /// Merges ([`HistoryCursor::merged`]). Safe under concurrency.
    async fn merge_history_cursor(&self, org: OrgId, repo: RepoId, cursor: &HistoryCursor) -> Result<(), KnowledgeError>;

    // ---- Reads at a version (server; some used by the indexer) ----
    /// Entries of the given paths that exist, ordered by path.
    async fn manifest_entries(&self, org: OrgId, key: &RepoVersionKey, paths: &[RepoPath]) -> Result<Vec<ManifestEntry>, KnowledgeError>;
    /// Ordered by path.
    async fn manifest_all(&self, org: OrgId, key: &RepoVersionKey, page: &PageReq) -> Result<Paged<ManifestEntry>, KnowledgeError>;
    /// Immediate children of `dir` (`None` = root), ordered by name; directories are implied by
    /// deeper paths.
    async fn list_dir(&self, org: OrgId, key: &RepoVersionKey, dir: Option<&RepoPath>, page: &PageReq) -> Result<Paged<DirEntry>, KnowledgeError>;
    /// Ordered by path; an unparsable glob is `Invalid`.
    async fn find_paths(&self, org: OrgId, key: &RepoVersionKey, m: &PathMatch, page: &PageReq) -> Result<Paged<ManifestEntry>, KnowledgeError>;
    /// Exact basename match ("conn.go"), ordered by path length then path. Used for stack-frame
    /// suffix matching and evidence-gate "did you mean" hints.
    async fn paths_by_basename(&self, org: OrgId, key: &RepoVersionKey, basename: &str, limit: u16) -> Result<Vec<RepoPath>, KnowledgeError>;
    async fn version_stats(&self, org: OrgId, key: &RepoVersionKey) -> Result<VersionStats, KnowledgeError>;

    /// Ordered by score, then qualifier match, exported, non-test, shorter path, path, ord.
    async fn find_symbols(&self, org: OrgId, key: &RepoVersionKey, q: &SymbolQuery, page: &PageReq) -> Result<Paged<SymbolHit>, KnowledgeError>;
    /// Enclosing chain at (path, line), innermost first.
    async fn symbols_at(&self, org: OrgId, key: &RepoVersionKey, path: &RepoPath, line: u32) -> Result<Vec<SymbolHit>, KnowledgeError>;
    /// Ordered by ord.
    async fn symbols_in_file(&self, org: OrgId, key: &RepoVersionKey, path: &RepoPath) -> Result<Vec<SymbolHit>, KnowledgeError>;
    /// Refs valid at the version (path holds that blob), in input order; others are skipped.
    async fn symbols_by_ref(&self, org: OrgId, key: &RepoVersionKey, refs: &[SymbolRef]) -> Result<Vec<SymbolHit>, KnowledgeError>;
    /// Ordered by path, then position.
    async fn name_refs(&self, org: OrgId, key: &RepoVersionKey, q: &NameRefQuery, page: &PageReq) -> Result<Paged<RefHit>, KnowledgeError>;
    /// One hop, batched. Name precision resolves callee names to defs at the version (`Call` and
    /// `Dispatch` refs, grouped by enclosing symbol); Precise uses SCIP (`NotFound` without a ready
    /// index at the requested sha). At most `limit` edges, ordered by (from, to).
    async fn call_edges(
        &self,
        org: OrgId,
        key: &RepoVersionKey,
        from: &[SymbolRef],
        dir: CallDirection,
        precision: &Precision,
        limit: u32,
    ) -> Result<Vec<CallEdge>, KnowledgeError>;

    /// SCIP index for exactly this sha, else the newest index of the repo (caller reports PreciseStale).
    async fn scip_index_for(&self, org: OrgId, key: &RepoVersionKey) -> Result<Option<ScipIndexInfo>, KnowledgeError>;
    async fn scip_symbol_at(&self, org: OrgId, index: ArtifactId, path: &RepoPath, line: u32, col: Option<u32>) -> Result<Option<ScipSymbolHit>, KnowledgeError>;
    /// Occurrences of `symbol`, restricted to documents whose blob equals the manifest blob at `key`.
    /// `defs = false` leaves definition occurrences out. Ordered by path, then position.
    async fn scip_occurrences(
        &self,
        org: OrgId,
        index: ArtifactId,
        key: &RepoVersionKey,
        symbol: &str,
        defs: bool,
        page: &PageReq,
    ) -> Result<Paged<RefHit>, KnowledgeError>;

    /// knowledge.md "Matching": candidates whose tokens all appear in `q.tokens`, verified by
    /// their pattern on the segments, then the overlap fallback. Verified first, then score.
    async fn match_errors(&self, org: OrgId, key: &RepoVersionKey, rev: ErrorsRev, q: &ErrorQuery) -> Result<Vec<ErrorMatch>, KnowledgeError>;
    async fn match_catalog(&self, org: OrgId, key: &RepoVersionKey, rev: ErrorsRev, q: &ErrorQuery) -> Result<Vec<CatalogMatch>, KnowledgeError>;
    /// `I18nUse` sites with this key (verified, score 1.0), ordered by path, then position.
    async fn sites_for_i18n_key(&self, org: OrgId, key: &RepoVersionKey, rev: ErrorsRev, i18n_key: &str, page: &PageReq) -> Result<Paged<ErrorMatch>, KnowledgeError>;
    /// Sites inside the symbol's span in its file, by position.
    async fn error_sites_in(&self, org: OrgId, key: &RepoVersionKey, rev: ErrorsRev, symbol: &SymbolRef, limit: u32) -> Result<Vec<ErrorMatch>, KnowledgeError>;

    /// Nodes of `kinds` (empty = all), ordered by id.
    async fn app_nodes(&self, org: OrgId, key: &RepoVersionKey, kinds: &[AppNodeKind], page: &PageReq) -> Result<Paged<AppNode>, KnowledgeError>;
    /// Nodes within `depth` hops of `focus` (both directions), capped at `max_nodes`.
    async fn app_neighborhood(&self, org: OrgId, key: &RepoVersionKey, focus: &[u32], depth: u8, max_nodes: u32) -> Result<AppGraph, KnowledgeError>;
    /// Nodes bound to any of `symbols`, ordered by id.
    async fn app_nodes_for_symbols(&self, org: OrgId, key: &RepoVersionKey, symbols: &[SymbolRef]) -> Result<Vec<AppNode>, KnowledgeError>;

    /// Union of symbol names, tables, endpoint / page keys, events, catalog keys and catalog texts
    /// containing any of the (lowercase) `words`; ordered by weight.
    async fn glossary_hits(&self, org: OrgId, key: &RepoVersionKey, words: &[String], limit: u32) -> Result<Vec<GlossaryHit>, KnowledgeError>;
    /// Tables, endpoint resources and exported type names by occurrence count.
    async fn glossary_top(&self, org: OrgId, key: &RepoVersionKey, limit: u32) -> Result<Vec<GlossaryHit>, KnowledgeError>;

    /// Paths whose entry differs between two versions of one repo (`Invalid` across repos),
    /// ordered by path.
    async fn manifest_diff(
        &self,
        org: OrgId,
        from: &RepoVersionKey,
        to: &RepoVersionKey,
        prefix: Option<&RepoPath>,
        page: &PageReq,
    ) -> Result<Paged<ManifestChange>, KnowledgeError>;
    async fn has_commit(&self, org: OrgId, repo: RepoId, sha: CommitSha) -> Result<bool, KnowledgeError>;
    /// Stored commits of `shas`, in input order.
    async fn commits(&self, org: OrgId, repo: RepoId, shas: &[CommitSha]) -> Result<Vec<CommitRecord>, KnowledgeError>;
    /// PRs whose merge commit is one of `shas`, in input order.
    async fn pull_requests_for_commits(&self, org: OrgId, repo: RepoId, shas: &[CommitSha]) -> Result<Vec<(CommitSha, PullRequestRecord)>, KnowledgeError>;
    /// Ordered by (identifier, source, commit, pr).
    async fn ticket_links(&self, org: OrgId, repo: RepoId, by: &LinkLookup) -> Result<Vec<TicketLink>, KnowledgeError>;

    // ---- GC ----
    /// Per-sha facts of `key`: manifest, app graph, SCIP indexes of that sha.
    async fn delete_version_facts(&self, org: OrgId, key: &RepoVersionKey) -> Result<(), KnowledgeError>;
    /// Delete blob facts + blobs rows not referenced by any manifest of the org and older than
    /// `grace`; returns the oids whose content objects the caller must delete.
    async fn sweep_unreferenced_blobs(&self, org: OrgId, grace: Duration, max: u32) -> Result<Vec<BlobOid>, KnowledgeError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sha(n: u8) -> CommitSha {
        CommitSha::from_bytes([n; 20])
    }

    #[test]
    fn trigram_similarity_matches_pg_trgm() {
        assert_eq!(trigram_similarity("connect", "connect"), 1.0);
        assert_eq!(trigram_similarity("Connect", "connect"), 1.0);
        assert_eq!(trigram_similarity("", ""), 0.0);
        // pg_trgm: similarity('word', 'two words') = 0.36363637
        assert!((trigram_similarity("word", "two words") - 0.363_636_37).abs() < 1e-6);
        assert!(trigram_similarity("connection", "conection") >= FUZZY_THRESHOLD);
        assert!(trigram_similarity("connect", "parse") < FUZZY_THRESHOLD);
    }

    #[test]
    fn history_cursor_merge_unions_heads_and_keeps_the_newest() {
        let a = HistoryCursor { walked_heads: (0..50).map(sha).collect(), prs_updated_after: Some(OffsetDateTime::UNIX_EPOCH) };
        let b = HistoryCursor { walked_heads: vec![sha(3), sha(60)], prs_updated_after: None };
        let merged = a.merged(&b);
        assert_eq!(merged.walked_heads.len(), HistoryCursor::MAX_HEADS);
        assert_eq!(merged.walked_heads.first(), Some(&sha(1)), "the oldest head is dropped");
        assert_eq!(merged.walked_heads.last(), Some(&sha(60)));
        assert_eq!(merged.prs_updated_after, Some(OffsetDateTime::UNIX_EPOCH));
        assert_eq!(HistoryCursor::default().merged(&b).walked_heads, vec![sha(3), sha(60)]);
    }

    #[test]
    fn scip_range_cover_is_half_open_on_columns() {
        let r = ScipRange { start_line: 3, start_col: 4, end_line: 3, end_col: 9 };
        assert!(r.covers(3, Some(4)) && r.covers(3, Some(8)) && r.covers(3, None));
        assert!(!r.covers(3, Some(9)) && !r.covers(3, Some(3)) && !r.covers(4, None));
    }
}
