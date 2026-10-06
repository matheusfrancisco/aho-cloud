//! Claude Code adapter (`claude -p` with stream-json input and output) for the `Harness` port
//! (harness.md "Claude Code adapter").

pub mod files;
pub mod protocol;

#[cfg(test)]
mod tests;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mage_core::domain::HarnessKind;
use tokio::sync::OnceCell;

use self::files::{ClaudePaths, PLACEHOLDER_SETTINGS};
use self::protocol::{ClaudeProtocol, Expectations, StartFailure, TranscriptRoot};
use crate::adapters::driven::harness_common::config::expand_home;
use crate::adapters::driven::harness_common::lines::DEFAULT_MAX_LINE;
use crate::adapters::driven::harness_common::redact::Redactor;
use crate::adapters::driven::harness_common::session::{LaunchSpec, Session, Timing, launch};
use crate::adapters::driven::harness_common::{
    HarnessConfig, SessionFiles, allowlisted_env, probe_version, scratch_abs, system_append_text, workspace_dir,
};
use crate::ports::executor::{Env, FileAccess, IsolationMode};
use crate::ports::harness::{
    CredentialModeKind, CredentialsMode, ExitReason, Harness, HarnessError, HarnessInfo, HarnessSession, HarnessSpec, LAUNCH_DIR,
};

/// First user message written → `system/init`.
pub const INIT_DEADLINE: Duration = Duration::from_secs(60);

/// Managed settings that would silently change policy on the runner host.
pub const MANAGED_SETTINGS: &[&str] =
    &["/etc/claude-code/managed-settings.json", "/Library/Application Support/ClaudeCode/managed-settings.json"];

pub struct ClaudeHarness {
    cfg: HarnessConfig,
    isolation: IsolationMode,
    probed: OnceCell<HarnessInfo>,
}

impl ClaudeHarness {
    /// `isolation` = `runner.isolation`: `LocalLogin` is advertised only in dev mode; `Vertex` only
    /// with `runner.harness.allow_direct_vertex`.
    pub fn new(cfg: HarnessConfig, isolation: IsolationMode) -> Self {
        Self { cfg, isolation, probed: OnceCell::new() }
    }

    fn supported_modes(&self) -> Vec<CredentialModeKind> {
        let mut modes = vec![CredentialModeKind::Gateway];
        if self.isolation == IsolationMode::Dev {
            modes.push(CredentialModeKind::LocalLogin);
        }
        if self.cfg.allow_direct_vertex {
            modes.push(CredentialModeKind::Vertex);
        }
        modes
    }

    async fn write_files(&self, env: &dyn Env, spec: &HarnessSpec, paths: &ClaudePaths, system_append: &str) -> Result<(), HarnessError> {
        let files = SessionFiles { env, base: &spec.session_dir };
        files.dir("", FileAccess::AgentReadOnly).await?;
        files.dir("claude", FileAccess::AgentReadOnly).await?;
        if !matches!(spec.credentials, CredentialsMode::LocalLogin) {
            files.dir("claude/config", FileAccess::AgentCreateOnly).await?;
            files.file("claude/config/settings.json", PLACEHOLDER_SETTINGS).await?;
        }
        files.file("claude/settings.json", &files::settings_json(spec, paths)).await?;
        files.file("claude/mcp.json", &files::mcp_json(spec)).await?;
        files.file("claude/system-append.md", system_append.as_bytes()).await?;
        if let CredentialsMode::Gateway { token, .. } = &spec.credentials {
            files.file("claude/gateway-token", token.expose().as_bytes()).await?;
        }
        Ok(())
    }
}

#[async_trait]
impl Harness for ClaudeHarness {
    fn kind(&self) -> HarnessKind {
        HarnessKind::ClaudeCode
    }

    async fn probe(&self) -> Result<HarnessInfo, HarnessError> {
        self.probed
            .get_or_try_init(|| async {
                if let Some(path) = MANAGED_SETTINGS.iter().find(|p| Path::new(p).exists()) {
                    return Err(HarnessError::Unsupported(format!("managed Claude Code settings present at {path}")));
                }
                let version = probe_version(&self.cfg.claude.binary, &self.cfg.claude.supported).await?;
                Ok(HarnessInfo { kind: HarnessKind::ClaudeCode, version, credential_modes: self.supported_modes() })
            })
            .await
            .cloned()
    }

    async fn start(&self, env: Arc<dyn Env>, spec: HarnessSpec) -> Result<Box<dyn HarnessSession>, HarnessError> {
        spec.validate_in(env.as_ref(), &self.supported_modes(), self.cfg.allow_direct_vertex).await?;
        let version = self.probe().await?.version;
        let paths = ClaudePaths {
            session: scratch_abs(env.as_ref(), spec.session_dir.as_str()),
            workspace: spec.cwd.as_ref().map(|cwd| workspace_dir(env.as_ref(), cwd)),
        };
        let system_append = system_append_text(env.as_ref(), &spec);
        self.write_files(env.as_ref(), &spec, &paths, &system_append).await?;
        let session_id = uuid::Builder::from_random_bytes(rand::random()).into_uuid().to_string();
        let launch_cwd = scratch_abs(env.as_ref(), LAUNCH_DIR);
        let transcript = match spec.credentials {
            CredentialsMode::LocalLogin => {
                TranscriptRoot::Host(expand_home(&self.cfg.claude.dev_config_dir).join("projects").join(files::project_dir_name(&launch_cwd)))
            }
            _ => TranscriptRoot::Scratch(spec.session_dir.join("claude/config/projects")?),
        };
        let expect = Expectations::new(
            &spec.mcp_servers,
            spec.builtin_tools,
            launch_cwd,
            spec.model.as_str().to_owned(),
            matches!(spec.credentials, CredentialsMode::Gateway { .. }),
            version,
        );
        let (proto, init) = ClaudeProtocol::new(expect, session_id.clone(), transcript);
        let mut env_vars = files::env_vars(&spec, &self.cfg.claude, &paths);
        env_vars.extend(allowlisted_env(&spec));
        let launch_spec = LaunchSpec {
            kind: HarnessKind::ClaudeCode,
            run_id: spec.run_id,
            argv: files::argv(&self.cfg.claude.binary, &spec, &paths, &session_id, &system_append),
            env: env_vars,
            redactor: Redactor::for_spec(&spec),
            log_dir: self.cfg.log_dir.clone(),
            timing: Timing { stall: self.cfg.stall_timeout(), abort_deadline: self.cfg.abort_deadline() },
            max_line: DEFAULT_MAX_LINE,
        };
        let shared = launch(env.clone(), launch_spec, proto).await?;
        if let Err(e) = shared.core.write_line(files::user_message(&spec.prompt)).await {
            return Err(shared.fail_start(ExitReason::Crashed, e).await);
        }
        match tokio::time::timeout(INIT_DEADLINE, init).await {
            Ok(Ok(Ok(info))) => Ok(Box::new(Session::new(shared, info))),
            Ok(Ok(Err(StartFailure::Violation(detail)))) => {
                let err = HarnessError::IsolationViolation(detail.clone());
                Err(shared.fail_start(ExitReason::IsolationViolation(detail), err).await)
            }
            Ok(Ok(Err(StartFailure::Unavailable(reason)))) => Err(shared.fail_start(ExitReason::Crashed, HarnessError::Unavailable(reason)).await),
            Ok(Err(_)) => Err(shared.fail_unavailable("claude exited before init").await),
            Err(_) => Err(shared.fail_unavailable("claude sent no init within 60 s").await),
        }
    }
}
