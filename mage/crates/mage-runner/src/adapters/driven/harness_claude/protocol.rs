//! Claude Code stream-json stdout handling (harness.md "Output message mapping", "Inventory
//! check", turn rules).

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::sync::oneshot;

use super::files::{self, ACCEPTED_PERMISSION_MODES, HELPER_KEY_SOURCE, INTERNAL_TOOLS};
use crate::adapters::driven::harness_common::session::{Protocol, SessionCore, TranscriptSource};
use crate::ports::executor::{Area, EnvPath, ListOpts, RelPath};
use mage_core::domain::HarnessKind;

use crate::ports::harness::{
    BuiltinToolPolicy, ExitReason, HarnessEvent, McpServerName, McpServerSpec, NoticeLevel, SessionInfo, ToolRef, TurnOutcome, UsageDelta,
};

/// Quiet window after `result` before `Idle` (the session-state message is undocumented).
pub const IDLE_AFTER_RESULT: Duration = Duration::from_millis(1500);
const RETRYABLE_STATUS: &[u64] = &[429, 500, 502, 503, 529];

/// Why the first `init` did not produce a session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartFailure {
    Violation(String),
    Unavailable(String),
}

/// What the `init` message must show (H-ISO-2).
#[derive(Debug, Clone)]
pub struct Expectations {
    pub servers: Vec<McpServerName>,
    /// `mcp__<server>__<tool>` → tool.
    pub mcp_tools: HashMap<String, ToolRef>,
    pub builtins: BTreeSet<&'static str>,
    pub cwd: String,
    pub model: String,
    pub gateway: bool,
    pub version: String,
}

impl Expectations {
    pub fn new(servers: &[McpServerSpec], policy: BuiltinToolPolicy, cwd: String, model: String, gateway: bool, version: String) -> Self {
        let mut mcp_tools = HashMap::new();
        for s in servers {
            for t in &s.expected_tools {
                mcp_tools.insert(format!("mcp__{}__{t}", s.name), ToolRef::Mcp { server: s.name.clone(), tool: t.clone() });
            }
        }
        let mut names: Vec<McpServerName> = servers.iter().map(|s| s.name.clone()).collect();
        names.sort_by_key(|s| std::cmp::Reverse(s.as_str().len()));
        let builtins = files::policy_tools(policy).iter().chain(INTERNAL_TOOLS).copied().collect();
        Self { servers: names, mcp_tools, builtins, cwd, model, gateway, version }
    }

    pub fn normalize(&self, name: &str) -> ToolRef {
        if let Some(t) = self.mcp_tools.get(name) {
            return t.clone();
        }
        if let Some(rest) = name.strip_prefix("mcp__") {
            for server in &self.servers {
                if let Some(tool) = rest.strip_prefix(server.as_str()).and_then(|r| r.strip_prefix("__")) {
                    return ToolRef::Mcp { server: server.clone(), tool: tool.to_owned() };
                }
            }
        }
        ToolRef::Builtin(name.to_owned())
    }

