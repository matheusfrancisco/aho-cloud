//! `RunTools` (executor.md "UC3 Tool call", "Run MCP"): the Run MCP tool implementations for one
//! run. The rmcp adapter (`adapters::driving::mcp`) authenticates, then maps `tools/list` to
//! [`RunTools::tool_defs`] and `tools/call` to [`RunTools::call`]; everything else lives here so
//! every executor answers identically.
//!
//! Per call: tool-set check (unknown tool), call budget, argument size cap, `ToolCall` event,
//! the tool itself (cancelled with the MCP request or `shutdown`), output shaping (per-field caps,
//! then the whole-result cap), `ToolResult` event, one line in the env's `mcp.jsonl`.
//!
//! `submit_*` tools post through `ServerLink` and report every outcome to the session task over
//! [`RunToolsInit::submissions`]; while a gate is pending, further submits get "gate in progress".

mod inputs;
mod schema;
pub mod shape;
mod submit;
mod workspace;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Instant;

use mage_core::domain::{ArtifactId, ArtifactKind, CommitSha, OrgId, RepoId, RunId, RunKind};
use mage_core::ports::{Clock, IdGen};
use mage_core::wire::mcp::{
    MAX_INPUT_BYTES, SUBMIT_DIAGNOSIS_SCHEMA, SUBMIT_FIX_SCHEMA, SUBMIT_RECIPE_SCHEMA, SUBMIT_TRIAGE_SCHEMA,
};
use mage_core::wire::runner::{FixSpec, OutboundEventKind, Rejection, SubmitAck, WireRunResult};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use time::format_description::well_known::Rfc3339;
use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;

pub use self::inputs::*;
pub use self::schema::{Schema, SchemaViolation};
use crate::app::runner::artifacts::upload_file;
use crate::error::AppError;
use crate::ports::browser::{BrowserError, BrowserRunner, LocalArtifact};
use crate::ports::executor::{Env, ExecError};
use crate::ports::link::{LinkConflict, LinkError, ServerLink};

/// Event and MCP-log preview cap.
pub const PREVIEW_BYTES: usize = 4 * 1024;

/// One Run MCP tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RunTool {
    Exec,
    Read,
    Write,
    List,
    Logs,
    Services,
    PlaywrightRun,
    HttpRequest,
    DbQuery,
    SubmitTriage,
    SubmitRecipe,
    SubmitDiagnosis,
    SubmitFix,
}

const ALL_TOOLS: [RunTool; 13] = [
    RunTool::Exec,
    RunTool::Read,
    RunTool::Write,
    RunTool::List,
    RunTool::Logs,
    RunTool::Services,
    RunTool::PlaywrightRun,
    RunTool::HttpRequest,
    RunTool::DbQuery,
    RunTool::SubmitTriage,
    RunTool::SubmitRecipe,
    RunTool::SubmitDiagnosis,
    RunTool::SubmitFix,
];

