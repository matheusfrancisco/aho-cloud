//! oh-my-pi adapter (`omp --mode rpc`, protocol v2) for the `Harness` port (harness.md
//! "omp adapter").

pub mod files;
pub mod frame;
pub mod protocol;

#[cfg(test)]
mod tests;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mage_core::domain::HarnessKind;
use serde_json::{Value, json};
use tokio::sync::{OnceCell, oneshot};

use self::files::{EVENT_FILTER, INTERNAL_TOOLS, OmpPaths, PROVIDER, policy_tools};
use self::protocol::{Frame, OmpProtocol, ToolNames, prompt_command};
use crate::adapters::driven::harness_common::lines::DEFAULT_MAX_LINE;
use crate::adapters::driven::harness_common::session::{HANDSHAKE_STEP, LaunchSpec, MCP_READINESS, Session, Shared, Timing, launch};
use crate::adapters::driven::harness_common::{
    HarnessConfig, ModelTable, SessionFiles, allowlisted_env, probe_version, scratch_abs, system_append_text, workspace_dir,
};
use crate::adapters::driven::harness_common::redact::Redactor;
use crate::ports::executor::{Env, EnvPath, FileAccess, IsolationMode};
use crate::ports::harness::{
    CredentialModeKind, CredentialsMode, ExitReason, Harness, HarnessError, HarnessInfo, HarnessSession, HarnessSpec, LAUNCH_DIR,
    SessionInfo, ToolRef,
};

/// spawn → `ready`.
pub const READY_DEADLINE: Duration = Duration::from_secs(30);

pub struct OmpHarness {
    cfg: HarnessConfig,
    models: ModelTable,
    isolation: IsolationMode,
    probed: OnceCell<HarnessInfo>,
}

impl OmpHarness {
    /// `isolation` = `runner.isolation`: `LocalLogin` is advertised only in dev mode.
    pub fn new(cfg: HarnessConfig, models: ModelTable, isolation: IsolationMode) -> Self {
        Self { cfg, models, isolation, probed: OnceCell::new() }
    }

    fn supported_modes(&self) -> Vec<CredentialModeKind> {
        let mut modes = vec![CredentialModeKind::Gateway];
        if self.isolation == IsolationMode::Dev {
            modes.push(CredentialModeKind::LocalLogin);
        }
        modes
    }

    async fn write_files(&self, env: &dyn Env, spec: &HarnessSpec) -> Result<(), HarnessError> {
        let files = SessionFiles { env, base: &spec.session_dir };
        files.dir("", FileAccess::AgentReadOnly).await?;
        files.dir("omp", FileAccess::AgentReadOnly).await?;
        if let Some(models) = files::models_yml(spec, self.models.get(spec.model.as_str()).copied().unwrap_or_default()) {
            files.dir("omp/agent", FileAccess::AgentCreateOnly).await?;
            files.file("omp/agent/models.yml", &models).await?;
            files.file("omp/agent/config.yml", b"{}\n").await?;
            files.file("omp/agent/mcp.json", b"{\"mcpServers\":{}}\n").await?;
        }
        files.file("omp/overlay.yml", &files::overlay_yml(spec)).await?;
        files.file("omp/system-append.md", system_append_text(env, spec).as_bytes()).await?;
        files.dir("omp/sessions", FileAccess::AgentWritable).await?;
        env.make_dir(&EnvPath::scratch(&format!("{LAUNCH_DIR}/.omp"))?, FileAccess::AgentReadOnly).await?;
        env.put_file(&EnvPath::scratch(&format!("{LAUNCH_DIR}/.omp/mcp.json"))?, &files::mcp_json(spec), FileAccess::AgentReadOnly).await?;
        Ok(())
    }
}

#[async_trait]
impl Harness for OmpHarness {
    fn kind(&self) -> HarnessKind {
        HarnessKind::Omp
    }

    async fn probe(&self) -> Result<HarnessInfo, HarnessError> {
        self.probed
            .get_or_try_init(|| async {
                let version = probe_version(&self.cfg.omp.binary, &self.cfg.omp.supported).await?;
                Ok(HarnessInfo { kind: HarnessKind::Omp, version, credential_modes: self.supported_modes() })
            })
            .await
            .cloned()
    }