    /// The inventory check on a `system/init` message. `Ok` = the session's tool list.
    pub fn check(&self, init: &Value) -> Result<Vec<ToolRef>, StartFailure> {
        let declared: BTreeSet<&str> = self.servers.iter().map(McpServerName::as_str).collect();
        let mut connected = BTreeSet::new();
        for s in init.get("mcp_servers").and_then(Value::as_array).into_iter().flatten() {
            let name = s.get("name").and_then(Value::as_str).unwrap_or_default();
            if !declared.contains(name) {
                return Err(StartFailure::Violation(format!("unexpected MCP server {name}")));
            }
            let status = s.get("status").and_then(Value::as_str).unwrap_or_default();
            if status != "connected" {
                return Err(StartFailure::Unavailable(format!("MCP server {name} is {status}")));
            }
            connected.insert(name);
        }
        if let Some(missing) = declared.iter().find(|n| !connected.contains(*n)) {
            return Err(StartFailure::Unavailable(format!("MCP server {missing} not connected")));
        }
        let mut seen = HashSet::new();
        let mut tools = Vec::new();
        for name in init.get("tools").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
            if name.starts_with("mcp__") {
                match self.mcp_tools.get(name) {
                    Some(t) => {
                        seen.insert(name);
                        tools.push(t.clone());
                    }
                    None => return Err(StartFailure::Violation(format!("unexpected MCP tool {name}"))),
                }
            } else if self.builtins.contains(name) {
                tools.push(ToolRef::Builtin(name.to_owned()));
            } else {
                return Err(StartFailure::Violation(format!("unexpected built-in tool {name}")));
            }
        }
        let mut missing: Vec<&str> = self.mcp_tools.keys().map(String::as_str).filter(|n| !seen.contains(n)).collect();
        if !missing.is_empty() {
            missing.sort_unstable();
            return Err(StartFailure::Unavailable(format!("MCP tools not available: {}", missing.join(", "))));
        }
        for key in ["plugins", "skills", "agents"] {
            if init.get(key).and_then(Value::as_array).is_some_and(|a| !a.is_empty()) {
                return Err(StartFailure::Violation(format!("unexpected {key}: {}", init[key])));
            }
        }
        let cwd = init.get("cwd").and_then(Value::as_str).unwrap_or_default();
        if cwd.trim_end_matches('/') != self.cwd.trim_end_matches('/') {
            return Err(StartFailure::Violation(format!("cwd {cwd}, expected {}", self.cwd)));
        }
        let mode = init.get("permissionMode").and_then(Value::as_str).unwrap_or_default();
        if !ACCEPTED_PERMISSION_MODES.contains(&mode) {
            return Err(StartFailure::Violation(format!("permission mode {mode}")));
        }
        let model = init.get("model").and_then(Value::as_str).unwrap_or_default();
        if model != self.model {
            return Err(StartFailure::Violation(format!("model {model}, expected {}", self.model)));
        }
        if self.gateway {
            let source = init.get("apiKeySource").and_then(Value::as_str).unwrap_or_default();
            if source != HELPER_KEY_SOURCE {
                return Err(StartFailure::Violation(format!("apiKeySource {source}, expected {HELPER_KEY_SOURCE}")));
            }
        }
        Ok(tools)
    }
}

/// Where the transcript is looked up after exit.
#[derive(Debug, Clone)]
pub enum TranscriptRoot {
    /// `<session_dir>/claude/config/projects` in the scratch area.
    Scratch(RelPath),
    /// LocalLogin: `<dev_config_dir>/projects/<project dir>`.
    Host(PathBuf),
}

struct State {
    init_tx: Option<oneshot::Sender<Result<SessionInfo, StartFailure>>>,
    ready: bool,
    blocked: bool,
    calls: HashMap<String, ToolRef>,
    messages: HashSet<String>,
    last_cost_usd: f64,
    cancel_queued: bool,
    unknown: u64,
}

pub struct ClaudeProtocol {
    expect: Expectations,
    session_id: String,
    transcript: TranscriptRoot,
    state: Mutex<State>,
}

impl ClaudeProtocol {
    pub fn new(expect: Expectations, session_id: String, transcript: TranscriptRoot) -> (Self, oneshot::Receiver<Result<SessionInfo, StartFailure>>) {
        let (tx, rx) = oneshot::channel();
        let state = State {
            init_tx: Some(tx),
            ready: false,
            blocked: false,
            calls: HashMap::new(),
            messages: HashSet::new(),
            last_cost_usd: 0.0,
            cancel_queued: false,
            unknown: 0,
        };
        (Self { expect, session_id, transcript, state: Mutex::new(state) }, rx)
    }