impl RunTool {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Exec => "exec",
            Self::Read => "read",
            Self::Write => "write",
            Self::List => "list",
            Self::Logs => "logs",
            Self::Services => "services",
            Self::PlaywrightRun => "playwright_run",
            Self::HttpRequest => "http_request",
            Self::DbQuery => "db_query",
            Self::SubmitTriage => mage_core::wire::mcp::SUBMIT_TRIAGE,
            Self::SubmitRecipe => mage_core::wire::mcp::SUBMIT_RECIPE,
            Self::SubmitDiagnosis => mage_core::wire::mcp::SUBMIT_DIAGNOSIS,
            Self::SubmitFix => mage_core::wire::mcp::SUBMIT_FIX,
        }
    }

    pub fn from_name(name: &str) -> Option<RunTool> {
        ALL_TOOLS.into_iter().find(|t| t.name() == name)
    }

    pub const fn is_submit(self) -> bool {
        matches!(self, Self::SubmitTriage | Self::SubmitRecipe | Self::SubmitDiagnosis | Self::SubmitFix)
    }

    pub const fn description(self) -> &'static str {
        match self {
            Self::Exec => "Run a command in the workspace. A non-zero exit is a normal result. Output is capped (8 KiB head + 24 KiB tail per stream); the full output is in logs source=exec:<seq>.",
            Self::Read => "Read a workspace file by lines (at most 2000 lines or 256 KiB per call; follow next_line).",
            Self::Write => "Write a workspace file; missing parent directories are created.",
            Self::List => "List a workspace directory (depth 1-4, at most 1000 entries).",
            Self::Logs => "Read a log: prepare, build:<step>, service:<name>, exec:<seq>, egress, or mcp. Use the returned cursor as since to read only new lines.",
            Self::Services => "Inspect and control the recipe's services: status, start, stop, restart, health; db_reset (Diagnose, Fix); validate, build, db_setup (Learn, using the draft .mage/env.toml).",
            Self::PlaywrightRun => "Run a Playwright script from the workspace against the running app, with video, trace, and HAR. Returns the status, failures, HTTP error statuses, and artifact ids.",
            Self::HttpRequest => "Send an HTTP request to one of this environment's ports on 127.0.0.1 (no redirects).",
            Self::DbQuery => "Run one SQL statement on this environment's disposable database (read-only unless write is true).",
            Self::SubmitTriage => "Submit your triage: the likely causes with code citations. On a tool error, fix every listed problem and resubmit the complete list.",
            Self::SubmitRecipe => "Submit the recipe (.mage/env.toml) for the boot gate. Keep the workspace file in sync. End your turn after a successful submit.",
            Self::SubmitDiagnosis => "Submit your diagnosis. For reproduced, the script runs here once first and must show the signature; the repro gate then re-runs it in fresh environments. End your turn after a successful submit.",
            Self::SubmitFix => "Submit your fix. Mage computes the patch from your working tree and re-checks it in a fresh environment. End your turn after a successful submit.",
        }
    }
}

/// `ToolSet::for_kind(kind)`: the tools a run of `kind` serves. Verify runs have no Run MCP.
pub struct ToolSet;

impl ToolSet {
    pub const fn for_kind(kind: RunKind) -> &'static [RunTool] {
        use RunTool::*;
        match kind {
            RunKind::Triage => &[SubmitTriage],
            RunKind::Learn => &[Exec, Read, Write, List, Logs, Services, SubmitRecipe],
            RunKind::Diagnose => &[Exec, Read, Write, List, Logs, Services, PlaywrightRun, HttpRequest, DbQuery, SubmitDiagnosis],
            RunKind::Fix => &[Exec, Read, Write, List, Logs, Services, PlaywrightRun, HttpRequest, DbQuery, SubmitFix],
            RunKind::Verify => &[],
        }
    }
}

/// A tool as listed by `tools/list`.
#[derive(Debug, Clone)]
pub struct ToolDef {
    pub name: &'static str,
    pub description: &'static str,
    pub input_schema: Arc<Map<String, Value>>,
}

static TRIAGE_SCHEMA: LazyLock<Result<Schema, String>> = LazyLock::new(|| Schema::compile(SUBMIT_TRIAGE_SCHEMA));
static RECIPE_SCHEMA: LazyLock<Result<Schema, String>> = LazyLock::new(|| Schema::compile(SUBMIT_RECIPE_SCHEMA));
static DIAGNOSIS_SCHEMA: LazyLock<Result<Schema, String>> = LazyLock::new(|| Schema::compile(SUBMIT_DIAGNOSIS_SCHEMA));
static FIX_SCHEMA: LazyLock<Result<Schema, String>> = LazyLock::new(|| Schema::compile(SUBMIT_FIX_SCHEMA));

fn submit_schema(tool: RunTool) -> Option<&'static Result<Schema, String>> {
    match tool {
        RunTool::SubmitTriage => Some(&TRIAGE_SCHEMA),
        RunTool::SubmitRecipe => Some(&RECIPE_SCHEMA),
        RunTool::SubmitDiagnosis => Some(&DIAGNOSIS_SCHEMA),
        RunTool::SubmitFix => Some(&FIX_SCHEMA),
        _ => None,
    }
}

fn derived_schema<T: JsonSchema>() -> Map<String, Value> {
    let schema = schemars::generate::SchemaSettings::draft2020_12().into_generator().into_root_schema_for::<T>();
    match serde_json::to_value(schema) {
        Ok(Value::Object(mut map)) => {
            map.remove("title");
            map.remove("description");
            map
        }
        _ => Map::new(),
    }
}

