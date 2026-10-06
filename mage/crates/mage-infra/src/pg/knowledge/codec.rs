//! Column codecs of the knowledge tables: domain enums ↔ text (their serde names), integer range
//! checks, stored paths and ranges, and the opaque keyset cursors of the paged reads.

use mage_core::domain::path::{FileClass, Language, RepoPath};
use mage_core::ports::knowledge::{
    AppEdgeKind, AppFact, AppNodeKind, BlobStage, EntryKind, ErrorSiteKind, FrameworkId, LinkSource, LogLevel, PageReq, Paged, RefKind,
    ScipRange, StageStatus, SymbolKind,
};
use mage_core::wire::indexer::ChangeKind;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::pg::error::DbError;

/// Rows per bulk `INSERT … SELECT … FROM UNNEST(…)` (storage.md "Adapter internals").
pub(super) const CHUNK: usize = 5_000;

/// A domain enum stored as text.
pub(super) trait DbEnum: Sized + Copy {
    fn as_db(self) -> &'static str;
    fn from_db(s: &str) -> Result<Self, DbError>;
}

macro_rules! db_enum {
    ($ty:ident, $what:literal { $($variant:ident => $text:literal),+ $(,)? }) => {
        impl DbEnum for $ty {
            fn as_db(self) -> &'static str {
                match self {
                    $($ty::$variant => $text,)+
                }
            }

            fn from_db(s: &str) -> Result<Self, DbError> {
                match s {
                    $($text => Ok($ty::$variant),)+
                    other => Err(DbError::Invalid(format!(concat!("unknown stored ", $what, " {:?}"), other))),
                }
            }
        }
    };
}

db_enum!(Language, "language" {
    Go => "go", TypeScript => "typescript", Tsx => "tsx", JavaScript => "javascript", Python => "python", Rust => "rust",
    Java => "java", Ruby => "ruby", Clojure => "clojure", ClojureScript => "clojurescript", Cljc => "cljc", Sql => "sql",
    Json => "json", Yaml => "yaml", Toml => "toml", Markdown => "markdown", Html => "html", Css => "css", Shell => "shell",
    Proto => "proto", Other => "other",
});
db_enum!(FileClass, "file class" {
    Source => "source", Test => "test", Generated => "generated", Vendored => "vendored", Doc => "doc", Config => "config",
    Data => "data", Binary => "binary",
});
db_enum!(EntryKind, "entry kind" { File => "file", Symlink => "symlink", Submodule => "submodule" });
db_enum!(SymbolKind, "symbol kind" {
    Module => "module", Function => "function", Method => "method", Class => "class", Struct => "struct",
    Interface => "interface", Trait => "trait", Enum => "enum", TypeAlias => "type_alias", Constant => "constant",
    Variable => "variable", Field => "field", Macro => "macro", Test => "test", Registered => "registered",
});
db_enum!(RefKind, "ref kind" {
    Call => "call", Type => "type", Read => "read", Import => "import", Dispatch => "dispatch", Render => "render",
});
db_enum!(ErrorSiteKind, "error site kind" {
    Throw => "throw", Log => "log", HttpResponse => "http_response", UiMessage => "ui_message", I18nUse => "i18n_use",
});
db_enum!(LogLevel, "log level" { Error => "error", Warn => "warn", Info => "info", Debug => "debug", Fatal => "fatal" });
db_enum!(FrameworkId, "framework" {
    GoNetHttp => "go_net_http", Gin => "gin", Chi => "chi", GorillaMux => "gorilla_mux", Echo => "echo", JsFetch => "js_fetch",
    Axios => "axios", ReactRouter => "react_router", NextRoutes => "next_routes", ReFrame => "re_frame",
    CljsHttpMaps => "cljs_http_maps", Reitit => "reitit", SqlStrings => "sql_strings",
});
db_enum!(AppNodeKind, "app node kind" {
    Page => "page", Component => "component", ClientCall => "client_call", Endpoint => "endpoint", Handler => "handler",
    Table => "table", Job => "job", Flag => "flag", Event => "event",
});
db_enum!(AppEdgeKind, "app edge kind" {
    Renders => "renders", CallsEndpoint => "calls_endpoint", HandledBy => "handled_by", Dispatches => "dispatches",
    Reads => "reads", Writes => "writes", Enqueues => "enqueues", Checks => "checks",
});
db_enum!(LinkSource, "link source" {
    CommitSubject => "commit_subject", CommitBody => "commit_body", MergeBranch => "merge_branch", PrTitle => "pr_title",
    PrBranch => "pr_branch", PrBody => "pr_body",
});
db_enum!(ChangeKind, "change kind" {
    Added => "added", Modified => "modified", Deleted => "deleted", Renamed => "renamed", TypeChanged => "type_changed",
});

/// `blob_stages.stage` and `.rev` of a stage marker.
pub(super) fn stage_columns(stage: BlobStage) -> Result<(&'static str, i16), DbError> {
    match stage {
        BlobStage::Parse(rev) => Ok(("parse", to_rev(rev.0)?)),
        BlobStage::ErrorSites(rev) => Ok(("error_sites", to_rev(rev.0)?)),
        BlobStage::AppFacts(rev) => Ok(("app_facts", to_rev(rev.0)?)),
    }
}