    /// Maps one stream-json message.
    fn dispatch(&self, core: &SessionCore, msg: Value) {
        if self.state.lock().blocked {
            return;
        }
        let ty = msg.get("type").and_then(Value::as_str).unwrap_or_default().to_owned();
        let subtype = msg.get("subtype").and_then(Value::as_str).unwrap_or_default().to_owned();
        match (ty.as_str(), subtype.as_str()) {
            ("system", "init") => self.on_init(core, &msg),
            ("system", "compact_boundary") => core.emit(HarnessEvent::Notice { level: NoticeLevel::Info, text: "context compacted".into() }),
            ("system", "api_retry") => core.emit(HarnessEvent::Notice {
                level: NoticeLevel::Warning,
                text: format!(
                    "model request retry {}/{} in {} ms: {} {}",
                    field(&msg, "attempt"),
                    field(&msg, "max_retries"),
                    field(&msg, "retry_delay_ms"),
                    field(&msg, "error_status"),
                    field(&msg, "error")
                ),
            }),
            ("system", "status") => {}
            ("system", s) if s.contains("rate_limit") || s.contains("permission") => {
                core.emit(HarnessEvent::Notice { level: NoticeLevel::Warning, text: format!("{s}: {}", compact(&msg)) });
            }
            ("stream_event", _) => {
                if !self.state.lock().ready {
                    return;
                }
                let turn = core.open_turn();
                let event = msg.get("event");
                let delta = event.filter(|e| e.get("type").and_then(Value::as_str) == Some("content_block_delta")).and_then(|e| e.get("delta"));
                let text = delta.filter(|d| d.get("type").and_then(Value::as_str) == Some("text_delta")).and_then(|d| d.get("text")).and_then(Value::as_str);
                if let Some(text) = text.filter(|t| !t.is_empty()) {
                    core.emit(HarnessEvent::AssistantText { turn, text: text.to_owned() });
                }
            }
            ("assistant", _) => self.on_assistant(core, &msg),
            ("user", _) => self.on_user(core, &msg),
            ("result", _) => self.on_result(core, &msg),
            ("control_request", _) => {
                let id = field(&msg, "request_id");
                let req = msg.get("request").and_then(|r| r.get("subtype")).and_then(Value::as_str).unwrap_or_default().to_owned();
                let reply = if req == "can_use_tool" {
                    json!({ "type": "control_response", "response": { "subtype": "success", "request_id": id, "response": { "behavior": "deny", "message": "denied by Mage" } } })
                } else {
                    json!({ "type": "control_response", "response": { "subtype": "error", "request_id": id, "error": "not supported by Mage" } })
                };
                core.write_detached(reply.to_string().into_bytes());
                core.emit(HarnessEvent::Notice { level: NoticeLevel::Warning, text: format!("harness control request {req} denied") });
            }
            ("control_response", _) => tracing::debug!(response = %compact(&msg), "claude control_response"),
            (t, _) if t.contains("rate_limit") => {
                core.emit(HarnessEvent::Notice { level: NoticeLevel::Warning, text: format!("rate limit: {}", compact(&msg)) });
            }
            _ => {
                let mut st = self.state.lock();
                st.unknown += 1;
                tracing::debug!(message_type = %ty, subtype = %subtype, unknown = st.unknown, "ignoring claude message");
            }
        }
    }

    fn on_init(&self, core: &SessionCore, init: &Value) {
        let check = self.expect.check(init);
        let mut st = self.state.lock();
        st.cancel_queued = init
            .get("capabilities")
            .and_then(Value::as_array)
            .is_some_and(|c| c.iter().any(|v| v.as_str() == Some("interrupt_cancel_queued_v1")));
        if st.ready {
            // Every new turn re-announces init: re-check, fail closed on anything unexpected.
            match check {
                Err(StartFailure::Violation(detail)) => {
                    st.blocked = true;
                    drop(st);
                    core.kill(ExitReason::IsolationViolation(detail), Duration::ZERO);
                }
                Err(StartFailure::Unavailable(detail)) => {
                    drop(st);
                    core.emit(HarnessEvent::Notice { level: NoticeLevel::Warning, text: detail });
                }
                Ok(_) => {}
            }
            return;
        }
        let tx = st.init_tx.take();
        match check {
            Ok(tools) => {
                st.ready = true;
                drop(st);
                let info = SessionInfo {
                    harness: HarnessKind::ClaudeCode,
                    version: self.expect.version.clone(),
                    native_session_id: init.get("session_id").and_then(Value::as_str).unwrap_or(&self.session_id).to_owned(),
                    model: init.get("model").and_then(Value::as_str).unwrap_or_default().to_owned(),
                    tools,
                    pid: core.pid,
                    pgid: core.pid,
                };
                core.ready(info.clone());
                core.open_turn();
                if let Some(tx) = tx {
                    let _ = tx.send(Ok(info));
                }
            }
            Err(failure) => {
                st.blocked = true;
                drop(st);
                let reason = match &failure {
                    StartFailure::Violation(d) => ExitReason::IsolationViolation(d.clone()),
                    StartFailure::Unavailable(_) => ExitReason::Crashed,
                };
                core.kill(reason, Duration::ZERO);
                if let Some(tx) = tx {
                    let _ = tx.send(Err(failure));
                }
            }
        }
    }