/// The `inputSchema` served for `tool`.
pub fn input_schema(tool: RunTool) -> Map<String, Value> {
    match tool {
        RunTool::Exec => derived_schema::<ExecInput>(),
        RunTool::Read => derived_schema::<ReadInput>(),
        RunTool::Write => derived_schema::<WriteInput>(),
        RunTool::List => derived_schema::<ListInput>(),
        RunTool::Logs => derived_schema::<LogsInput>(),
        RunTool::Services => derived_schema::<ServicesInput>(),
        RunTool::PlaywrightRun => derived_schema::<PlaywrightRunInput>(),
        RunTool::HttpRequest => derived_schema::<HttpRequestInput>(),
        RunTool::DbQuery => derived_schema::<DbQueryInput>(),
        submit => match submit_schema(submit) {
            Some(Ok(schema)) => schema.as_object(),
            _ => Map::new(),
        },
    }
}

/// `[runner.mcp]` plus the precheck timeout (`[runner.verify].script_timeout_s`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct RunToolsConfig {
    /// Whole tool result, serialized.
    pub max_response_bytes: usize,
    /// Tool calls per run; afterwards every tool answers "tool budget exhausted".
    pub max_calls: u32,
    /// `playwright_run` artifact uploads per run.
    pub max_explore_uploads: u32,
    /// `submit_diagnosis` precheck of a Shell script.
    pub script_timeout_s: u64,
}

impl Default for RunToolsConfig {
    fn default() -> Self {
        Self { max_response_bytes: 64 * 1024, max_calls: 2000, max_explore_uploads: 20, script_timeout_s: 300 }
    }
}

/// Run context the session task hands to `RunTools`.
pub struct RunToolsInit {
    pub run_id: RunId,
    pub org_id: OrgId,
    pub kind: RunKind,
    /// Lease fencing token, echoed on every `ServerLink` call.
    pub attempt: u16,
    /// `repo` / `sha` of the run (citations in `submit_diagnosis.fix_sites`); `None` for Triage.
    pub repo_id: Option<RepoId>,
    pub sha: Option<CommitSha>,
    /// The run's env (`empty_env` for Triage).
    pub env: Option<Arc<dyn Env>>,
    /// Fix runs: exclusions and deny rules for `submit_fix`.
    pub fix: Option<FixSpec>,
    /// Runner-local dir for files awaiting upload.
    pub outbox: PathBuf,
    /// `ToolCall` / `ToolResult` / `Notice` events for the event pump.
    pub events: mpsc::UnboundedSender<OutboundEventKind>,
    /// Every submit outcome, plus lease loss seen by any tool.
    pub submissions: mpsc::UnboundedSender<SubmitOutcome>,
}

/// Ports `RunTools` calls besides the env.
#[derive(Clone)]
pub struct RunToolsDeps {
    pub browser: Arc<dyn BrowserRunner>,
    pub link: Arc<dyn ServerLink>,
    pub clock: Arc<dyn Clock>,
    pub ids: Arc<dyn IdGen>,
}

/// What a `submit_*` call (or any tool that met a 409) tells the session task.
#[derive(Debug, Clone, PartialEq)]
pub enum SubmitOutcome {
    /// `POST /result` answered 200. `ack.status` says whether a gate follows (`AwaitingGate`)
    /// or the run is done (`Succeeded`).
    Accepted { ack: SubmitAck, result: WireRunResult },
    /// 422: the agent got the feedback as a tool error; the run stays `Running` unless the
    /// server reports otherwise.
    Rejected(Rejection),
    /// 409: `LeaseLost` → self-fence; `RunTerminal` / `InvalidState` → follow the status.
    Conflict(LinkConflict),
}

/// A tool-level error: the call reached the tool, which answers with `isError: true`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError(pub String);

