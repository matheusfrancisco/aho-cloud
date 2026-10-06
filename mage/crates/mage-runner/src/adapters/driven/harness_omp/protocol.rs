//! omp RPC stdout handling: frame decoding, response correlation, and the event mapping of
//! harness.md "Event mapping".

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::{Map, Value, json};
use tokio::sync::oneshot;

use super::frame::FrameDecoder;
use crate::adapters::driven::harness_common::session::{Protocol, SessionCore, TranscriptSource};
use crate::ports::executor::{EnvPath, RelPath};
use crate::ports::harness::{HarnessEvent, McpServerName, McpServerSpec, NoticeLevel, ToolRef, TurnOutcome, UsageDelta};

pub type Frame = Map<String, Value>;

/// Maps omp tool names (`mcp__<server>_<tool>` for MCP) back to `ToolRef`.
#[derive(Debug, Clone, Default)]
pub struct ToolNames {
    /// Minted name → expected tool.
    exact: HashMap<String, ToolRef>,
    /// Declared servers, longest first (fallback split).
    servers: Vec<McpServerName>,
}

impl ToolNames {
    pub fn new(servers: &[McpServerSpec]) -> Self {
        let mut exact = HashMap::new();
        for s in servers {
            for t in &s.expected_tools {
                exact.insert(mint_mcp_tool_name(s.name.as_str(), t), ToolRef::Mcp { server: s.name.clone(), tool: t.clone() });
            }
        }
        let mut servers: Vec<McpServerName> = servers.iter().map(|s| s.name.clone()).collect();
        servers.sort_by_key(|s| std::cmp::Reverse(s.as_str().len()));
        Self { exact, servers }
    }

    /// The expected MCP tool for an omp tool name, if it is one.
    pub fn expected(&self, name: &str) -> Option<&ToolRef> {
        self.exact.get(name).or_else(|| {
            // omp caps names at 64 chars with a hash suffix: match the kept prefix.
            (name.len() == 64).then(|| self.exact.iter().find(|(k, _)| k.len() > 64 && k[..55] == name[..55]).map(|(_, v)| v)).flatten()
        })
    }

    pub fn expected_names(&self) -> impl Iterator<Item = (&String, &ToolRef)> {
        self.exact.iter()
    }

    pub fn normalize(&self, name: &str) -> ToolRef {
        if let Some(t) = self.expected(name) {
            return t.clone();
        }
        if let Some(rest) = name.strip_prefix("mcp__") {
            for server in &self.servers {
                if let Some(tool) = rest.strip_prefix(server.as_str()).and_then(|r| r.strip_prefix('_')) {
                    return ToolRef::Mcp { server: server.clone(), tool: tool.to_owned() };
                }
            }
        }
        ToolRef::Builtin(name.to_owned())
    }
}

/// omp's `createMCPToolName` (18.3.1): sanitize both parts (`[^a-z0-9_]+` → `_`, collapse, trim),
/// strip a redundant `<server>_` tool prefix, join as `mcp__<server>_<tool>`.
pub fn mint_mcp_tool_name(server: &str, tool: &str) -> String {
    fn sanitize(v: &str, fallback: &str) -> String {
        let mut out = String::with_capacity(v.len());
        for c in v.to_lowercase().chars() {
            let c = if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' { c } else { '_' };
            if c == '_' && out.ends_with('_') {
                continue;
            }
            out.push(c);
        }
        let trimmed = out.trim_matches('_');
        if trimmed.is_empty() { fallback.to_owned() } else { trimmed.to_owned() }
    }
    let server = sanitize(server, "server");
    let tool = sanitize(tool, "tool");
    let tool = tool.strip_prefix(&format!("{server}_")).map(str::to_owned).unwrap_or(tool);
    format!("mcp__{server}_{tool}")
}

#[derive(Default)]
struct State {
    decoder: FrameDecoder,
    ready: Option<oneshot::Sender<Frame>>,
    pending: HashMap<String, oneshot::Sender<Frame>>,
    /// Prompt ids sent by `steer` (their responses are not awaited).
    prompts: HashSet<String>,
    tools: ToolNames,
    session_file: Option<String>,
    unknown: u64,
}