/// `blob_stages.status` and `.error`: the failure reason, or the error-node count of a partial parse.
pub(super) fn status_columns(status: &StageStatus) -> (&'static str, Option<String>) {
    match status {
        StageStatus::Ok => ("ok", None),
        StageStatus::Partial { error_nodes } => ("partial", Some(error_nodes.to_string())),
        StageStatus::Failed { reason } => ("failed", Some(reason.clone())),
    }
}

/// `app_facts.kind`: the fact's variant (`data` holds the whole fact).
pub(super) fn fact_kind(fact: &AppFact) -> &'static str {
    match fact {
        AppFact::RouteGroup { .. } => "route_group",
        AppFact::RouteDecl { .. } => "route_decl",
        AppFact::PageDecl { .. } => "page_decl",
        AppFact::HttpCall { .. } => "http_call",
        AppFact::TableRef { .. } => "table_ref",
        AppFact::JobRef { .. } => "job_ref",
        AppFact::FlagRead { .. } => "flag_read",
    }
}

/// A `*_rev smallint` column.
pub(super) fn to_rev(rev: u32) -> Result<i16, DbError> {
    i16::try_from(rev).map_err(|_| DbError::Invalid(format!("revision {rev} exceeds the stored range")))
}

pub(super) fn to_i32(n: u32, what: &str) -> Result<i32, DbError> {
    i32::try_from(n).map_err(|_| DbError::Invalid(format!("{what} {n} exceeds the stored range")))
}

pub(super) fn to_i64(n: u64, what: &str) -> Result<i64, DbError> {
    i64::try_from(n).map_err(|_| DbError::Invalid(format!("{what} {n} exceeds the stored range")))
}

pub(super) fn to_u32(n: i32, what: &str) -> Result<u32, DbError> {
    u32::try_from(n).map_err(|_| DbError::Invalid(format!("stored {what} {n} is negative")))
}

pub(super) fn to_u64(n: i64, what: &str) -> Result<u64, DbError> {
    u64::try_from(n).map_err(|_| DbError::Invalid(format!("stored {what} {n} is negative")))
}

/// A `count(*)` as `u32`, saturating.
pub(super) fn count(n: i64) -> u32 {
    u32::try_from(n.max(0)).unwrap_or(u32::MAX)
}

pub(super) fn to_path(path: String) -> Result<RepoPath, DbError> {
    RepoPath::new(path).map_err(|e| DbError::Invalid(format!("stored path: {e}")))
}

/// `int4[]` `{start_line, start_col, end_line, end_col}` of a SCIP range.
pub(super) fn range_columns(range: &ScipRange) -> Result<[i32; 4], DbError> {
    Ok([
        to_i32(range.start_line, "scip line")?,
        to_i32(range.start_col, "scip column")?,
        to_i32(range.end_line, "scip line")?,
        to_i32(range.end_col, "scip column")?,
    ])
}

pub(super) fn to_range(stored: &[i32]) -> Result<ScipRange, DbError> {
    let [start_line, start_col, end_line, end_col] = stored else {
        return Err(DbError::Invalid(format!("stored scip range {stored:?} does not have 4 elements")));
    };
    Ok(ScipRange {
        start_line: to_u32(*start_line, "scip line")?,
        start_col: to_u32(*start_col, "scip column")?,
        end_line: to_u32(*end_line, "scip line")?,
        end_col: to_u32(*end_col, "scip column")?,
    })
}

/// A validated `PageReq` with its decoded keyset: the sort key of the last item served, in the
/// method's order (JSON, opaque to the app).
pub(super) struct Page<K> {
    pub limit: usize,
    pub after: Option<K>,
}

impl<K: Serialize + DeserializeOwned> Page<K> {
    pub fn new(req: &PageReq) -> Result<Self, DbError> {
        if req.limit == 0 {
            return Err(DbError::Invalid("page limit must be at least 1".into()));
        }
        let after = match &req.after {
            None => None,
            Some(cursor) => Some(serde_json::from_str(cursor).map_err(|_| DbError::Invalid(format!("bad cursor {cursor:?}")))?),
        };
        Ok(Self { limit: usize::try_from(req.limit).unwrap_or(usize::MAX), after })
    }

    /// `LIMIT` of a keyset query: the row past the page tells whether another page follows.
    pub fn fetch(&self) -> i64 {
        i64::try_from(self.limit).unwrap_or(i64::MAX).saturating_add(1)
    }

    /// The page of `items` (in the method's order, starting after the cursor, at most one past the
    /// page), with the cursor of its last item when more follow.
    pub fn finish<T>(&self, mut items: Vec<T>, key: impl Fn(&T) -> K) -> Result<Paged<T>, DbError> {
        if items.len() <= self.limit {
            return Ok(Paged { items, next: None });
        }
        items.truncate(self.limit);
        let next = match items.last() {
            Some(last) => Some(serde_json::to_string(&key(last)).map_err(|e| DbError::Invalid(format!("cursor: {e}")))?),
            None => None,
        };
        Ok(Paged { items, next })
    }
}
