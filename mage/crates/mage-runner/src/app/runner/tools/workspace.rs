//! Environment tools: `exec`, `read`, `write`, `list`, `logs`, `services`, `playwright_run`,
//! `http_request`, `db_query` (executor.md "Tool schemas"). Paths are workspace-relative and
//! parsed into `EnvPath::ws`; the `Env` enforces confinement beneath the area root.

use std::collections::BTreeMap;
use std::str::FromStr;
use std::time::{Duration, Instant};

use base64::Engine as _;
use mage_core::domain::recipe::{RecipeIssue, RecipeReport, RecipeSpec};
use mage_core::domain::{RunKind, ScriptKind};
use serde_json::{Map, Value, json};

use super::inputs::*;
use super::shape::{clean_bytes, clean_text, join_captured, truncate_bytes};
use super::{RunTools, ToolError, ToolOut};
use crate::ports::browser::{BrowserScript, LocalArtifact, RecordOpts, ScreenshotMode, ScriptSource};
use crate::ports::executor::{
    Baseline, Command, DbQuery, DbRows, DbValue, DiffOptions, EntryKind, Env, EnvPath, ExecSpec, Exit, HealthReport, ListOpts, LogCursor,
    LogQuery, LogSource, PathGlob, PrepareObserver, ProcessRole, ReadRange, RebuildScope, RelPath, ServiceSelector, ServiceState,
    ServicesReport, StepStatus, WriteMode, is_reserved_env_name,
};

const READ_MAX_BYTES: u64 = 256 * 1024;
const READ_LINE_MAX_CHARS: usize = 2000;
const LIST_MAX_ENTRIES: u32 = 1000;
const LOGS_MAX_BYTES: usize = 64 * 1024;
/// Room left for the `logs` result fields besides `lines`.
const LOGS_ENVELOPE_BYTES: usize = 1024;
const LOG_REGEX_SIZE_LIMIT: usize = 1 << 20;
const HTTP_RESPONSE_BODY_MAX: usize = 32 * 1024;
const DB_CELL_MAX_BYTES: usize = 4 * 1024;
const DB_RESULT_MAX_BYTES: usize = 64 * 1024;
const PW_FAILURES_MAX: usize = 10;
const PW_STATUSES_MAX: usize = 50;
/// The draft recipe Learn iterates on.
pub const RECIPE_PATH: &str = ".mage/env.toml";
const RECIPE_READ_MAX: u64 = mage_core::domain::recipe::RECIPE_MAX_BYTES as u64 + 1;

