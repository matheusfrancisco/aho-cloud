//! Run MCP tool inputs (executor.md "Tool schemas"). Each struct derives `Deserialize` +
//! `JsonSchema` with `deny_unknown_fields`; the served `inputSchema` is generated from it. Bounds
//! are declared for the schema and enforced again in `RunTools` (clients may not validate).
//! `submit_*` inputs are `mage_core::wire::mcp` types with checked-in schemas.

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::Deserialize;

pub const EXEC_COMMAND_MAX: usize = 16 * 1024;
pub const EXEC_ARGV_MAX: usize = 256;
pub const EXEC_TIMEOUT_MAX_S: u32 = 900;
pub const EXEC_ENV_MAX: usize = 32;
pub const EXEC_ENV_VALUE_MAX: usize = 4096;
pub const STDIN_MAX: usize = 1024 * 1024;
pub const READ_LINES_MAX: u32 = 2000;
pub const WRITE_CONTENT_MAX: usize = 1024 * 1024;
pub const LOGS_TAIL_MAX: u32 = 2000;
pub const GREP_MAX: usize = 512;
pub const PW_GREP_MAX: usize = 256;
pub const PW_TIMEOUT_MIN_S: u32 = 10;
pub const PW_TIMEOUT_MAX_S: u32 = 600;
pub const HTTP_HEADERS_MAX: usize = 32;
pub const HTTP_BODY_MAX: usize = 1024 * 1024;
pub const HTTP_TIMEOUT_MAX_S: u32 = 60;
pub const SQL_MAX: usize = 64 * 1024;
pub const DB_PARAMS_MAX: usize = 100;
pub const DB_ROWS_MAX: u32 = 1000;
pub const DB_TIMEOUT_MAX_S: u32 = 30;

fn dot() -> String {
    ".".to_owned()
}
const fn exec_timeout() -> u32 {
    120
}
const fn one() -> u32 {
    1
}
const fn read_lines() -> u32 {
    READ_LINES_MAX
}
const fn depth() -> u8 {
    1
}
const fn yes() -> bool {
    true
}
const fn logs_tail() -> u32 {
    200
}
const fn pw_timeout() -> u32 {
    180
}
const fn http_timeout() -> u32 {
    30
}
const fn db_rows() -> u32 {
    200
}
const fn db_timeout() -> u32 {
    10
}

/// Run a command in the workspace (as the agent user, inside this run's environment).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ExecInput {
    /// Shell command, run with `/bin/sh -c`. Exactly one of `command` / `argv`.
    #[schemars(length(min = 1, max = 16384))]
    pub command: Option<String>,
    /// Program and arguments, run directly. Exactly one of `command` / `argv`.
    #[schemars(length(min = 1, max = 256))]
    pub argv: Option<Vec<String>>,
    /// Workspace-relative working directory.
    #[serde(default = "dot")]
    pub cwd: String,
    #[serde(default = "exec_timeout")]
    #[schemars(range(min = 1, max = 900))]
    pub timeout_s: u32,
    /// Extra environment variables (`^[A-Z_][A-Z0-9_]*$`, at most 32; PATH, HOME, MAGE_*, proxy
    /// and loader variables are reserved).
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Bytes written to the process's stdin (at most 1 MiB).
    pub stdin: Option<String>,
}

/// Read a workspace file by lines.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadInput {
    /// Workspace-relative path.
    pub path: String,
    /// 1-based first line.
    #[serde(default = "one")]
    #[schemars(range(min = 1))]
    pub start_line: u32,
    #[serde(default = "read_lines")]
    #[schemars(range(min = 1, max = 2000))]
    pub max_lines: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WriteModeArg {
    /// Fails if the file exists.
    Create,
    #[default]
    Overwrite,
    Append,
}

/// Write a workspace file (missing parent directories are created).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WriteInput {
    /// Workspace-relative path.
    pub path: String,
    /// At most 1 MiB.
    pub content: String,
    #[serde(default)]
    pub mode: WriteModeArg,
}

/// List a workspace directory.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListInput {
    #[serde(default = "dot")]
    pub path: String,
    #[serde(default = "depth")]
    #[schemars(range(min = 1, max = 4))]
    pub depth: u8,
    /// Glob filter (`**`, `*`, `?`).
    pub glob: Option<String>,
    /// Include dotfiles.
    #[serde(default)]
    pub hidden: bool,
    #[serde(default = "yes")]
    pub respect_gitignore: bool,
}