    async fn start(&self, env: Arc<dyn Env>, spec: HarnessSpec) -> Result<Box<dyn HarnessSession>, HarnessError> {
        spec.validate_in(env.as_ref(), &self.supported_modes(), false).await?;
        let version = self.probe().await?.version;
        self.write_files(env.as_ref(), &spec).await?;
        let paths = OmpPaths {
            session: scratch_abs(env.as_ref(), spec.session_dir.as_str()),
            workspace: spec.cwd.as_ref().map(|cwd| workspace_dir(env.as_ref(), cwd)),
        };
        let mut env_vars = files::env_vars(&spec, &self.cfg.omp, &paths);
        env_vars.extend(allowlisted_env(&spec));
        let tools = ToolNames::new(&spec.mcp_servers);
        let (proto, ready) = OmpProtocol::new(env.scratch_root(), tools.clone());
        let launch_spec = LaunchSpec {
            kind: HarnessKind::Omp,
            run_id: spec.run_id,
            argv: files::argv(&self.cfg.omp.binary, &spec, &paths),
            env: env_vars,
            redactor: Redactor::for_spec(&spec),
            log_dir: self.cfg.log_dir.clone(),
            timing: Timing { stall: self.cfg.stall_timeout(), abort_deadline: self.cfg.abort_deadline() },
            max_line: DEFAULT_MAX_LINE,
        };
        let shared = launch(env.clone(), launch_spec, proto).await?;
        let info = handshake(&shared, &spec, &tools, version, ready).await?;
        Ok(Box::new(Session::new(shared, info)))
    }
}

/// Steps 1–6 of harness.md "Handshake"; on any failure the process is terminated first.
async fn handshake(
    shared: &Arc<Shared<OmpProtocol>>,
    spec: &HarnessSpec,
    tools: &ToolNames,
    version: String,
    ready: oneshot::Receiver<Frame>,
) -> Result<SessionInfo, HarnessError> {
    let core = &shared.core;
    let ready = match tokio::time::timeout(READY_DEADLINE, ready).await {
        Ok(Ok(frame)) => frame,
        _ => return Err(shared.fail_unavailable("omp did not send ready within 30 s").await),
    };
    let v2 = ready.get("supportedProtocolVersions").and_then(Value::as_array).is_some_and(|v| v.iter().any(|x| x.as_u64() == Some(2)));
    if !v2 {
        let err = HarnessError::NotFound("omp without RPC protocol v2".into());
        return Err(shared.fail_start(ExitReason::Crashed, err).await);
    }
    if let Some(max) = ready.get("maxFrameBytes").and_then(Value::as_u64) {
        let limit = usize::try_from(max).unwrap_or(usize::MAX).saturating_add(1024);
        core.max_line.store(limit.min(DEFAULT_MAX_LINE), std::sync::atomic::Ordering::Relaxed);
    }
    if let Some(max) = ready.get("maxReassembledFrameBytes").and_then(Value::as_u64) {
        shared.proto.set_max_reassembled(usize::try_from(max).unwrap_or(usize::MAX));
    }
    command(shared, json!({ "type": "negotiate_protocol", "protocolVersion": 2 })).await?;
    let filter = command(shared, json!({ "type": "set_event_filter", "events": EVENT_FILTER })).await?;
    let echoed: BTreeSet<&str> =
        filter.get("data").and_then(|d| d.get("events")).and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
    if echoed != EVENT_FILTER.iter().copied().collect::<BTreeSet<_>>() {
        let err = HarnessError::Unavailable(format!("omp echoed event filter {echoed:?}"));
        return Err(shared.fail_start(ExitReason::Crashed, err).await);
    }
    command(shared, json!({ "type": "set_steering_mode", "mode": "all" })).await?;
    let state = inventory(shared, spec, tools).await?;
    let data = state.get("data").cloned().unwrap_or(Value::Null);
    shared.proto.set_session_file(data.get("sessionFile").and_then(Value::as_str).map(str::to_owned));
    let info = SessionInfo {
        harness: HarnessKind::Omp,
        version,
        native_session_id: data.get("sessionId").and_then(Value::as_str).unwrap_or_default().to_owned(),
        model: data.get("model").and_then(|m| m.get("id")).and_then(Value::as_str).unwrap_or_default().to_owned(),
        tools: tool_refs(&data, tools),
        pid: core.pid,
        pgid: core.pid,
    };
    core.ready(info.clone());
    core.open_turn();
    let id = core.next_id("p");
    let response = shared.proto.expect(&id);
    core.write_line(prompt_command(&id, &spec.prompt)).await?;
    match tokio::time::timeout(HANDSHAKE_STEP, response).await {
        Ok(Ok(frame)) if frame.get("success").and_then(Value::as_bool) == Some(true) => Ok(info),
        Ok(Ok(frame)) => {
            let err = HarnessError::Unavailable(format!("first prompt rejected: {}", frame.get("error").map(Value::to_string).unwrap_or_default()));
            Err(shared.fail_start(ExitReason::Crashed, err).await)
        }
        _ => Err(shared.fail_unavailable("first prompt not acknowledged within 10 s").await),
    }
}

