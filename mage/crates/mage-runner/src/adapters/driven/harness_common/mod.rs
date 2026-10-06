//! Internals shared by the harness adapters (harness.md "Adapter internals shared by both
//! adapters"): config, redaction, event pump, stdout/stderr handling, the session runtime, and
//! helpers for session files and probing.

pub mod config;
pub mod lines;
pub mod pump;
pub mod redact;
pub mod session;
pub mod stderr;
#[cfg(test)]
pub mod testkit;

use std::time::Duration;

pub use config::{ClaudeConfig, HarnessConfig, ModelMeta, ModelTable, OmpConfig, VersionReq};

use crate::ports::executor::{Env, ExecError, FileAccess, RelPath};
use crate::ports::harness::{BuiltinToolPolicy, HarnessError, HarnessSpec, RUN_MCP_SERVER};

/// Absolute workspace directory the agent works in (`Env::root()` joined with `spec.cwd`).
pub fn workspace_dir(env: &dyn Env, cwd: &RelPath) -> String {
    let root = env.root().trim_end_matches('/');
    if cwd.is_root() { root.to_owned() } else { format!("{root}/{}", cwd.as_str()) }
}

/// Absolute path of `rel` inside the scratch area as Env processes see it.
pub fn scratch_abs(env: &dyn Env, rel: &str) -> String {
    format!("{}/{rel}", env.scratch_root().trim_end_matches('/'))
}

/// Adapter preamble (harness.md "Launch directory") + `spec.system_append`.
pub fn system_append_text(env: &dyn Env, spec: &HarnessSpec) -> String {
    let preamble = match (&spec.builtin_tools, &spec.cwd) {
        (BuiltinToolPolicy::WorkspaceRw, Some(cwd)) => {
            let ws = workspace_dir(env, cwd);
            format!(
                "Workspace: {ws} (read/write).  Your shell starts in an empty directory: run `cd {ws}` first and use absolute \
                 paths with file tools. Tools named {RUN_MCP_SERVER}.* act on the same workspace."
            )
        }
        (BuiltinToolPolicy::ReadOnly, Some(cwd)) => {
            let ws = workspace_dir(env, cwd);
            format!("Workspace: {ws} (read-only).  Use absolute paths with file tools. Tools named {RUN_MCP_SERVER}.* act on the same workspace.")
        }
        _ => String::new(),
    };
    match (preamble.is_empty(), spec.system_append.is_empty()) {
        (true, _) => spec.system_append.clone(),
        (false, true) => preamble,
        (false, false) => format!("{preamble}\n\n{}", spec.system_append),
    }
}

/// `env_allowlist` passthrough from the runner's own environment (unset names are skipped).
pub fn allowlisted_env(spec: &HarnessSpec) -> Vec<(String, String)> {
    spec.env_allowlist.iter().filter_map(|name| std::env::var(name).ok().map(|v| (name.clone(), v))).collect()
}

/// Session file writer: `put_file`/`make_dir` under `spec.session_dir` in the scratch area.
pub struct SessionFiles<'a> {
    pub env: &'a dyn Env,
    pub base: &'a RelPath,
}

impl SessionFiles<'_> {
    pub fn rel(&self, sub: &str) -> Result<RelPath, ExecError> {
        self.base.join(sub)
    }

    pub async fn dir(&self, sub: &str, access: FileAccess) -> Result<(), HarnessError> {
        let rel = if sub.is_empty() { self.base.clone() } else { self.rel(sub)? };
        let path = crate::ports::executor::EnvPath::new(crate::ports::executor::Area::Scratch, rel);
        Ok(self.env.make_dir(&path, access).await?)
    }

    pub async fn file(&self, sub: &str, bytes: &[u8]) -> Result<(), HarnessError> {
        let path = crate::ports::executor::EnvPath::new(crate::ports::executor::Area::Scratch, self.rel(sub)?);
        self.env.put_file(&path, bytes, FileAccess::AgentReadOnly).await?;
        Ok(())
    }
}

/// Runs `<binary> --version` on the runner host and checks it against `supported`.
pub async fn probe_version(binary: &str, supported: &VersionReq) -> Result<String, HarnessError> {
    let run = tokio::process::Command::new(binary).arg("--version").stdin(std::process::Stdio::null()).kill_on_drop(true).output();
    let out = match tokio::time::timeout(Duration::from_secs(30), run).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => return Err(HarnessError::NotFound(format!("{binary}: {e}"))),
        Err(_) => return Err(HarnessError::NotFound(format!("{binary} --version timed out"))),
    };
    if !out.status.success() {
        return Err(HarnessError::NotFound(format!("{binary} --version failed: {}", out.status)));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let version = config::extract_version(&text).ok_or_else(|| HarnessError::NotFound(format!("{binary}: no version in {text:?}")))?;
    if !supported.matches(&version) {
        return Err(HarnessError::NotFound(format!("{binary} {version} is outside the supported range {}", supported.as_str())));
    }
    Ok(version)
}