    fn on_assistant(&self, core: &SessionCore, msg: &Value) {
        if !self.state.lock().ready {
            return;
        }
        let turn = core.open_turn();
        let message = msg.get("message");
        let id = message.and_then(|m| m.get("id")).and_then(Value::as_str).unwrap_or_default().to_owned();
        let new_message = !id.is_empty() && self.state.lock().messages.insert(id);
        if let Some(u) = message.and_then(|m| m.get("usage")).filter(|_| new_message) {
            let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
            core.add_usage(UsageDelta {
                input_tokens: n("input_tokens"),
                output_tokens: n("output_tokens"),
                cache_read_tokens: n("cache_read_input_tokens"),
                cache_write_tokens: n("cache_creation_input_tokens"),
                cost_micros: None,
                model_calls: 1,
            });
        }
        for block in message.and_then(|m| m.get("content")).and_then(Value::as_array).into_iter().flatten() {
            if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            let call_id = block.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
            let tool = self.expect.normalize(block.get("name").and_then(Value::as_str).unwrap_or_default());
            self.state.lock().calls.insert(call_id.clone(), tool.clone());
            let args_preview = block.get("input").map(Value::to_string).unwrap_or_default();
            core.emit(HarnessEvent::ToolCall { turn, call_id, tool, args_preview });
        }
        if let Some(err) = msg.get("error").filter(|e| !e.is_null()) {
            let text = err.as_str().map(str::to_owned).unwrap_or_else(|| err.to_string());
            core.emit(HarnessEvent::Notice { level: NoticeLevel::Error, text });
        }
    }