impl ToolError {
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl std::fmt::Display for ToolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Log tails carried by build and health failures, as shown to the agent.
const ERROR_LOG_TAIL_BYTES: usize = 8 * 1024;

impl From<ExecError> for ToolError {
    fn from(err: ExecError) -> Self {
        Self(match err {
            ExecError::Confinement { .. } => "path escapes the workspace".to_owned(),
            ExecError::NotFound(m) => format!("not found: {m}"),
            ExecError::Invalid(m) => m,
            ExecError::Destroyed => "the environment was destroyed".to_owned(),
            ExecError::PrepareFailed { step, exit, log_tail } => {
                let exit = exit.map_or_else(String::new, |e| format!(" ({e})"));
                format!("step {step} failed{exit}:\n{}", shape::clean_text(shape::tail_bytes(&log_tail, ERROR_LOG_TAIL_BYTES)))
            }
            ExecError::ServiceUnhealthy { service, reason, log_tail } => format!(
                "service {service} unhealthy: {reason}\n{}",
                shape::clean_text(shape::tail_bytes(&log_tail, ERROR_LOG_TAIL_BYTES))
            ),
            other => other.to_string(),
        })
    }
}

impl From<BrowserError> for ToolError {
    fn from(err: BrowserError) -> Self {
        match err {
            BrowserError::NotFound(m) | BrowserError::Invalid(m) => Self(m),
            BrowserError::Unavailable(m) => Self(format!("Playwright is unavailable on this runner: {m}")),
            BrowserError::Conflict(m) => Self(m),
            BrowserError::Env(e) => Self::from(e),
        }
    }
}

type ToolOut = Result<Value, ToolError>;

/// `tools/call` with a name outside this run's tool set.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown tool: {0}")]
pub struct UnknownTool(pub String);

/// A finished tool call: `value` is the structured result (`Null` for errors), `text` what the
/// agent reads (serialized JSON, the plain message for `submit_triage`, or the error message).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolResponse {
    pub ok: bool,
    pub value: Value,
    pub text: String,
}

#[derive(Debug, Default)]
struct ToolState {
    gate_pending: bool,
    submit_seq: u32,
    explore_seq: u32,
    precheck_seq: u32,
    explore_uploads: u32,
}

pub struct RunTools {
    run_id: RunId,
    org_id: OrgId,
    kind: RunKind,
    attempt: u16,
    repo_id: Option<RepoId>,
    sha: Option<CommitSha>,
    env: Option<Arc<dyn Env>>,
    fix: Option<FixSpec>,
    outbox: PathBuf,
    events: mpsc::UnboundedSender<OutboundEventKind>,
    submissions: mpsc::UnboundedSender<SubmitOutcome>,
    deps: RunToolsDeps,
    config: RunToolsConfig,
    state: parking_lot::Mutex<ToolState>,
    calls: AtomicU32,
    exec_slots: Semaphore,
    playwright_slots: Semaphore,
    db_slots: Semaphore,
    services_slots: Semaphore,
    submit_slot: Semaphore,
    shutdown: CancellationToken,
    http: reqwest::Client,
}

