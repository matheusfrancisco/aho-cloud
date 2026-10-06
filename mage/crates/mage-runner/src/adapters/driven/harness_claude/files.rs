//! Claude Code session files, command line, and environment (harness.md "Claude Code adapter").

use serde_json::{Map, Value, json};

use crate::adapters::driven::harness_common::{ClaudeConfig, config::expand_home};
use crate::ports::harness::{BuiltinToolPolicy, CredentialsMode, HarnessSpec};

/// Built-ins per policy (`--tools`).
pub fn policy_tools(policy: BuiltinToolPolicy) -> &'static [&'static str] {
    match policy {
        BuiltinToolPolicy::None => &[],
        BuiltinToolPolicy::ReadOnly => &["Read", "Grep", "Glob"],
        BuiltinToolPolicy::WorkspaceRw => &["Read", "Grep", "Glob", "Edit", "Write", "Bash", "TodoWrite"],
    }
}

/// Built-ins Claude Code loads even when not requested (measured with 2.1.269, `--tools ""`: none).
pub const INTERNAL_TOOLS: &[&str] = &[];

pub const DENIED_TOOLS: &[&str] = &["WebFetch", "WebSearch", "Agent"];

/// `init.apiKeySource` when the key comes from `apiKeyHelper` (measured with 2.1.269).
pub const HELPER_KEY_SOURCE: &str = "apiKeyHelper";

/// Permission modes accepted in `init`: `dontAsk` as requested, or `default`, which 2.1.269 forces
/// whenever `CLAUDE_CODE_SUBPROCESS_ENV_SCRUB` is set (with an explicit `--allowedTools` list it
/// denies everything else in `-p` mode, like `dontAsk`).
pub const ACCEPTED_PERMISSION_MODES: &[&str] = &["dontAsk", "default"];

/// Absolute paths inside the Env the command line and settings refer to.
pub struct ClaudePaths {
    /// `<scratch>/<session_dir>`.
    pub session: String,
    /// `--add-dir` (`None` for `BuiltinToolPolicy::None`).
    pub workspace: Option<String>,
}

/// `claude/config/settings.json` placeholder (user layer of the fresh config dir).
pub const PLACEHOLDER_SETTINGS: &[u8] = b"{\"disableAllHooks\": true}\n";

/// `claude/settings.json`, passed with `--settings` (flag layer, above user/project/local).
pub fn settings_json(spec: &HarnessSpec, paths: &ClaudePaths) -> Vec<u8> {
    let mut doc = Map::new();
    doc.insert("disableAllHooks".into(), Value::Bool(true));
    if matches!(spec.credentials, CredentialsMode::Gateway { .. }) {
        doc.insert("apiKeyHelper".into(), Value::String(format!("cat {}", shell_quote(&format!("{}/claude/gateway-token", paths.session)))));
    }
    doc.insert("permissions".into(), json!({ "defaultMode": "dontAsk", "deny": DENIED_TOOLS }));
    pretty(&Value::Object(doc))
}

/// `claude/mcp.json`, passed with `--mcp-config` + `--strict-mcp-config`.
pub fn mcp_json(spec: &HarnessSpec) -> Vec<u8> {
    let mut servers = Map::new();
    for s in &spec.mcp_servers {
        servers.insert(
            s.name.as_str().to_owned(),
            json!({
                "type": "http",
                "url": s.url,
                "headers": { "Authorization": format!("Bearer {}", s.bearer.expose()) },
                "timeout": u64::try_from(s.tool_timeout.as_millis()).unwrap_or(u64::MAX),
            }),
        );
    }
    pretty(&json!({ "mcpServers": servers }))
}

/// `--allowedTools` (one space-separated value): policy built-ins and `mcp__<server>` per server.
pub fn allowed_tools(spec: &HarnessSpec) -> String {
    policy_tools(spec.builtin_tools)
        .iter()
        .map(|t| (*t).to_owned())
        .chain(spec.mcp_servers.iter().map(|s| format!("mcp__{}", s.name)))
        .collect::<Vec<_>>()
        .join(" ")
}

