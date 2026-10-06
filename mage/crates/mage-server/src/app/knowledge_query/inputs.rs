//! Tool inputs of the Knowledge MCP (knowledge.md "Tools"): one struct per tool, unknown fields
//! rejected, JSON Schema derived for `inputSchema`. Serde enforces shapes (a malformed call is a
//! JSON-RPC `-32602`); ranges are checked by the tools (`invalid_argument`).
//!
//! Every tool also takes `repo` and (except `changes_between`, which has `from`/`to`) `sha`:
//! optional, and when given they must equal the token's (`out_of_scope` otherwise).

use mage_core::domain::{Language, RepoPath};
use mage_core::ports::knowledge::{AppNodeKind, RefKind, SymbolKind};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::envelope::ToolError;

const fn one() -> u32 {
    1
}
const fn one_u8() -> u8 {
    1
}
const fn yes() -> bool {
    true
}
const fn limit_5() -> u32 {
    5
}
const fn limit_10() -> u32 {
    10
}
const fn limit_20() -> u32 {
    20
}
const fn limit_40() -> u32 {
    40
}
const fn limit_50() -> u32 {
    50
}
const fn limit_80() -> u32 {
    80
}
const fn limit_100() -> u32 {
    100
}
fn default_include() -> Vec<ChangeInclude> {
    vec![ChangeInclude::Files, ChangeInclude::Commits]
}

pub(crate) fn check_range<T: PartialOrd + std::fmt::Display>(name: &str, value: T, min: T, max: T) -> Result<(), ToolError> {
    if value < min || value > max {
        return Err(ToolError::invalid(format!("{name} must be in {min}..={max}, got {value}")));
    }
    Ok(())
}

pub(crate) fn check_len(name: &str, value: &str, min: usize, max: usize) -> Result<(), ToolError> {
    let len = value.chars().count();
    if len < min || len > max {
        return Err(ToolError::invalid(format!("{name} must have {min}..={max} characters, got {len}")));
    }
    Ok(())
}

/// `""` and `"/"` are the root (`None`); anything else must be a normalized repo path.
pub(crate) fn parse_path_or_root(path: &str) -> Result<Option<RepoPath>, ToolError> {
    let trimmed = path.trim().trim_start_matches("./").trim_matches('/');
    if trimmed.is_empty() {
        return Ok(None);
    }
    RepoPath::new(trimmed).map(Some).map_err(|e| ToolError::invalid(format!("path {path:?}: {e}")))
}

pub(crate) fn parse_path(path: &str) -> Result<RepoPath, ToolError> {
    parse_path_or_root(path)?.ok_or_else(|| ToolError::invalid("path must name a file"))
}

pub(crate) fn parse_lang(lang: &str) -> Result<Language, ToolError> {
    serde_json::from_value(serde_json::Value::String(lang.trim().to_ascii_lowercase()))
        .map_err(|_| ToolError::invalid(format!("unknown language {lang:?}")))
}

// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RepoOverviewInput {
    /// Repo id; defaults to (and must equal) the token's repo.
    #[serde(default)]
    pub repo: Option<String>,
    /// 40-hex commit; defaults to (and must equal) the token's sha.
    #[serde(default)]
    pub sha: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    /// Literal text (live search on the indexer).
    #[default]
    Text,
    /// Rust `regex` syntax.
    Regex,
    /// Symbol names (fuzzy).
    Symbol,
    /// File paths (substring, or a glob when the query has `*` or `?`).
    Path,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CaseArg {
    /// Case-insensitive unless the query has an uppercase letter.
    #[default]
    Smart,
    Sensitive,
    Insensitive,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SearchCodeInput {
    #[schemars(length(min = 1, max = 512))]
    pub query: String,
    #[serde(default)]
    pub mode: SearchMode,
    /// Only paths matching one of these globs (`**`, `*`, `?`).
    #[serde(default)]
    #[schemars(length(max = 8))]
    pub path_glob: Vec<String>,
    #[serde(default)]
    #[schemars(length(max = 8))]
    pub exclude_glob: Vec<String>,
    /// Language name, e.g. `go`, `typescript`, `clojurescript`.
    #[serde(default)]
    pub lang: Option<String>,
    #[serde(default)]
    pub case: CaseArg,
    /// Lines of context around each hit.
    #[serde(default = "one_u8")]
    #[schemars(range(max = 3))]
    pub context: u8,
    /// Include generated, vendored and binary files.
    #[serde(default)]
    pub include_generated: bool,
    #[serde(default = "limit_20")]
    #[schemars(range(min = 1, max = 50))]
    pub limit: u32,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub sha: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadFileInput {
    /// File or directory path; `""` is the repo root.
    pub path: String,
    #[serde(default = "one")]
    #[schemars(range(min = 1))]
    pub start_line: u32,
    /// Default: up to 400 lines from `start_line`.
    #[serde(default)]
    pub end_line: Option<u32>,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub sha: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SymbolKindArg {
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
    Registered,
}

impl From<SymbolKindArg> for SymbolKind {
    fn from(kind: SymbolKindArg) -> Self {
        match kind {
            SymbolKindArg::Module => SymbolKind::Module,
            SymbolKindArg::Function => SymbolKind::Function,
            SymbolKindArg::Method => SymbolKind::Method,
            SymbolKindArg::Class => SymbolKind::Class,
            SymbolKindArg::Struct => SymbolKind::Struct,
            SymbolKindArg::Interface => SymbolKind::Interface,
            SymbolKindArg::Trait => SymbolKind::Trait,
            SymbolKindArg::Enum => SymbolKind::Enum,
            SymbolKindArg::TypeAlias => SymbolKind::TypeAlias,
            SymbolKindArg::Constant => SymbolKind::Constant,
            SymbolKindArg::Variable => SymbolKind::Variable,
            SymbolKindArg::Field => SymbolKind::Field,
            SymbolKindArg::Macro => SymbolKind::Macro,
            SymbolKindArg::Test => SymbolKind::Test,
            SymbolKindArg::Registered => SymbolKind::Registered,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MatchArg {
    /// Exact, then case-insensitive, then prefix, then fuzzy: the first that finds something.
    #[default]
    Auto,
    Exact,
    Prefix,
    Fuzzy,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FindSymbolInput {
    /// Name, or qualified: `pkg.Func`, `Type.Method`, `ns/fn`, `:ns/kw`, `Class::method`.
    #[schemars(length(min = 1, max = 256))]
    pub name: String,
    #[serde(default)]
    pub kind: Vec<SymbolKindArg>,
    #[serde(default)]
    pub lang: Option<String>,
    #[serde(default)]
    pub path_prefix: Option<String>,
    #[serde(default, rename = "match")]
    pub matching: MatchArg,
    #[serde(default = "yes")]
    pub include_tests: bool,
    #[serde(default = "limit_20")]
    #[schemars(range(min = 1, max = 50))]
    pub limit: u32,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub sha: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RefKindArg {
    Call,
    Read,
    Write,
    Import,
    Type,
}

impl RefKindArg {
    /// Name-level ref kinds this filter keeps (tree-sitter does not classify writes).
    pub fn name_kinds(self) -> &'static [RefKind] {
        match self {
            RefKindArg::Call => &[RefKind::Call, RefKind::Dispatch, RefKind::Render],
            RefKindArg::Read => &[RefKind::Read],
            RefKindArg::Write => &[],
            RefKindArg::Import => &[RefKind::Import],
            RefKindArg::Type => &[RefKind::Type],
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReferencesInput {
    /// Qualified name or SCIP symbol string; or give `path` + `line`.
    #[serde(default)]
    pub symbol: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub line: Option<u32>,
    #[serde(default)]
    pub column: Option<u32>,
    #[serde(default)]
    pub kinds: Vec<RefKindArg>,
    #[serde(default = "yes")]
    pub include_tests: bool,
    #[serde(default = "limit_50")]
    #[schemars(range(min = 1, max = 100))]
    pub limit: u32,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub sha: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum DirectionArg {
    #[default]
    Callers,
    Callees,
    Both,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CallGraphInput {
    #[serde(default)]
    pub symbol: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub line: Option<u32>,
    #[serde(default)]
    pub direction: DirectionArg,
    #[serde(default = "one_u8")]
    #[schemars(range(min = 1, max = 3))]
    pub depth: u8,
    /// Maximum nodes.
    #[serde(default = "limit_40")]
    #[schemars(range(min = 1, max = 100))]
    pub limit: u32,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub sha: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LookupErrorInput {
    /// A log line, stack trace or error message, verbatim.
    #[schemars(length(min = 1, max = 4096))]
    pub text: String,
    #[serde(default = "limit_10")]
    #[schemars(range(min = 1, max = 20))]
    pub limit: u32,
    #[serde(default)]
    pub include_low_specificity: bool,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub sha: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FocusKind {
    Route,
    Page,
    Component,
    Symbol,
    Table,
    Path,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Focus {
    pub kind: FocusKind,
    pub value: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum AppNodeKindArg {
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

impl From<AppNodeKindArg> for AppNodeKind {
    fn from(kind: AppNodeKindArg) -> Self {
        match kind {
            AppNodeKindArg::Page => AppNodeKind::Page,
            AppNodeKindArg::Component => AppNodeKind::Component,
            AppNodeKindArg::ClientCall => AppNodeKind::ClientCall,
            AppNodeKindArg::Endpoint => AppNodeKind::Endpoint,
            AppNodeKindArg::Handler => AppNodeKind::Handler,
            AppNodeKindArg::Table => AppNodeKind::Table,
            AppNodeKindArg::Job => AppNodeKind::Job,
            AppNodeKindArg::Flag => AppNodeKind::Flag,
            AppNodeKindArg::Event => AppNodeKind::Event,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AppMapInput {
    #[serde(default)]
    pub focus: Option<Focus>,
    #[serde(default)]
    pub kinds: Vec<AppNodeKindArg>,
    #[serde(default = "one_u8")]
    #[schemars(range(min = 1, max = 2))]
    pub depth: u8,
    #[serde(default = "limit_80")]
    #[schemars(range(min = 1, max = 200))]
    pub limit: u32,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub sha: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RouteInfoInput {
    /// Path or full URL; query and fragment are ignored.
    pub url: String,
    #[serde(default)]
    pub method: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub sha: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExplainInput {
    #[serde(default)]
    pub symbol: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub line: Option<u32>,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub sha: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GlossaryInput {
    #[serde(default)]
    #[schemars(length(min = 1, max = 128))]
    pub term: Option<String>,
    #[serde(default = "limit_20")]
    #[schemars(range(min = 1, max = 50))]
    pub limit: u32,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub sha: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChangeInclude {
    Files,
    Commits,
    Hunks,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ChangesBetweenInput {
    /// 40-hex sha of the older commit.
    pub from: String,
    /// 40-hex sha; defaults to (and must equal) the token's sha.
    #[serde(default)]
    pub to: Option<String>,
    #[serde(default)]
    pub path_prefix: Option<String>,
    #[serde(default = "default_include")]
    pub include: Vec<ChangeInclude>,
    #[serde(default = "limit_100")]
    #[schemars(range(min = 1, max = 200))]
    pub limit: u32,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub repo: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BlameInput {
    pub path: String,
    #[schemars(range(min = 1))]
    pub start_line: u32,
    /// At most `start_line + 199`.
    pub end_line: u32,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub sha: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SimilarTicketsInput {
    /// Default: the run's ticket title, body and error strings.
    #[serde(default)]
    #[schemars(length(max = 4096))]
    pub text: Option<String>,
    #[serde(default = "limit_5")]
    #[schemars(range(min = 1, max = 10))]
    pub limit: u32,
    #[serde(default)]
    pub repo: Option<String>,
    #[serde(default)]
    pub sha: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RunRecipeInput {
    #[serde(default)]
    pub repo: Option<String>,
}