/// Read a log: `prepare`, `build:<step>`, `service:<name>`, `exec:<seq>`, `egress`, or `mcp`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LogsInput {
    pub source: String,
    /// Number of (matching) lines from the end.
    #[serde(default = "logs_tail")]
    #[schemars(range(min = 0, max = 2000))]
    pub tail: u32,
    /// Rust regex, case-sensitive (prefix `(?i)` for insensitive).
    #[schemars(length(max = 512))]
    pub grep: Option<String>,
    /// Cursor from a previous call: only lines written after it.
    pub since: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ServicesAction {
    Status,
    Start,
    Stop,
    Restart,
    Health,
    /// Diagnose, Fix: re-clone the database from the prepared template.
    DbReset,
    /// Learn: check the draft `.mage/env.toml`.
    Validate,
    /// Learn: run the draft recipe's `[[build]]` steps.
    Build,
    /// Learn: create the database, extensions, then migrate and seed.
    DbSetup,
}

impl ServicesAction {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Status => "status",
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "restart",
            Self::Health => "health",
            Self::DbReset => "db_reset",
            Self::Validate => "validate",
            Self::Build => "build",
            Self::DbSetup => "db_setup",
        }
    }
}

/// Inspect and control the recipe's services (Learn: the draft `.mage/env.toml` is re-read and
/// validated first).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ServicesInput {
    pub action: ServicesAction,
    /// Services to act on; default all.
    pub names: Option<Vec<String>>,
    /// `restart` only: rebuild what your changes affect first.
    #[serde(default)]
    pub rebuild: bool,
}

/// Run a Playwright script from the workspace with video, trace, and HAR recording.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlaywrightRunInput {
    /// Workspace path: `*.spec.ts`, `*.spec.js`, `*.spec.mjs`, `*.test.ts`, or `*.test.js`. Import
    /// only `@playwright/test` and Node built-ins; `MAGE_BASE_URL`, `MAGE_PORT_<NAME>`,
    /// `MAGE_LOGIN_*` are set.
    pub script: String,
    /// Test title filter (regular expression).
    #[schemars(length(max = 256))]
    pub grep: Option<String>,
    #[serde(default = "pw_timeout")]
    #[schemars(range(min = 10, max = 600))]
    pub timeout_s: u32,
    /// Record video, trace, and HAR.
    #[serde(default = "yes")]
    pub record: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "UPPERCASE")]
pub enum HttpMethodArg {
    Get,
    Head,
    Post,
    Put,
    Patch,
    Delete,
    Options,
}

impl HttpMethodArg {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Head => "HEAD",
            Self::Post => "POST",
            Self::Put => "PUT",
            Self::Patch => "PATCH",
            Self::Delete => "DELETE",
            Self::Options => "OPTIONS",
        }
    }
}

/// Send an HTTP request to one of this environment's loopback ports (no redirects, no proxy).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HttpRequestInput {
    pub method: HttpMethodArg,
    /// `http://127.0.0.1:<env port>/...` (or `localhost`). Alternative to `port` + `path`.
    pub url: Option<String>,
    /// Named recipe port; the request goes to `http://127.0.0.1:<port><path>`.
    pub port: Option<String>,
    pub path: Option<String>,
    /// At most 32 headers.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// UTF-8 body (at most 1 MiB). Exclusive with `body_base64`.
    pub body: Option<String>,
    pub body_base64: Option<String>,
    #[serde(default = "http_timeout")]
    #[schemars(range(min = 1, max = 60))]
    pub timeout_s: u32,
}

/// Run one SQL statement against this environment's disposable database, as the app's role.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DbQueryInput {
    #[schemars(length(min = 1, max = 65536))]
    pub sql: String,
    /// `$1`, `$2`, … values: null, boolean, integer, number, or string (at most 100).
    #[serde(default)]
    pub params: Vec<serde_json::Value>,
    /// Read-only unless true.
    #[serde(default)]
    pub write: bool,
    #[serde(default = "db_rows")]
    #[schemars(range(min = 1, max = 1000))]
    pub max_rows: u32,
    #[serde(default = "db_timeout")]
    #[schemars(range(min = 1, max = 30))]
    pub timeout_s: u32,
}