pub fn argv(binary: &str, spec: &HarnessSpec, paths: &ClaudePaths, session_id: &str, system_append: &str) -> Vec<String> {
    let s = &paths.session;
    let mut argv: Vec<String> = [
        binary,
        "-p",
        "--input-format",
        "stream-json",
        "--output-format",
        "stream-json",
        "--verbose",
        "--include-partial-messages",
        "--session-id",
        session_id,
        "--model",
        spec.model.as_str(),
    ]
    .iter()
    .map(|a| (*a).to_owned())
    .collect();
    argv.extend([
        "--mcp-config".into(),
        format!("{s}/claude/mcp.json"),
        "--strict-mcp-config".into(),
        "--settings".into(),
        format!("{s}/claude/settings.json"),
        "--setting-sources".into(),
        "user".into(),
        "--permission-mode".into(),
        "dontAsk".into(),
        "--tools".into(),
        policy_tools(spec.builtin_tools).join(","),
        "--allowedTools".into(),
        allowed_tools(spec),
        "--disallowedTools".into(),
        DENIED_TOOLS.join(","),
        "--disable-slash-commands".into(),
    ]);
    if !system_append.is_empty() {
        argv.push("--append-system-prompt".into());
        argv.push(system_append.to_owned());
    }
    if let Some(ws) = &paths.workspace {
        argv.push("--add-dir".into());
        argv.push(ws.clone());
    }
    if let Some(micros) = spec.budget.max_cost_micros {
        argv.push("--max-budget-usd".into());
        argv.push(format!("{}.{:06}", micros / 1_000_000, micros % 1_000_000));
    }
    argv
}

/// `CLAUDE_CONFIG_DIR` for this spec.
pub fn config_dir(spec: &HarnessSpec, cfg: &ClaudeConfig, paths: &ClaudePaths) -> String {
    match spec.credentials {
        CredentialsMode::LocalLogin => expand_home(&cfg.dev_config_dir).to_string_lossy().into_owned(),
        _ => format!("{}/claude/config", paths.session),
    }
}

/// Harness variables (no secret); the `env_allowlist` passthrough is appended by the caller.
pub fn env_vars(spec: &HarnessSpec, cfg: &ClaudeConfig, paths: &ClaudePaths) -> Vec<(String, String)> {
    let mut vars: Vec<(String, String)> = vec![("CLAUDE_CONFIG_DIR".into(), config_dir(spec, cfg, paths))];
    match &spec.credentials {
        CredentialsMode::Gateway { base_url, .. } => vars.push(("ANTHROPIC_BASE_URL".into(), base_url.trim_end_matches('/').to_owned())),
        CredentialsMode::Vertex { project_id, region } => {
            vars.push(("CLAUDE_CODE_USE_VERTEX".into(), "1".into()));
            vars.push(("ANTHROPIC_VERTEX_PROJECT_ID".into(), project_id.clone()));
            vars.push(("CLOUD_ML_REGION".into(), region.clone()));
        }
        CredentialsMode::LocalLogin => vars.push(("ENABLE_TOOL_SEARCH".into(), "false".into())),
    }
    for name in [
        "CLAUDE_CODE_DISABLE_CLAUDE_MDS",
        "CLAUDE_CODE_DISABLE_AUTO_MEMORY",
        "CLAUDE_CODE_SUBPROCESS_ENV_SCRUB",
        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
        "CLAUDE_CODE_DISABLE_BACKGROUND_TASKS",
        "CLAUDE_AGENT_SDK_DISABLE_BUILTIN_AGENTS",
        "CLAUDE_CODE_STARTUP_FAILURE_RESULTS",
    ] {
        vars.push((name.into(), "1".into()));
    }
    if let Some(max) = spec.mcp_servers.iter().map(|s| s.tool_timeout).max() {
        vars.push(("MCP_TOOL_TIMEOUT".into(), max.as_millis().to_string()));
    }
    vars
}

/// Claude Code's project directory name for a cwd (`/a/b.c` → `-a-b-c`).
pub fn project_dir_name(cwd: &str) -> String {
    cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

/// One stdin user message (`client_composed`: no `@path` expansion, no slash commands).
pub fn user_message(text: &str) -> Vec<u8> {
    json!({
        "type": "user",
        "message": { "role": "user", "content": [{ "type": "text", "text": text }] },
        "parent_tool_use_id": null,
        "client_composed": true,
    })
    .to_string()
    .into_bytes()
}

/// Interrupt `control_request`; `cancel_queued` only when `init.capabilities` advertises it.
pub fn interrupt_request(id: &str, cancel_queued: bool) -> Vec<u8> {
    let mut request = json!({ "subtype": "interrupt" });
    if cancel_queued {
        request["cancel_queued"] = Value::Bool(true);
    }
    json!({ "type": "control_request", "request_id": id, "request": request }).to_string().into_bytes()
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn pretty(v: &Value) -> Vec<u8> {
    let mut out = serde_json::to_vec_pretty(v).unwrap_or_default();
    out.push(b'\n');
    out
}