pub struct OmpProtocol {
    state: Mutex<State>,
    /// `<scratch_root>/` prefix used to turn `sessionFile` into an `EnvPath`.
    scratch_root: String,
}

impl OmpProtocol {
    pub fn new(scratch_root: &str, tools: ToolNames) -> (Self, oneshot::Receiver<Frame>) {
        let (tx, rx) = oneshot::channel();
        let state = State { ready: Some(tx), tools, ..State::default() };
        (Self { state: Mutex::new(state), scratch_root: format!("{}/", scratch_root.trim_end_matches('/')) }, rx)
    }

    /// Registers a waiter for the response to command `id`.
    pub fn expect(&self, id: &str) -> oneshot::Receiver<Frame> {
        let (tx, rx) = oneshot::channel();
        self.state.lock().pending.insert(id.to_owned(), tx);
        rx
    }

    pub fn set_max_reassembled(&self, max: usize) {
        self.state.lock().decoder.set_max_reassembled(max);
    }

    pub fn set_session_file(&self, path: Option<String>) {
        self.state.lock().session_file = path;
    }

    /// Maps one decoded frame.
    fn dispatch(&self, core: &SessionCore, frame: Frame) {
        let ty = frame.get("type").and_then(Value::as_str).unwrap_or_default().to_owned();
        match ty.as_str() {
            "ready" => {
                if let Some(tx) = self.state.lock().ready.take() {
                    let _ = tx.send(frame);
                }
            }
            "response" => self.on_response(core, frame),
            "agent_start" => {
                core.open_turn();
            }
            "message_update" => {
                let event = frame.get("assistantMessageEvent");
                let delta = match event {
                    Some(e) if e.get("type").and_then(Value::as_str) == Some("text_delta") => e.get("delta").and_then(Value::as_str),
                    _ => None,
                };
                if let Some(delta) = delta.filter(|d| !d.is_empty()) {
                    let turn = core.open_turn();
                    core.emit(HarnessEvent::AssistantText { turn, text: delta.to_owned() });
                }
            }
            "message_end" => {
                let Some(msg) = frame.get("message") else { return };
                if msg.get("role").and_then(Value::as_str) != Some("assistant") {
                    return;
                }
                if let Some(u) = msg.get("usage") {
                    let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
                    let cost = u.get("cost").and_then(|c| c.get("total")).and_then(Value::as_f64).map(usd_to_micros);
                    core.add_usage(UsageDelta {
                        input_tokens: n("input"),
                        output_tokens: n("output"),
                        cache_read_tokens: n("cacheRead"),
                        cache_write_tokens: n("cacheWrite"),
                        cost_micros: cost,
                        model_calls: 1,
                    });
                }
                if msg.get("stopReason").and_then(Value::as_str) == Some("error") {
                    let text = msg.get("errorMessage").and_then(Value::as_str).unwrap_or("model request failed");
                    core.emit(HarnessEvent::Notice { level: NoticeLevel::Error, text: text.to_owned() });
                }
            }
            "tool_execution_start" => {
                let (turn, _) = core.turn();
                let call_id = str_field(&frame, "toolCallId");
                let tool = self.state.lock().tools.normalize(&str_field(&frame, "toolName"));
                let args_preview = frame.get("args").map(Value::to_string).unwrap_or_default();
                core.emit(HarnessEvent::ToolCall { turn, call_id, tool, args_preview });
            }
            "tool_execution_end" => {
                let (turn, _) = core.turn();
                let call_id = str_field(&frame, "toolCallId");
                let tool = self.state.lock().tools.normalize(&str_field(&frame, "toolName"));
                let ok = !frame.get("isError").and_then(Value::as_bool).unwrap_or(false);
                let preview = frame.get("result").map(content_text).unwrap_or_default();
                core.emit(HarnessEvent::ToolResult { turn, call_id, tool, ok, preview });
            }
            "agent_end" => {}
            "prompt_result" => {
                let status = frame.get("status").and_then(Value::as_str).unwrap_or("completed");
                let outcome = match status {
                    "completed" => TurnOutcome::Completed,
                    "aborted" => TurnOutcome::Aborted,
                    _ => {
                        let err = frame.get("error");
                        TurnOutcome::Failed {
                            message: err.and_then(|e| e.get("message")).and_then(Value::as_str).unwrap_or("prompt failed").to_owned(),
                            retryable: err.and_then(|e| e.get("retryable")).and_then(Value::as_bool).unwrap_or(false),
                            http_status: err.and_then(|e| e.get("httpStatus")).and_then(Value::as_u64).and_then(|s| u16::try_from(s).ok()),
                        }
                    }
                };
                // Folded steers report at the same yield: only the first closes the turn.
                if !core.end_turn(outcome.clone()) {
                    match outcome {
                        TurnOutcome::Completed => {}
                        TurnOutcome::Aborted => core.emit(HarnessEvent::Notice { level: NoticeLevel::Warning, text: "queued message aborted".into() }),
                        TurnOutcome::Failed { message, .. } => core.emit(HarnessEvent::Notice { level: NoticeLevel::Error, text: message }),
                    }
                }
                if frame.get("sessionSettled").and_then(Value::as_bool) == Some(true) {
                    core.idle();
                }
            }
            "session_settled" => core.idle(),
            "auto_retry_start" => core.emit(HarnessEvent::Notice {
                level: NoticeLevel::Warning,
                text: format!(
                    "model request retry {}/{} in {} ms: {}",
                    num(&frame, "attempt"),
                    num(&frame, "maxAttempts"),
                    num(&frame, "delayMs"),
                    str_field(&frame, "errorMessage")
                ),
            }),
            "auto_retry_end" => {
                let success = frame.get("success").and_then(Value::as_bool).unwrap_or(false);
                let text = if success {
                    format!("model request succeeded after {} attempts", num(&frame, "attempt"))
                } else {
                    format!("model request retries exhausted: {}", str_field(&frame, "finalError"))
                };
                core.emit(HarnessEvent::Notice { level: NoticeLevel::Warning, text });
            }
            "retry_fallback_applied" => core.emit(HarnessEvent::Notice {
                level: NoticeLevel::Warning,
                text: format!("model fallback {} → {}", str_field(&frame, "from"), str_field(&frame, "to")),
            }),
            "auto_compaction_start" => core.emit(HarnessEvent::Notice {
                level: NoticeLevel::Info,
                text: format!("context compaction started ({})", str_field(&frame, "reason")),
            }),
            "auto_compaction_end" => {
                let text = match frame.get("errorMessage").and_then(Value::as_str) {
                    Some(e) => format!("context compaction failed: {e}"),
                    None if frame.get("aborted").and_then(Value::as_bool) == Some(true) => "context compaction aborted".to_owned(),
                    None => "context compaction finished".to_owned(),
                };
                core.emit(HarnessEvent::Notice { level: NoticeLevel::Info, text });
            }
            "notice" => {
                let level = match frame.get("level").and_then(Value::as_str) {
                    Some("error") => NoticeLevel::Error,
                    Some("warning") => NoticeLevel::Warning,
                    _ => NoticeLevel::Info,
                };
                core.emit(HarnessEvent::Notice { level, text: str_field(&frame, "message") });
            }
            "extension_error" => {
                tracing::warn!(run_id = %core.run_id, "omp extension_error under --no-extensions (isolation anomaly)");
                core.emit(HarnessEvent::Notice {
                    level: NoticeLevel::Error,
                    text: format!(
                        "extension error ({}, {}): {}",
                        str_field(&frame, "extensionPath"),
                        str_field(&frame, "event"),
                        str_field(&frame, "error")
                    ),
                });
            }
            "extension_ui_request" => {
                let id = str_field(&frame, "id");
                core.write_detached(json!({ "type": "extension_ui_response", "id": id, "cancelled": true }).to_string().into_bytes());
                core.emit(HarnessEvent::Notice {
                    level: NoticeLevel::Warning,
                    text: format!("harness UI request {} cancelled (no UI)", str_field(&frame, "method")),
                });
            }
            "host_tool_call" => {
                let id = str_field(&frame, "id");
                let reply = json!({
                    "type": "host_tool_result", "id": id, "isError": true,
                    "result": { "content": [{ "type": "text", "text": "no host tools are registered" }] },
                });
                core.write_detached(reply.to_string().into_bytes());
                core.emit(HarnessEvent::Notice {
                    level: NoticeLevel::Error,
                    text: format!("unexpected host tool call {}", str_field(&frame, "toolName")),
                });
            }
            "host_uri_request" => {
                let id = str_field(&frame, "id");
                let reply = json!({ "type": "host_uri_result", "id": id, "isError": true, "error": "no host URI schemes are registered" });
                core.write_detached(reply.to_string().into_bytes());
                core.emit(HarnessEvent::Notice {
                    level: NoticeLevel::Error,
                    text: format!("unexpected host URI request {}", str_field(&frame, "url")),
                });
            }
            _ => {
                let mut st = self.state.lock();
                st.unknown += 1;
                tracing::debug!(frame_type = %ty, unknown = st.unknown, "ignoring omp frame");
            }
        }
    }

