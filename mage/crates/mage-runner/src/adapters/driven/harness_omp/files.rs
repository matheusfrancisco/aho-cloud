//! omp session files, command line, and environment (harness.md "omp adapter").

use serde_json::{Map, Value, json};

use crate::adapters::driven::harness_common::{ModelMeta, OmpConfig, config::expand_home};
use crate::ports::harness::{BuiltinToolPolicy, CredentialsMode, HarnessSpec};

/// Provider name of the Mage gateway in `models.yml`.
pub const PROVIDER: &str = "mage";

/// Built-ins per policy (`BUILTIN_TOOL_NAMES` of omp 18.3.1 that its `--tools` accepts under the
/// overlay; `find` and `ast_grep` are not available there).
pub fn policy_tools(policy: BuiltinToolPolicy) -> &'static [&'static str] {
    match policy {
        BuiltinToolPolicy::None => &[],
        BuiltinToolPolicy::ReadOnly => &["read", "grep", "glob"],
        BuiltinToolPolicy::WorkspaceRw => &["read", "grep", "glob", "edit", "write", "bash", "todo"],
    }
}

/// Built-ins omp loads even when not requested (measured with omp 18.3.1, `--no-tools`: none).
pub const INTERNAL_TOOLS: &[&str] = &[];

/// Session-event filter sent with `set_event_filter`.
pub const EVENT_FILTER: &[&str] = &[
    "agent_start",
    "agent_end",
    "message_update",
    "message_end",
    "tool_execution_start",
    "tool_execution_end",
    "auto_compaction_start",
    "auto_compaction_end",
    "auto_retry_start",
    "auto_retry_end",
    "retry_fallback_applied",
    "notice",
];

/// Discovery providers disabled in the overlay: every provider except `native`.
pub const DISABLED_PROVIDERS: &[&str] = &[
    "claude",
    "claude-plugins",
    "claude-md",
    "agent-plugins",
    "agents",
    "agents-md",
    "codex",
    "gemini",
    "opencode",
    "cursor",
    "windsurf",
    "cline",
    "github",
    "vscode",
    "mcp-json",
    "ssh-json",
    "omp-plugins",
];

/// `--model` value and the overlay `modelRoles` target.
pub fn model_ref(spec: &HarnessSpec) -> String {
    match spec.credentials {
        CredentialsMode::Gateway { .. } => format!("{PROVIDER}/{}", spec.model),
        _ => spec.model.as_str().to_owned(),
    }
}

/// `launch/.omp/mcp.json` (project-level MCP config; the launch dir is omp's project).
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

/// `omp/agent/models.yml` (Gateway only).
pub fn models_yml(spec: &HarnessSpec, meta: ModelMeta) -> Option<Vec<u8>> {
    let CredentialsMode::Gateway { base_url, token } = &spec.credentials else { return None };
    let model = spec.model.as_str();
    let doc = json!({
        "providers": {
            PROVIDER: {
                "baseUrl": base_url.trim_end_matches('/'),
                "apiKey": token.expose(),
                "api": "anthropic-messages",
                "authHeader": true,
                "models": [{
                    "id": model,
                    "name": format!("{model} (Mage gateway)"),
                    "reasoning": meta.reasoning,
                    "input": ["text", "image"],
                    "contextWindow": meta.context_window,
                    "maxTokens": meta.max_tokens,
                }],
            }
        }
    });
    Some(yaml(&doc).into_bytes())
}

/// `omp/overlay.yml`, passed with `--config` (overlay layer: above project and global config).
/// Beyond harness.md: `tools.xdev: false` (omp 18.3.1 otherwise mounts MCP tools as `xd://`
/// devices outside the tool list and `get_state.dumpTools`) and `bash.autoBackground.enabled:
/// false` (RPC hosts default it on; a steer would background a running command and wake the
/// agent after it yielded).
pub fn overlay_yml(spec: &HarnessSpec) -> Vec<u8> {
    let model = model_ref(spec);
    let doc = json!({
        "mcp": { "enableProjectConfig": true },
        "disabledProviders": DISABLED_PROVIDERS,
        "disabledExtensions": [
            "context-file:project:AGENTS.md",
            "context-file:project:CLAUDE.md",
            "context-file:project:GEMINI.md",
            "context-file:project:copilot-instructions.md",
        ],
        "tools": { "approvalMode": "yolo", "xdev": false },
        "bash": { "autoBackground": { "enabled": false } },
        "browser": { "enabled": false },
        "skills": { "enabled": false },
        "advisor": { "enabled": false },
        "autolearn": { "enabled": false },
        "memory": { "backend": "off" },
        "memories": { "enabled": false },
        "goal": { "enabled": false },
        "lsp": { "enabled": false },
        "async": { "enabled": false },
        "startup": { "checkUpdate": false },
        "collab": { "autoStart": false },
        "modelRoles": { "default": model, "smol": model, "slow": model, "plan": model },
    });
    yaml(&doc).into_bytes()
}