impl RunTools {
    pub fn new(init: RunToolsInit, deps: RunToolsDeps, config: RunToolsConfig) -> Result<RunTools, AppError> {
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| AppError::Internal(format!("http client: {e}")))?;
        Ok(RunTools {
            run_id: init.run_id,
            org_id: init.org_id,
            kind: init.kind,
            attempt: init.attempt,
            repo_id: init.repo_id,
            sha: init.sha,
            env: init.env,
            fix: init.fix,
            outbox: init.outbox,
            events: init.events,
            submissions: init.submissions,
            deps,
            config,
            state: parking_lot::Mutex::new(ToolState::default()),
            calls: AtomicU32::new(0),
            exec_slots: Semaphore::new(4),
            playwright_slots: Semaphore::new(1),
            db_slots: Semaphore::new(2),
            services_slots: Semaphore::new(1),
            submit_slot: Semaphore::new(1),
            shutdown: CancellationToken::new(),
            http,
        })
    }

    pub fn run_id(&self) -> RunId {
        self.run_id
    }

    pub fn org_id(&self) -> OrgId {
        self.org_id
    }

    pub fn kind(&self) -> RunKind {
        self.kind
    }

    pub fn tools(&self) -> &'static [RunTool] {
        ToolSet::for_kind(self.kind)
    }

    /// Exact tool names served for this run (harness inventory check).
    pub fn tool_names(&self) -> Vec<&'static str> {
        self.tools().iter().map(|t| t.name()).collect()
    }

    pub fn tool_defs(&self) -> Vec<ToolDef> {
        self.tools()
            .iter()
            .map(|t| ToolDef { name: t.name(), description: t.description(), input_schema: Arc::new(input_schema(*t)) })
            .collect()
    }

    /// The session task received `GateFeedback`: submits are accepted again.
    pub fn gate_feedback_received(&self) {
        self.state.lock().gate_pending = false;
    }

    /// True while a submitted result awaits its gate.
    pub fn gate_pending(&self) -> bool {
        self.state.lock().gate_pending
    }

    /// Cancels every in-flight call (dropping its exec kills the process group) and refuses new
    /// ones. Idempotent.
    pub fn shutdown(&self) {
        self.shutdown.cancel();
    }

    pub fn is_shut_down(&self) -> bool {
        self.shutdown.is_cancelled()
    }

    /// One `tools/call`. `cancel` fires when the MCP request is cancelled.
    pub async fn call(&self, name: &str, args: Value, cancel: CancellationToken) -> Result<ToolResponse, UnknownTool> {
        let Some(tool) = RunTool::from_name(name).filter(|t| self.tools().contains(t)) else {
            return Err(UnknownTool(name.to_owned()));
        };
        let started = Instant::now();
        let args_preview = shape::preview(&args, PREVIEW_BYTES);
        let event_name = format!("run.{}", tool.name());
        self.emit(OutboundEventKind::ToolCall { name: event_name.clone(), args_preview: args_preview.clone() });

        let result = if self.shutdown.is_cancelled() {
            Err(ToolError::new("the run is shutting down"))
        } else if self.calls.fetch_add(1, Ordering::Relaxed) >= self.config.max_calls {
            Err(ToolError::new("tool budget exhausted"))
        } else if shape::json_len(&args) > MAX_INPUT_BYTES {
            Err(ToolError::new(format!("arguments are larger than {} KiB", MAX_INPUT_BYTES / 1024)))
        } else {
            tokio::select! {
                r = self.dispatch(tool, args) => r,
                () = cancel.cancelled() => Err(ToolError::new("cancelled")),
                () = self.shutdown.cancelled() => Err(ToolError::new("the run is shutting down")),
            }
        };

        let response = match result {
            Ok(mut value) => {
                shape::cap_result(&mut value, self.config.max_response_bytes);
                let text = match &value {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                ToolResponse { ok: true, value, text }
            }
            Err(ToolError(message)) => {
                let message = shape::truncate_bytes(&message, self.config.max_response_bytes).to_owned();
                ToolResponse { ok: false, value: Value::Null, text: message }
            }
        };
        let result_preview = shape::truncate_bytes(&response.text, PREVIEW_BYTES).to_owned();
        self.emit(OutboundEventKind::ToolResult { name: event_name, ok: response.ok, preview: result_preview.clone() });
        self.record(tool, response.ok, started, &args_preview, &result_preview).await;
        Ok(response)
    }

    async fn dispatch(&self, tool: RunTool, args: Value) -> ToolOut {
        if let Some(schema) = submit_schema(tool) {
            let schema = schema.as_ref().map_err(|e| ToolError::new(format!("internal: {e}")))?;
            let violations = schema.validate(&args);
            if !violations.is_empty() {
                let list: Vec<String> = violations.iter().map(|v| format!("- {v}")).collect();
                return Err(ToolError::new(format!("{} arguments do not match its schema:\n{}", tool.name(), list.join("\n"))));
            }
        }
        match tool {
            RunTool::Exec => self.exec(parse(args)?).await,
            RunTool::Read => self.read(parse(args)?).await,
            RunTool::Write => self.write(parse(args)?).await,
            RunTool::List => self.list(parse(args)?).await,
            RunTool::Logs => self.logs(parse(args)?).await,
            RunTool::Services => self.services(parse(args)?).await,
            RunTool::PlaywrightRun => self.playwright_run(parse(args)?).await,
            RunTool::HttpRequest => self.http_request(parse(args)?).await,
            RunTool::DbQuery => self.db_query(parse(args)?).await,
            RunTool::SubmitTriage => self.submit_triage(parse(args)?).await,
            RunTool::SubmitRecipe => self.submit_recipe(parse(args)?).await,
            RunTool::SubmitDiagnosis => self.submit_diagnosis(parse(args)?).await,
            RunTool::SubmitFix => self.submit_fix(parse(args)?).await,
        }
    }

    fn env(&self) -> Result<&Arc<dyn Env>, ToolError> {
        self.env.as_ref().ok_or_else(|| ToolError::new("this run has no environment"))
    }

    fn emit(&self, kind: OutboundEventKind) {
        // The pump may already be gone during teardown.
        let _ = self.events.send(kind);
    }

    fn report(&self, outcome: SubmitOutcome) {
        let _ = self.submissions.send(outcome);
    }

    /// Maps a link failure for a non-submit tool, reporting a lost lease to the session task.
    fn link_error(&self, err: LinkError) -> ToolError {
        if let LinkError::Conflict(conflict) = err {
            self.report(SubmitOutcome::Conflict(conflict));
            return ToolError::new("run cancelled");
        }
        ToolError::new(format!("upload failed: {err}"))
    }

    /// UC3 step 6: one JSON line per call in the env's `mcp.jsonl`.
    async fn record(&self, tool: RunTool, ok: bool, started: Instant, args: &str, result: &str) {
        let Some(env) = &self.env else { return };
        let at = self.deps.clock.now().format(&Rfc3339).unwrap_or_default();
        let line = json!({
            "at": at,
            "tool": tool.name(),
            "ok": ok,
            "duration_ms": u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            "args": args,
            "result": result,
        });
        // A destroyed env (teardown racing a late call) has nowhere to log.
        let _ = env.record_mcp(line.to_string().as_bytes()).await;
    }

    /// Uploads a local file and deletes it after success.
    async fn upload_local(&self, artifact: &LocalArtifact) -> Result<ArtifactId, LinkError> {
        let id = upload_file(
            self.deps.link.as_ref(),
            self.run_id,
            self.attempt,
            artifact.kind,
            &artifact.name,
            &artifact.content_type,
            &artifact.path,
        )
        .await?;
        remove_quietly(&artifact.path).await;
        Ok(id)
    }

    /// Writes `bytes` to the outbox and uploads them.
    async fn upload_bytes(&self, kind: ArtifactKind, name: &str, content_type: &str, bytes: &[u8]) -> Result<ArtifactId, ToolError> {
        tokio::fs::create_dir_all(&self.outbox).await.map_err(|e| ToolError::new(format!("outbox: {e}")))?;
        let path = self.outbox.join(name);
        tokio::fs::write(&path, bytes).await.map_err(|e| ToolError::new(format!("outbox: {e}")))?;
        let result = upload_file(self.deps.link.as_ref(), self.run_id, self.attempt, kind, name, content_type, &path).await;
        remove_quietly(&path).await;
        result.map_err(|e| self.link_error(e))
    }
}