fn ms(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

fn env_name_ok(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_uppercase() || c == '_')
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// `- path: [code] message` per issue.
pub(super) fn render_issues(issues: &[RecipeIssue]) -> String {
    issues
        .iter()
        .map(|i| {
            let code = serde_json::to_value(i.code).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default();
            let at = if i.path.is_empty() { "recipe".to_owned() } else { i.path.clone() };
            format!("- {at}: [{code}] {}", i.message)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn issue_json(issue: &RecipeIssue, severity: &str) -> Value {
    json!({ "path": issue.path, "code": issue.code, "message": issue.message, "severity": severity })
}

#[derive(Default)]
struct ExploreTally {
    skipped: u32,
    errors: Vec<String>,
}

/// Collects `PrepareObserver` step reports for the `services` output.
#[derive(Default)]
struct StepLog(parking_lot::Mutex<Vec<(String, StepStatus)>>);

impl PrepareObserver for StepLog {
    fn step(&self, step: &str, status: StepStatus) {
        let mut steps = self.0.lock();
        match steps.iter_mut().find(|(name, _)| name == step) {
            Some(entry) => entry.1 = status,
            None => steps.push((step.to_owned(), status)),
        }
    }

    fn line(&self, _step: &str, _line: &str) {}
}

impl StepLog {
    fn to_json(&self, skipped: &[String]) -> Vec<Value> {
        let mut out: Vec<Value> = self
            .0
            .lock()
            .iter()
            .map(|(name, status)| {
                let (status, secs) = match status {
                    StepStatus::Started => ("started", None),
                    StepStatus::Ok { secs } => ("ok", Some(*secs)),
                    StepStatus::Failed { .. } => ("failed", None),
                };
                json!({ "name": name, "status": status, "secs": secs, "log": format!("build:{name}") })
            })
            .collect();
        out.extend(skipped.iter().map(|name| json!({ "name": name, "status": "skipped", "secs": null, "log": format!("build:{name}") })));
        out
    }
}

fn state_json(state: &ServiceState) -> (&'static str, Option<String>) {
    match state {
        ServiceState::Stopped => ("stopped", None),
        ServiceState::Starting => ("starting", None),
        ServiceState::Healthy => ("healthy", None),
        ServiceState::Unhealthy { reason } => ("unhealthy", Some(reason.clone())),
        ServiceState::Exited { exit } => ("exited", Some(exit.to_string())),
    }
}

impl RunTools {
    pub(super) async fn exec(&self, input: ExecInput) -> ToolOut {
        let env = self.env()?;
        let command = match (input.command, input.argv) {
            (Some(cmd), None) => {
                if cmd.is_empty() || cmd.len() > EXEC_COMMAND_MAX {
                    return Err(ToolError::new(format!("command must be 1..={EXEC_COMMAND_MAX} bytes")));
                }
                Command::Shell(cmd)
            }
            (None, Some(argv)) => {
                if argv.is_empty() || argv.len() > EXEC_ARGV_MAX {
                    return Err(ToolError::new(format!("argv must have 1..={EXEC_ARGV_MAX} entries")));
                }
                Command::Argv(argv)
            }
            _ => return Err(ToolError::new("exactly one of command or argv is required")),
        };
        if !(1..=EXEC_TIMEOUT_MAX_S).contains(&input.timeout_s) {
            return Err(ToolError::new(format!("timeout_s must be 1..={EXEC_TIMEOUT_MAX_S}")));
        }
        if input.env.len() > EXEC_ENV_MAX {
            return Err(ToolError::new(format!("at most {EXEC_ENV_MAX} env entries")));
        }
        for (name, value) in &input.env {
            if !env_name_ok(name) {
                return Err(ToolError::new(format!("env name {name:?} must match ^[A-Z_][A-Z0-9_]*$")));
            }
            if is_reserved_env_name(name) {
                return Err(ToolError::new(format!("env name {name} is reserved")));
            }
            if value.len() > EXEC_ENV_VALUE_MAX {
                return Err(ToolError::new(format!("env value of {name} is longer than {EXEC_ENV_VALUE_MAX} bytes")));
            }
        }
        if input.stdin.as_ref().is_some_and(|s| s.len() > STDIN_MAX) {
            return Err(ToolError::new("stdin is larger than 1 MiB"));
        }
        let mut spec = match command {
            Command::Shell(cmd) => ExecSpec::shell(cmd, ProcessRole::Exec),
            Command::Argv(argv) => ExecSpec::argv(argv, ProcessRole::Exec),
        };
        spec.cwd = EnvPath::ws(&input.cwd)?;
        spec.env = input.env.into_iter().collect();
        spec.stdin = input.stdin.map(String::into_bytes);
        spec.timeout = Duration::from_secs(u64::from(input.timeout_s));
        let _slot = self.exec_slots.acquire().await.map_err(|_| ToolError::new("the run is shutting down"))?;
        let out = env.exec(spec, None).await?;
        let log = out.log.to_string();
        let (exit_code, signal) = match out.exit {
            Exit::Code(c) => (Some(c), None),
            Exit::Signal(s) => (None, Some(s)),
            Exit::TimedOut | Exit::Killed(_) => (None, None),
        };
        Ok(json!({
            "exit_code": exit_code,
            "signal": signal,
            "timed_out": out.exit == Exit::TimedOut,
            "duration_ms": ms(out.duration),
            "stdout": join_captured(&out.stdout.head, &out.stdout.tail, out.stdout.omitted(), &log),
            "stderr": join_captured(&out.stderr.head, &out.stderr.tail, out.stderr.omitted(), &log),
            "stdout_bytes": out.stdout.total_bytes,
            "stderr_bytes": out.stderr.total_bytes,
            "truncated": out.stdout.omitted() > 0 || out.stderr.omitted() > 0,
            "log": log,
        }))
    }

    pub(super) async fn read(&self, input: ReadInput) -> ToolOut {
        let env = self.env()?;
        if input.start_line == 0 {
            return Err(ToolError::new("start_line is 1-based"));
        }
        if !(1..=READ_LINES_MAX).contains(&input.max_lines) {
            return Err(ToolError::new(format!("max_lines must be 1..={READ_LINES_MAX}")));
        }
        let path = EnvPath::ws(&input.path)?;
        let read = env
            .read(&path, ReadRange::Lines { start: input.start_line, count: input.max_lines, max_bytes: READ_MAX_BYTES })
            .await?;
        let sha256 = hex::encode(read.sha256);
        if read.binary {
            return Ok(json!({
                "path": path.rel.as_str(), "content": "", "start_line": input.start_line, "end_line": input.start_line.saturating_sub(1),
                "total_lines": read.total_lines, "truncated": false, "next_line": null, "sha256": sha256, "binary": true,
            }));
        }
        let text = String::from_utf8_lossy(&read.bytes);
        let mut lines: Vec<&str> = text.split_inclusive('\n').collect();
        // A byte cap may end mid-line: hand that line out whole on the next call.
        if read.truncated && lines.len() > 1 && !text.ends_with('\n') {
            lines.pop();
        }
        let mut content = String::with_capacity(text.len());
        for line in &lines {
            let (body, nl) = line.strip_suffix('\n').map_or((*line, ""), |b| (b, "\n"));
            if body.chars().count() > READ_LINE_MAX_CHARS {
                content.extend(body.chars().take(READ_LINE_MAX_CHARS));
                content.push_str("…[line truncated]");
            } else {
                content.push_str(body);
            }
            content.push_str(nl);
        }
        let n = u32::try_from(lines.len()).unwrap_or(u32::MAX);
        let end_line = (input.start_line + n).saturating_sub(1);
        let total_lines = read.total_lines.unwrap_or(end_line);
        let next_line = (end_line < total_lines).then_some(end_line + 1);
        Ok(json!({
            "path": path.rel.as_str(),
            "content": clean_text(&content),
            "start_line": input.start_line,
            "end_line": end_line,
            "total_lines": total_lines,
            "truncated": read.truncated || next_line.is_some(),
            "next_line": next_line,
            "sha256": sha256,
            "binary": false,
        }))
    }

    pub(super) async fn write(&self, input: WriteInput) -> ToolOut {
        let env = self.env()?;
        if input.content.len() > WRITE_CONTENT_MAX {
            return Err(ToolError::new("content is larger than 1 MiB"));
        }
        let path = EnvPath::ws(&input.path)?;
        let mode = match input.mode {
            WriteModeArg::Create => WriteMode::Create,
            WriteModeArg::Overwrite => WriteMode::Overwrite,
            WriteModeArg::Append => WriteMode::Append,
        };
        let report = env.write(&path, input.content.as_bytes(), mode).await?;
        Ok(json!({ "path": path.rel.as_str(), "bytes": report.bytes, "created": report.created }))
    }

    pub(super) async fn list(&self, input: ListInput) -> ToolOut {
        let env = self.env()?;
        if !(1..=4).contains(&input.depth) {
            return Err(ToolError::new("depth must be 1..=4"));
        }
        let path = EnvPath::ws(&input.path)?;
        let glob = input.glob.as_deref().map(PathGlob::new).transpose()?;
        let listing = env
            .list(
                &path,
                ListOpts { depth: input.depth, glob, hidden: input.hidden, respect_gitignore: input.respect_gitignore, max_entries: LIST_MAX_ENTRIES },
            )
            .await?;
        let entries: Vec<Value> = listing
            .entries
            .iter()
            .take(LIST_MAX_ENTRIES as usize)
            .map(|e| {
                let kind = match e.kind {
                    EntryKind::File => "file",
                    EntryKind::Dir => "dir",
                    EntryKind::Symlink => "symlink",
                };
                json!({ "path": e.path.as_str(), "kind": kind, "bytes": e.bytes })
            })
            .collect();
        let truncated = listing.truncated || listing.entries.len() > LIST_MAX_ENTRIES as usize;
        Ok(json!({ "entries": entries, "truncated": truncated }))
    }

    pub(super) async fn logs(&self, input: LogsInput) -> ToolOut {
        let env = self.env()?;
        let source = LogSource::from_str(&input.source)?;
        if input.tail > LOGS_TAIL_MAX {
            return Err(ToolError::new(format!("tail must be 0..={LOGS_TAIL_MAX}")));
        }
        if let Some(grep) = &input.grep {
            if grep.chars().count() > GREP_MAX {
                return Err(ToolError::new(format!("grep is longer than {GREP_MAX} characters")));
            }
            regex::RegexBuilder::new(grep)
                .size_limit(LOG_REGEX_SIZE_LIMIT)
                .build()
                .map_err(|e| ToolError::new(format!("invalid grep regex: {e}")))?;
        }
        let since = match &input.since {
            Some(c) => Some(LogCursor(c.trim().parse::<u64>().map_err(|_| ToolError::new(format!("invalid cursor {c:?}")))?)),
            None => None,
        };
        let slice = env.logs(LogQuery { source, tail: input.tail, grep: input.grep.clone(), since }).await?;
        let mut lines: Vec<String> = slice.lines.iter().map(|l| clean_text(l)).collect();
        let mut truncated = slice.truncated;
        if lines.len() > LOGS_TAIL_MAX as usize {
            lines.drain(..lines.len() - LOGS_TAIL_MAX as usize);
            truncated = true;
        }
        // Oldest lines go first. Sizes are serialized (escaped) bytes, and the envelope keeps
        // room so the whole-result cap never has to cut the newest lines.
        let size = |l: &String| super::shape::json_len(&Value::String(l.clone())) + 1;
        let budget = LOGS_MAX_BYTES - LOGS_ENVELOPE_BYTES;
        let mut total: usize = lines.iter().map(size).sum();
        let mut drop = 0;
        while total > budget && drop < lines.len() {
            total -= size(&lines[drop]);
            drop += 1;
        }
        if drop > 0 {
            lines.drain(..drop);
            truncated = true;
        }
        Ok(json!({
            "source": input.source,
            "lines": lines,
            "cursor": slice.cursor.0.to_string(),
            "matched": slice.matched,
            "truncated": truncated,
        }))
    }

    pub(super) async fn services(&self, input: ServicesInput) -> ToolOut {
        let env = self.env()?;
        let learn = self.kind == RunKind::Learn;
        let allowed = match input.action {
            ServicesAction::Status | ServicesAction::Start | ServicesAction::Stop | ServicesAction::Restart | ServicesAction::Health => true,
            ServicesAction::DbReset => matches!(self.kind, RunKind::Diagnose | RunKind::Fix),
            ServicesAction::Validate | ServicesAction::Build | ServicesAction::DbSetup => learn,
        };
        if !allowed {
            return Err(ToolError::new(format!("services action {} is not available in this run", input.action.as_str())));
        }
        let _slot = self.services_slots.acquire().await.map_err(|_| ToolError::new("the run is shutting down"))?;
        let selector = match &input.names {
            Some(names) if !names.is_empty() => ServiceSelector::Named(names.clone()),
            _ => ServiceSelector::All,
        };

        let mut issues: Option<Vec<Value>> = None;
        if learn {
            let (spec, report) = load_draft_recipe(env.as_ref()).await?;
            issues = Some(report.warnings.iter().map(|w| issue_json(w, "warning")).collect());
            env.set_recipe(spec).await?;
        }

        let mut steps: Option<Vec<Value>> = None;
        let (services, all_healthy) = match input.action {
            ServicesAction::Validate => (Vec::new(), false),
            ServicesAction::Status | ServicesAction::Health => health_json(env.as_ref(), env.health().await?),
            ServicesAction::Start => report_json(env.start_services(selector).await?),
            ServicesAction::Stop => {
                env.stop_services(selector.clone()).await?;
                (stopped_json(env.as_ref(), &selector), false)
            }
            ServicesAction::Restart => {
                env.stop_services(selector.clone()).await?;
                if input.rebuild {
                    let log = StepLog::default();
                    let scope = if learn { RebuildScope::All } else { RebuildScope::Changed(changed_paths(env.as_ref()).await?) };
                    let report = env.rebuild(scope, &log).await?;
                    steps = Some(log.to_json(&report.steps_skipped));
                }
                report_json(env.start_services(selector).await?)
            }
            ServicesAction::DbReset => {
                env.reset_db().await?;
                health_json(env.as_ref(), env.health().await?)
            }
            ServicesAction::Build => {
                let log = StepLog::default();
                let result = env.build(&log).await;
                steps = Some(log.to_json(&[]));
                result?;
                (Vec::new(), false)
            }
            ServicesAction::DbSetup => {
                env.setup_db().await?;
                (Vec::new(), false)
            }
        };
        let mut out = Map::new();
        out.insert("services".into(), Value::Array(services));
        out.insert("all_healthy".into(), Value::Bool(all_healthy));
        if let Some(issues) = issues {
            out.insert("issues".into(), Value::Array(issues));
        }
        if let Some(steps) = steps {
            out.insert("steps".into(), Value::Array(steps));
        }
        Ok(Value::Object(out))
    }

    pub(super) async fn playwright_run(&self, input: PlaywrightRunInput) -> ToolOut {
        let env = self.env()?;
        let rel = RelPath::new(&input.script)?;
        if ScriptKind::from_file_name(rel.as_str()) != Some(ScriptKind::Playwright) {
            return Err(ToolError::new("script must be *.spec.ts, *.spec.js, *.spec.mjs, *.test.ts, or *.test.js"));
        }
        if input.grep.as_ref().is_some_and(|g| g.chars().count() > PW_GREP_MAX) {
            return Err(ToolError::new(format!("grep is longer than {PW_GREP_MAX} characters")));
        }
        if !(PW_TIMEOUT_MIN_S..=PW_TIMEOUT_MAX_S).contains(&input.timeout_s) {
            return Err(ToolError::new(format!("timeout_s must be {PW_TIMEOUT_MIN_S}..={PW_TIMEOUT_MAX_S}")));
        }
        let _slot = self.playwright_slots.acquire().await.map_err(|_| ToolError::new("the run is shutting down"))?;
        let label = {
            let mut state = self.state.lock();
            state.explore_seq += 1;
            format!("explore-{}", state.explore_seq)
        };
        let timeout = Duration::from_secs(u64::from(input.timeout_s));
        let opts = RecordOpts {
            video: input.record,
            trace: input.record,
            screenshots: ScreenshotMode::OnFailure,
            har: true,
            timeout,
            test_timeout: timeout,
            outbox: self.outbox.clone(),
            label,
        };
        let script = BrowserScript { source: ScriptSource::Workspace(rel), grep: input.grep };
        let outcome = self.deps.browser.run(env.as_ref(), &script, &opts).await?;

        let mut tally = ExploreTally::default();
        let video = self.upload_explore(outcome.video.as_ref(), &mut tally).await;
        let trace = self.upload_explore(outcome.trace.as_ref(), &mut tally).await;
        let mut screenshots = Vec::new();
        for shot in &outcome.screenshots {
            if let Some(id) = self.upload_explore(Some(shot), &mut tally).await {
                screenshots.push(id);
            }
        }
        // HAR and report are not uploaded for explorations.
        for local in outcome.har.iter().chain(outcome.report.iter()) {
            super::remove_quietly(&local.path).await;
        }

        let failures: Vec<Value> = outcome
            .failures
            .iter()
            .take(PW_FAILURES_MAX)
            .map(|f| json!({ "title": clean_text(&f.title), "message": clean_text(&f.message), "location": f.location }))
            .collect();
        let statuses: Vec<u16> = outcome.http_statuses.iter().copied().take(PW_STATUSES_MAX).collect();
        let mut out = json!({
            "passed": outcome.passed,
            "status": outcome.status.as_str(),
            "error_text": outcome.error_text.as_deref().map(clean_text),
            "failures": failures,
            "http_statuses": statuses,
            "duration_ms": ms(outcome.duration),
            "artifacts": { "video": video, "trace": trace, "screenshots": screenshots },
            "stdout_tail": clean_text(&outcome.stdout_tail),
        });
        let mut notes = Vec::new();
        if tally.skipped > 0 {
            notes.push(format!(
                "{} recording(s) kept local: the run's limit of {} uploaded exploration artifacts is reached",
                tally.skipped, self.config.max_explore_uploads
            ));
        }
        notes.extend(tally.errors);
        if !notes.is_empty() {
            out["note"] = Value::String(notes.join("; "));
        }
        Ok(out)
    }

    /// Uploads one exploration recording within the run's upload budget.
    async fn upload_explore(&self, artifact: Option<&LocalArtifact>, tally: &mut ExploreTally) -> Option<String> {
        let artifact = artifact?;
        let allowed = {
            let mut state = self.state.lock();
            let allowed = state.explore_uploads < self.config.max_explore_uploads;
            if allowed {
                state.explore_uploads += 1;
            }
            allowed
        };
        if !allowed {
            tally.skipped += 1;
            return None;
        }
        match self.upload_local(artifact).await {
            Ok(id) => Some(id.to_string()),
            Err(e) => {
                tally.errors.push(self.link_error(e).0);
                None
            }
        }
    }

    pub(super) async fn http_request(&self, input: HttpRequestInput) -> ToolOut {
        let env = self.env()?;
        let ports = env.ports();
        let url = match (&input.url, &input.port) {
            (Some(raw), None) => {
                if input.path.is_some() {
                    return Err(ToolError::new("path goes with port, not url"));
                }
                let url = url::Url::parse(raw).map_err(|e| ToolError::new(format!("invalid url: {e}")))?;
                if url.scheme() != "http" {
                    return Err(ToolError::new("only http:// URLs to this environment's ports are allowed"));
                }
                if !url.username().is_empty() || url.password().is_some() {
                    return Err(ToolError::new("credentials in the URL are not allowed"));
                }
                let host_ok = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
                let port_ok = url.port().is_some_and(|p| ports.contains_port(p));
                if !host_ok || !port_ok {
                    let known: Vec<String> = ports.0.iter().map(|(n, a)| format!("{}={}", n.as_str(), a.port())).collect();
                    return Err(ToolError::new(format!(
                        "only 127.0.0.1 / localhost / [::1] with one of this environment's ports is reachable ({})",
                        known.join(", ")
                    )));
                }
                url
            }
            (None, Some(name)) => {
                let addr = ports.get(name).ok_or_else(|| ToolError::new(format!("unknown port {name:?}")))?;
                let path = input.path.as_deref().unwrap_or("/");
                if !path.starts_with('/') {
                    return Err(ToolError::new("path must start with /"));
                }
                url::Url::parse(&format!("http://127.0.0.1:{}{path}", addr.port())).map_err(|e| ToolError::new(format!("invalid path: {e}")))?
            }
            _ => return Err(ToolError::new("exactly one of url or port is required")),
        };
        if !(1..=HTTP_TIMEOUT_MAX_S).contains(&input.timeout_s) {
            return Err(ToolError::new(format!("timeout_s must be 1..={HTTP_TIMEOUT_MAX_S}")));
        }
        if input.headers.len() > HTTP_HEADERS_MAX {
            return Err(ToolError::new(format!("at most {HTTP_HEADERS_MAX} headers")));
        }
        let body = match (input.body, input.body_base64) {
            (Some(_), Some(_)) => return Err(ToolError::new("body and body_base64 are exclusive")),
            (Some(text), None) => Some(text.into_bytes()),
            (None, Some(b64)) => Some(
                base64::engine::general_purpose::STANDARD.decode(b64.trim()).map_err(|e| ToolError::new(format!("invalid body_base64: {e}")))?,
            ),
            (None, None) => None,
        };
        if body.as_ref().is_some_and(|b| b.len() > HTTP_BODY_MAX) {
            return Err(ToolError::new("body is larger than 1 MiB"));
        }
        let method = reqwest::Method::from_bytes(input.method.as_str().as_bytes()).map_err(|e| ToolError::new(e.to_string()))?;
        let mut request = self.http.request(method, url).timeout(Duration::from_secs(u64::from(input.timeout_s)));
        for (name, value) in &input.headers {
            let name = reqwest::header::HeaderName::from_bytes(name.as_bytes()).map_err(|_| ToolError::new(format!("invalid header name {name:?}")))?;
            let value = reqwest::header::HeaderValue::from_str(value).map_err(|_| ToolError::new(format!("invalid value for header {name}")))?;
            request = request.header(name, value);
        }
        if let Some(body) = body {
            request = request.body(body);
        }
        let started = Instant::now();
        let mut response = request.send().await.map_err(|e| ToolError::new(format!("request failed: {}", e.without_url())))?;
        let status = response.status().as_u16();
        let mut headers: BTreeMap<String, String> = BTreeMap::new();
        for (name, value) in response.headers() {
            let value = String::from_utf8_lossy(value.as_bytes()).into_owned();
            headers.entry(name.as_str().to_owned()).and_modify(|v| {
                v.push_str(", ");
                v.push_str(&value);
            }).or_insert(value);
        }
        let mut body: Vec<u8> = Vec::new();
        let mut body_truncated = false;
        while let Some(chunk) = response.chunk().await.map_err(|e| ToolError::new(format!("reading the body failed: {}", e.without_url())))? {
            let room = HTTP_RESPONSE_BODY_MAX - body.len();
            if chunk.len() > room {
                body.extend_from_slice(&chunk[..room]);
                body_truncated = true;
                break;
            }
            body.extend_from_slice(&chunk);
        }
        let duration_ms = ms(started.elapsed());
        let mut out = json!({ "status": status, "headers": headers, "body_truncated": body_truncated, "duration_ms": duration_ms });
        match std::str::from_utf8(&body) {
            Ok(text) => out["body"] = Value::String(clean_text(text)),
            // A cut through a multi-byte character at the cap is still text.
            Err(e) if body_truncated && e.error_len().is_none() => {
                out["body"] = Value::String(clean_bytes(&body[..e.valid_up_to()]));
            }
            Err(_) => {
                out["body"] = Value::String(String::new());
                out["body_base64"] = Value::String(base64::engine::general_purpose::STANDARD.encode(&body));
            }
        }
        Ok(out)
    }

    pub(super) async fn db_query(&self, input: DbQueryInput) -> ToolOut {
        let env = self.env()?;
        if env.db_url().is_none() {
            return Err(ToolError::new("this environment has no database"));
        }
        if input.sql.trim().is_empty() || input.sql.len() > SQL_MAX {
            return Err(ToolError::new(format!("sql must be 1..={SQL_MAX} bytes")));
        }
        if input.params.len() > DB_PARAMS_MAX {
            return Err(ToolError::new(format!("at most {DB_PARAMS_MAX} params")));
        }
        if !(1..=DB_ROWS_MAX).contains(&input.max_rows) {
            return Err(ToolError::new(format!("max_rows must be 1..={DB_ROWS_MAX}")));
        }
        if !(1..=DB_TIMEOUT_MAX_S).contains(&input.timeout_s) {
            return Err(ToolError::new(format!("timeout_s must be 1..={DB_TIMEOUT_MAX_S}")));
        }
        let params = input
            .params
            .iter()
            .enumerate()
            .map(|(i, v)| match v {
                Value::Null => Ok(DbValue::Null),
                Value::Bool(b) => Ok(DbValue::Bool(*b)),
                Value::Number(n) => Ok(n.as_i64().map_or_else(|| DbValue::Float(n.as_f64().unwrap_or(f64::NAN)), DbValue::Int)),
                Value::String(s) => Ok(DbValue::Text(s.clone())),
                _ => Err(ToolError::new(format!("params[{i}] must be null, boolean, number, or string"))),
            })
            .collect::<Result<Vec<_>, _>>()?;
        let _slot = self.db_slots.acquire().await.map_err(|_| ToolError::new("the run is shutting down"))?;
        let started = Instant::now();
        let rows = env
            .db_query(DbQuery {
                sql: input.sql,
                params,
                write: input.write,
                max_rows: input.max_rows,
                timeout: Duration::from_secs(u64::from(input.timeout_s)),
            })
            .await?;
        Ok(rows_json(&rows, ms(started.elapsed())))
    }
}

fn cell_json(value: &DbValue) -> Value {
    match value {
        DbValue::Null => Value::Null,
        DbValue::Bool(b) => Value::Bool(*b),
        DbValue::Int(i) => json!(i),
        DbValue::Float(f) => json!(f),
        DbValue::Text(t) => {
            let cleaned = clean_text(t);
            if cleaned.len() > DB_CELL_MAX_BYTES {
                Value::String(format!("{}…", truncate_bytes(&cleaned, DB_CELL_MAX_BYTES)))
            } else {
                Value::String(cleaned)
            }
        }
    }
}

/// Rows until 64 KiB of serialized rows; cells capped at 4 KiB.
fn rows_json(rows: &DbRows, duration_ms: u64) -> Value {
    let columns: Vec<Value> = rows.columns.iter().map(|c| json!({ "name": c.name, "type": c.type_name })).collect();
    let mut out_rows = Vec::with_capacity(rows.rows.len());
    let mut total = 0usize;
    let mut truncated = rows.truncated;
    for row in &rows.rows {
        let cells: Vec<Value> = row.iter().map(cell_json).collect();
        let row_json = Value::Array(cells);
        let len = super::shape::json_len(&row_json);
        if total + len > DB_RESULT_MAX_BYTES {
            truncated = true;
            break;
        }
        total += len;
        out_rows.push(row_json);
    }
    json!({ "columns": columns, "rows": out_rows, "row_count": rows.row_count, "truncated": truncated, "duration_ms": duration_ms })
}

/// Learn: re-read, parse, and validate the workspace `.mage/env.toml`. Errors fail the call.
pub(super) async fn load_draft_recipe(env: &dyn Env) -> Result<(RecipeSpec, RecipeReport), ToolError> {
    let path = EnvPath::ws(RECIPE_PATH)?;
    let read = env.read(&path, ReadRange::Whole { max_bytes: RECIPE_READ_MAX }).await.map_err(|e| match e {
        crate::ports::executor::ExecError::NotFound(_) => ToolError::new(format!("write the draft recipe to {RECIPE_PATH} first")),
        e => ToolError::from(e),
    })?;
    if read.truncated || read.total_bytes > mage_core::domain::recipe::RECIPE_MAX_BYTES as u64 {
        return Err(ToolError::new(format!("{RECIPE_PATH} is larger than 64 KiB")));
    }
    let text = String::from_utf8(read.bytes).map_err(|_| ToolError::new(format!("{RECIPE_PATH} is not UTF-8")))?;
    let spec = RecipeSpec::parse(&text).map_err(|e| ToolError::new(format!("{RECIPE_PATH}: {e}")))?;
    let report = spec.validate();
    if !report.errors.is_empty() {
        return Err(ToolError::new(format!("{RECIPE_PATH} has errors:\n{}", render_issues(&report.errors))));
    }
    Ok((spec, report))
}

/// Paths the agent changed against the prepared baseline (`restart` with `rebuild: true`).
async fn changed_paths(env: &dyn Env) -> Result<Vec<RelPath>, ToolError> {
    let out = env.diff_against_base(&DiffOptions { against: Baseline::Prepared, exclude: Vec::new() }).await?;
    Ok(out.files.into_iter().map(|f| f.path).collect())
}

fn recipe_ports(env: &dyn Env, service: &str) -> Map<String, Value> {
    let ports = env.ports();
    let mut out = Map::new();
    if let Some(spec) = env.recipe().and_then(|r| r.services.get(service).cloned()) {
        for name in &spec.ports {
            if let Some(addr) = ports.get(name) {
                out.insert(name.clone(), json!(addr.port()));
            }
        }
    }
    out
}

fn report_json(report: ServicesReport) -> (Vec<Value>, bool) {
    let services = report
        .services
        .iter()
        .map(|s| {
            let (state, reason) = state_json(&s.state);
            let ports: Map<String, Value> = s.ports.iter().map(|(n, a)| (n.as_str().to_owned(), json!(a.port()))).collect();
            let detail = reason.or_else(|| s.last_check.as_ref().map(|c| c.detail.clone())).map(|d| clean_text(&d));
            json!({ "name": s.name, "state": state, "ports": ports, "detail": detail })
        })
        .collect();
    (services, report.all_healthy)
}

fn health_json(env: &dyn Env, report: HealthReport) -> (Vec<Value>, bool) {
    let services = report
        .checks
        .iter()
        .map(|c| {
            json!({
                "name": c.service,
                "state": if c.ok { "healthy" } else { "unhealthy" },
                "ports": recipe_ports(env, &c.service),
                "detail": clean_text(&c.detail),
            })
        })
        .collect();
    (services, report.all_ok)
}

fn stopped_json(env: &dyn Env, selector: &ServiceSelector) -> Vec<Value> {
    let names: Vec<String> = match selector {
        ServiceSelector::Named(names) => names.clone(),
        ServiceSelector::All => env.recipe().map(|r| r.services.keys().cloned().collect()).unwrap_or_default(),
    };
    names.iter().map(|n| json!({ "name": n, "state": "stopped", "ports": {}, "detail": null })).collect()
}