/// Paths inside the Env the command line refers to.
pub struct OmpPaths {
    /// `<scratch>/<session_dir>` (absolute).
    pub session: String,
    /// Workspace dir granted with `--add-dir` (`None` for `BuiltinToolPolicy::None`).
    pub workspace: Option<String>,
}

pub fn argv(binary: &str, spec: &HarnessSpec, paths: &OmpPaths) -> Vec<String> {
    let s = &paths.session;
    let mut argv: Vec<String> = vec![
        binary.into(),
        "--mode".into(),
        "rpc".into(),
        "--no-ui".into(),
        "--session-dir".into(),
        format!("{s}/omp/sessions"),
        "--config".into(),
        format!("{s}/omp/overlay.yml"),
        "--model".into(),
        model_ref(spec),
        "--append-system-prompt".into(),
        format!("{s}/omp/system-append.md"),
    ];
    for flag in ["--no-extensions", "--no-skills", "--no-rules", "--no-lsp", "--no-title", "--no-pty", "--auto-approve"] {
        argv.push(flag.into());
    }
    argv.push("--max-time".into());
    argv.push((spec.budget.max_wall.as_secs() + 300).to_string());
    if let Some(ws) = &paths.workspace {
        argv.push("--add-dir".into());
        argv.push(ws.clone());
    }
    match spec.builtin_tools {
        BuiltinToolPolicy::None => argv.push("--no-tools".into()),
        policy => {
            argv.push("--tools".into());
            argv.push(policy_tools(policy).join(","));
        }
    }
    argv
}

/// Harness variables (secrets never go here); the `env_allowlist` passthrough is appended by the
/// caller.
pub fn env_vars(spec: &HarnessSpec, cfg: &OmpConfig, paths: &OmpPaths) -> Vec<(String, String)> {
    let agent_dir = match spec.credentials {
        CredentialsMode::LocalLogin => expand_home(&cfg.dev_agent_dir).to_string_lossy().into_owned(),
        _ => format!("{}/omp/agent", paths.session),
    };
    [
        ("PI_CODING_AGENT_DIR", agent_dir.as_str()),
        ("OMP_MCP_STARTUP_TIMEOUT_MS", "0"),
        ("PI_NO_TITLE", "1"),
        ("PI_AUTO_QA", "0"),
        ("OMP_SKIP_SETUP", "1"),
        ("PI_BASH_NO_LOGIN", "1"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect()
}

fn pretty(v: &Value) -> Vec<u8> {
    let mut out = serde_json::to_vec_pretty(v).unwrap_or_default();
    out.push(b'\n');
    out
}

/// Block-style YAML (keys sorted) with every scalar written as a JSON scalar (strings
/// double-quoted, which YAML reads verbatim): no value can turn into YAML syntax. Arrays of
/// scalars use flow style.
pub fn yaml(v: &Value) -> String {
    let mut out = String::new();
    if let Value::Object(map) = v {
        yaml_map(&mut out, map, 0);
    }
    out
}

fn yaml_map(out: &mut String, map: &Map<String, Value>, indent: usize) {
    for (key, value) in sorted(map) {
        out.push_str(&" ".repeat(indent));
        out.push_str(&yaml_key(key));
        out.push(':');
        yaml_value(out, value, indent);
    }
}

fn yaml_value(out: &mut String, value: &Value, indent: usize) {
    match value {
        Value::Object(m) if !m.is_empty() => {
            out.push('\n');
            yaml_map(out, m, indent + 2);
        }
        Value::Array(items) if items.iter().any(|i| i.is_object()) => {
            out.push('\n');
            for item in items {
                out.push_str(&" ".repeat(indent + 2));
                out.push_str("- ");
                match item {
                    Value::Object(m) => {
                        let mut first = true;
                        for (k, v) in sorted(m) {
                            if !first {
                                out.push_str(&" ".repeat(indent + 4));
                            }
                            first = false;
                            out.push_str(&yaml_key(k));
                            out.push(':');
                            yaml_value(out, v, indent + 4);
                        }
                        if m.is_empty() {
                            out.push_str("{}\n");
                        }
                    }
                    scalar => {
                        out.push_str(&scalar.to_string());
                        out.push('\n');
                    }
                }
            }
        }
        other => {
            out.push(' ');
            out.push_str(&other.to_string());
            out.push('\n');
        }
    }
}

/// Keys in byte order, independent of serde_json's `preserve_order` feature.
fn sorted(map: &Map<String, Value>) -> Vec<(&String, &Value)> {
    let mut entries: Vec<_> = map.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    entries
}

fn yaml_key(key: &str) -> String {
    let plain = key.bytes().next().is_some_and(|b| b.is_ascii_alphabetic() || b == b'_')
        && key.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'));
    if plain { key.to_owned() } else { Value::String(key.to_owned()).to_string() }
}