fn parse<T: serde::de::DeserializeOwned>(args: Value) -> Result<T, ToolError> {
    let args = if args.is_null() { Value::Object(Map::new()) } else { args };
    serde_json::from_value(args).map_err(|e| ToolError::new(format!("invalid arguments: {e}")))
}

async fn remove_quietly(path: &std::path::Path) {
    // Already gone is fine; anything else leaves a file the run dir cleanup removes.
    let _ = tokio::fs::remove_file(path).await;
}

/// Artifact name for a file the agent named: lowercase `[a-z0-9._-]`, starting alphanumeric,
/// ≤ 128 bytes keeping the end (the extension decides `ScriptKind`).
pub fn artifact_name(raw: &str) -> String {
    let mapped: String = raw
        .chars()
        .map(|c| {
            let c = c.to_ascii_lowercase();
            if c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-') { c } else { '-' }
        })
        .collect();
    let trimmed = mapped.trim_start_matches(|c: char| !c.is_ascii_alphanumeric());
    let name = if trimmed.len() > 128 { &trimmed[trimmed.len() - 128..] } else { trimmed };
    let name = name.trim_start_matches(|c: char| !c.is_ascii_alphanumeric());
    if name.is_empty() { "file".to_owned() } else { name.to_owned() }
}

#[cfg(test)]
mod tests;