    fn on_response(&self, core: &SessionCore, frame: Frame) {
        let id = str_field(&frame, "id");
        let waiter = self.state.lock().pending.remove(&id);
        if let Some(tx) = waiter {
            let _ = tx.send(frame);
            return;
        }
        let is_prompt = self.state.lock().prompts.remove(&id);
        if !is_prompt || frame.get("success").and_then(Value::as_bool) != Some(false) {
            return;
        }
        let message = format!("prompt {id} rejected: {}", str_field(&frame, "error"));
        if !core.end_turn(TurnOutcome::Failed { message: message.clone(), retryable: false, http_status: None }) {
            core.emit(HarnessEvent::Notice { level: NoticeLevel::Error, text: message });
        } else {
            core.idle();
        }
    }
}

#[async_trait]
impl Protocol for OmpProtocol {
    async fn on_line(&self, core: &SessionCore, line: Vec<u8>) -> Result<(), String> {
        let frame = self.state.lock().decoder.push(&line)?;
        if let Some(frame) = frame {
            self.dispatch(core, frame);
        }
        Ok(())
    }

    fn on_eof(&self) {
        let mut st = self.state.lock();
        if let Err(e) = st.decoder.finish() {
            tracing::debug!(error = %e, "omp stdout ended mid-chunk");
        }
        st.ready = None;
        st.pending.clear();
    }