/// Sends one handshake command and awaits its `response` (10 s). `success: false` →
/// `Unavailable`.
async fn command(shared: &Arc<Shared<OmpProtocol>>, mut cmd: Value) -> Result<Frame, HarnessError> {
    let core = &shared.core;
    let id = core.next_id("h");
    cmd["id"] = Value::String(id.clone());
    let what = cmd.get("type").and_then(Value::as_str).unwrap_or_default().to_owned();
    let response = shared.proto.expect(&id);
    if let Err(e) = core.write_line(cmd.to_string().into_bytes()).await {
        return Err(shared.fail_start(ExitReason::Crashed, e).await);
    }
    match tokio::time::timeout(HANDSHAKE_STEP, response).await {
        Ok(Ok(frame)) if frame.get("success").and_then(Value::as_bool) == Some(true) => Ok(frame),
        Ok(Ok(frame)) => {
            let err = HarnessError::Unavailable(format!("omp {what} failed: {}", frame.get("error").map(Value::to_string).unwrap_or_default()));
            Err(shared.fail_start(ExitReason::Crashed, err).await)
        }
        _ => Err(shared.fail_unavailable(&format!("omp {what} not answered within 10 s")).await),
    }
}

/// Fail-closed inventory check on `get_state` (H-ISO-2): MCP tools equal the expected set (missing
/// → wait for MCP readiness, then `Unavailable`; extra → `IsolationViolation`), built-ins ⊆ policy
/// ∪ omp internals, model = requested.
async fn inventory(shared: &Arc<Shared<OmpProtocol>>, spec: &HarnessSpec, tools: &ToolNames) -> Result<Frame, HarnessError> {
    let deadline = tokio::time::Instant::now() + MCP_READINESS;
    loop {
        let state = command(shared, json!({ "type": "get_state" })).await?;
        let data = state.get("data").cloned().unwrap_or(Value::Null);
        match check_inventory(&data, spec, tools) {
            Ok(()) => return Ok(state),
            Err(InventoryError::Violation(detail)) => {
                let err = HarnessError::IsolationViolation(detail.clone());
                return Err(shared.fail_start(ExitReason::IsolationViolation(detail), err).await);
            }
            Err(InventoryError::Missing(missing)) => {
                if tokio::time::Instant::now() >= deadline {
                    let msg = format!("MCP tools not available after 30 s: {}", missing.join(", "));
                    return Err(shared.fail_start(ExitReason::Crashed, HarnessError::Unavailable(msg)).await);
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum InventoryError {
    Missing(Vec<String>),
    Violation(String),
}

/// Pure inventory comparison on a `get_state` payload.
pub fn check_inventory(data: &Value, spec: &HarnessSpec, tools: &ToolNames) -> Result<(), InventoryError> {
    let model = data.get("model");
    let model_id = model.and_then(|m| m.get("id")).and_then(Value::as_str).unwrap_or_default();
    let provider = model.and_then(|m| m.get("provider")).and_then(Value::as_str).unwrap_or_default();
    let provider_ok = !matches!(spec.credentials, CredentialsMode::Gateway { .. }) || provider == PROVIDER;
    if model_id != spec.model.as_str() || !provider_ok {
        return Err(InventoryError::Violation(format!("model {provider}/{model_id}, expected {}", files::model_ref(spec))));
    }
    let allowed: BTreeSet<&str> = policy_tools(spec.builtin_tools).iter().chain(INTERNAL_TOOLS).copied().collect();
    let mut seen = std::collections::HashSet::new();
    for name in data.get("dumpTools").and_then(Value::as_array).into_iter().flatten().filter_map(|t| t.get("name").and_then(Value::as_str)) {
        if name.starts_with("mcp__") {
            match tools.expected(name) {
                Some(t) => {
                    seen.insert(t.clone());
                }
                None => return Err(InventoryError::Violation(format!("unexpected MCP tool {name}"))),
            }
        } else if !allowed.contains(name) {
            return Err(InventoryError::Violation(format!("unexpected built-in tool {name}")));
        }
    }
    let mut missing: Vec<String> = tools.expected_names().filter(|(_, t)| !seen.contains(*t)).map(|(n, _)| n.clone()).collect();
    if missing.is_empty() {
        Ok(())
    } else {
        missing.sort();
        Err(InventoryError::Missing(missing))
    }
}

fn tool_refs(data: &Value, tools: &ToolNames) -> Vec<ToolRef> {
    data.get("dumpTools")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|t| t.get("name").and_then(Value::as_str))
        .map(|name| tools.normalize(name))
        .collect()
}