    fn on_user(&self, core: &SessionCore, msg: &Value) {
        if !self.state.lock().ready {
            return;
        }
        let content = msg.get("message").and_then(|m| m.get("content")).and_then(Value::as_array);
        let results: Vec<&Value> =
            content.into_iter().flatten().filter(|b| b.get("type").and_then(Value::as_str) == Some("tool_result")).collect();
        if results.is_empty() {
            return;
        }
        let turn = core.open_turn();
        for block in results {
            let call_id = block.get("tool_use_id").and_then(Value::as_str).unwrap_or_default().to_owned();
            let tool = self.state.lock().calls.remove(&call_id).unwrap_or_else(|| ToolRef::Builtin("unknown".into()));
            let ok = !block.get("is_error").and_then(Value::as_bool).unwrap_or(false);
            let preview = match block.get("content") {
                Some(Value::String(s)) => s.clone(),
                Some(Value::Array(parts)) => parts
                    .iter()
                    .filter(|p| p.get("type").and_then(Value::as_str) == Some("text"))
                    .filter_map(|p| p.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n"),
                _ => String::new(),
            };
            core.emit(HarnessEvent::ToolResult { turn, call_id, tool, ok, preview });
        }
    }

    fn on_result(&self, core: &SessionCore, msg: &Value) {
        let subtype = msg.get("subtype").and_then(Value::as_str).unwrap_or_default();
        let errors: Vec<String> = msg.get("errors").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_owned).collect();
        {
            let mut st = self.state.lock();
            if !st.ready {
                // Startup failure: a result before init.
                st.blocked = true;
                let reason = msg
                    .get("startup_failure_reason")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .or_else(|| (!errors.is_empty()).then(|| errors.join("; ")))
                    .unwrap_or_else(|| format!("claude ended before init ({subtype})"));
                if let Some(tx) = st.init_tx.take() {
                    let _ = tx.send(Err(StartFailure::Unavailable(reason)));
                }
                return;
            }
        }
        if let Some(total) = msg.get("total_cost_usd").and_then(Value::as_f64) {
            let delta = {
                let mut st = self.state.lock();
                let delta = (total - st.last_cost_usd).max(0.0);
                st.last_cost_usd = st.last_cost_usd.max(total);
                delta
            };
            let micros = (delta * 1_000_000.0).round() as u64;
            if micros > 0 {
                core.add_usage(UsageDelta { cost_micros: Some(micros), ..UsageDelta::default() });
            }
        }
        let is_error = msg.get("is_error").and_then(Value::as_bool).unwrap_or(false);
        let status = msg.get("api_error_status").and_then(Value::as_u64);
        let outcome = if core.aborting() {
            TurnOutcome::Aborted
        } else if subtype == "success" && !is_error {
            TurnOutcome::Completed
        } else if subtype == "error_max_budget_usd" {
            TurnOutcome::Failed { message: "budget".into(), retryable: false, http_status: None }
        } else {
            let message = if errors.is_empty() {
                msg.get("result").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| subtype.to_owned())
            } else {
                errors.join("; ")
            };
            TurnOutcome::Failed {
                message,
                retryable: status.is_some_and(|s| RETRYABLE_STATUS.contains(&s)),
                http_status: status.and_then(|s| u16::try_from(s).ok()),
            }
        };
        if !core.end_turn(outcome.clone()) {
            if let TurnOutcome::Failed { message, .. } = outcome {
                core.emit(HarnessEvent::Notice { level: NoticeLevel::Error, text: message });
            }
        }
        core.idle_after(IDLE_AFTER_RESULT);
    }
}

#[async_trait]
impl Protocol for ClaudeProtocol {
    async fn on_line(&self, core: &SessionCore, line: Vec<u8>) -> Result<(), String> {
        let msg: Value = serde_json::from_slice(&line).map_err(|e| format!("unparseable stream-json line: {e}"))?;
        if !msg.is_object() {
            return Err("stream-json line is not an object".into());
        }
        self.dispatch(core, msg);
        Ok(())
    }

    fn on_eof(&self) {
        self.state.lock().init_tx = None;
    }

    fn steer_line(&self, _core: &SessionCore, _id: &str, text: &str) -> Vec<u8> {
        files::user_message(text)
    }

    fn abort_line(&self, id: &str) -> Vec<u8> {
        files::interrupt_request(id, self.state.lock().cancel_queued)
    }

    fn interrupt_on_abort_timeout(&self) -> bool {
        true
    }

    async fn transcript(&self, core: &SessionCore) -> Option<TranscriptSource> {
        let file = format!("{}.jsonl", self.session_id);
        match &self.transcript {
            TranscriptRoot::Host(dir) => Some(TranscriptSource::Host(dir.join(file))),
            TranscriptRoot::Scratch(projects) => {
                let opts = ListOpts { depth: 2, glob: None, hidden: true, respect_gitignore: false, max_entries: 10_000 };
                let listing = core.env.list(&EnvPath::new(Area::Scratch, projects.clone()), opts).await.ok()?;
                listing
                    .entries
                    .into_iter()
                    .find(|e| e.path.file_name() == file)
                    .map(|e| TranscriptSource::Env(EnvPath::new(Area::Scratch, e.path)))
            }
        }
    }
}

fn field(msg: &Value, key: &str) -> String {
    match msg.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

fn compact(msg: &Value) -> String {
    let mut m = msg.clone();
    if let Some(o) = m.as_object_mut() {
        for k in ["type", "subtype", "session_id", "uuid"] {
            o.remove(k);
        }
    }
    m.to_string()
}