    fn steer_line(&self, _core: &SessionCore, id: &str, text: &str) -> Vec<u8> {
        self.state.lock().prompts.insert(id.to_owned());
        prompt_command(id, text)
    }

    fn abort_line(&self, id: &str) -> Vec<u8> {
        json!({ "id": id, "type": "abort" }).to_string().into_bytes()
    }

    async fn transcript(&self, _core: &SessionCore) -> Option<TranscriptSource> {
        let file = self.state.lock().session_file.clone()?;
        let rel = file.strip_prefix(&self.scratch_root)?;
        RelPath::new(rel).ok().map(|rel| TranscriptSource::Env(EnvPath::new(crate::ports::executor::Area::Scratch, rel)))
    }
}

/// `prompt` with `streamingBehavior: "steer"`: steers mid-turn, starts a turn when idle.
pub fn prompt_command(id: &str, text: &str) -> Vec<u8> {
    json!({ "id": id, "type": "prompt", "message": text, "streamingBehavior": "steer" }).to_string().into_bytes()
}

fn usd_to_micros(usd: f64) -> u64 {
    if usd.is_finite() && usd > 0.0 { (usd * 1_000_000.0).round() as u64 } else { 0 }
}

fn str_field(frame: &Frame, key: &str) -> String {
    match frame.get(key) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

fn num(frame: &Frame, key: &str) -> String {
    frame.get(key).map(Value::to_string).unwrap_or_else(|| "?".into())
}

/// Text blocks of a tool result (`{content: [{type: "text", text}]}`), joined by newlines.
fn content_text(result: &Value) -> String {
    match result.get("content") {
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|b| b.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|b| b.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}
