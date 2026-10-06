//! Executor keys of the `[runner]` config section (executor.md "Runner config (executor keys)").
//! Defaults are the production values; [`ExecHostConfig::dev`] gives the dev profile.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::Deserialize;

use super::postgres::PgStrategy;
use crate::ports::executor::{ExecError, IsolationMode, is_reserved_env_name};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunnerEnvironment {
    Production,
    Dev,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ExecHostConfig {
    pub environment: RunnerEnvironment,
    pub isolation: IsolationMode,
    pub data_dir: PathBuf,
    pub max_envs: u16,
    pub max_sessions_per_repo: u16,
    /// `[runner].server_url`: its host is always on the egress allowlist.
    pub server_url: Option<url::Url>,
    pub agent: AgentConfig,
    pub dev: DevConfig,
    /// Recipe `[toolchain]` name → version → bin dir.
    pub toolchains: BTreeMap<String, BTreeMap<String, PathBuf>>,
    pub ports: PortsConfig,
    pub exec: ExecConfig,
    pub prepare: PrepareConfig,
    pub fork: ForkConfig,
    pub cgroup: CgroupConfig,
    pub postgres: PostgresConfig,
    pub egress: EgressConfig,
    pub disk: DiskConfig,
    pub baseline: BaselineConfig,
    /// Prepended to `PATH` for `ProcessRole::Scip`.
    pub scip_bin: PathBuf,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AgentConfig {
    /// Enforced only.
    pub user: String,
    pub path: Vec<PathBuf>,
    /// Names only; reserved names are refused by [`ExecHostConfig::validate`].
    pub env_passthrough: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DevConfig {
    pub harness_real_home: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PortsConfig {
    pub range: [u16; 2],
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ExecConfig {
    pub default_timeout_s: u32,
    pub max_timeout_s: u32,
    pub kill_grace_s: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PrepareConfig {
    /// Sum of build step timeouts.
    pub max_total_s: u32,
    pub max_bundle_unpacked_gib: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ForkConfig {
    pub allow_copy_fallback: bool,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CgroupConfig {
    pub memory_max: String,
    pub pids_max: u32,
    pub cpu_weight: u32,
    /// Delegated runner cgroup; default: the daemon's own cgroup from `/proc/self/cgroup`.
    pub root: Option<PathBuf>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PostgresConfig {
    pub url: Option<String>,
    /// From `MAGE_RUNNER_POSTGRES_PASSWORD`, set by the config loader.
    pub password: Option<String>,
    pub strategy: PgStrategy,
}

impl std::fmt::Debug for PostgresConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PostgresConfig")
            .field("url", &self.url.as_deref().map(redact_url))
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("strategy", &self.strategy)
            .finish()
    }
}

fn redact_url(raw: &str) -> String {
    match url::Url::parse(raw) {
        Ok(mut u) => {
            if u.password().is_some() {
                let _ = u.set_password(Some("redacted"));
            }
            u.to_string()
        }
        Err(_) => "<invalid url>".to_owned(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EgressMode {
    Proxy,
    Off,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct EgressConfig {
    pub mode: EgressMode,
    pub registries: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct DiskConfig {
    pub min_free_gib: u64,
    pub bases_max: u32,
    pub bases_max_gib: u64,
    pub base_idle_days: u32,
    pub fork_max_gib: u64,
    /// Disk estimate for a repo's first base (later bases use the last base size).
    pub first_base_estimate_gib: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct BaselineConfig {
    /// Build output dirs kept out of baselines (`info/exclude`).
    pub excludes: Vec<String>,
}

impl Default for ExecHostConfig {
    fn default() -> Self {
        Self {
            environment: RunnerEnvironment::Production,
            isolation: IsolationMode::Enforced,
            data_dir: PathBuf::from("/var/lib/mage"),
            max_envs: 4,
            max_sessions_per_repo: 1,
            server_url: None,
            agent: AgentConfig::default(),
            dev: DevConfig::default(),
            toolchains: BTreeMap::new(),
            ports: PortsConfig::default(),
            exec: ExecConfig::default(),
            prepare: PrepareConfig::default(),
            fork: ForkConfig::default(),
            cgroup: CgroupConfig::default(),
            postgres: PostgresConfig::default(),
            egress: EgressConfig::default(),
            disk: DiskConfig::default(),
            baseline: BaselineConfig::default(),
            scip_bin: PathBuf::from("/opt/mage/scip/bin"),
        }
    }
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            user: "mage-agent".into(),
            path: ["/opt/mage/bin", "/usr/local/bin", "/usr/bin", "/bin"].iter().map(PathBuf::from).collect(),
            env_passthrough: Vec::new(),
        }
    }
}

impl Default for PortsConfig {
    fn default() -> Self {
        Self { range: [20000, 29999] }
    }
}

impl Default for ExecConfig {
    fn default() -> Self {
        Self { default_timeout_s: 120, max_timeout_s: 900, kill_grace_s: 5 }
    }
}

impl Default for PrepareConfig {
    fn default() -> Self {
        Self { max_total_s: 5400, max_bundle_unpacked_gib: 8 }
    }
}

impl Default for CgroupConfig {
    fn default() -> Self {
        Self { memory_max: "8G".into(), pids_max: 4096, cpu_weight: 100, root: None }
    }
}

impl Default for PostgresConfig {
    fn default() -> Self {
        Self { url: Some("postgres://mage_runner@127.0.0.1:5432/postgres".into()), password: None, strategy: PgStrategy::Auto }
    }
}

impl Default for EgressConfig {
    fn default() -> Self {
        Self {
            mode: EgressMode::Proxy,
            registries: [
                "registry.npmjs.org",
                "pypi.org",
                "files.pythonhosted.org",
                "proxy.golang.org",
                "sum.golang.org",
                "index.crates.io",
                "static.crates.io",
                "repo.maven.apache.org",
                "rubygems.org",
            ]
            .iter()
            .map(|s| (*s).to_owned())
            .collect(),
        }
    }
}

impl Default for DiskConfig {
    fn default() -> Self {
        Self { min_free_gib: 20, bases_max: 20, bases_max_gib: 200, base_idle_days: 14, fork_max_gib: 20, first_base_estimate_gib: 5 }
    }
}

impl Default for BaselineConfig {
    fn default() -> Self {
        Self {
            excludes: ["node_modules/", "target/", "dist/", "build/", ".venv/", "venv/", "__pycache__/", ".next/", ".turbo/", ".gradle/"]
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
        }
    }
}

impl ExecHostConfig {
    /// Dev profile (executor.md: dev differs where noted): `environment = dev`, `isolation = dev`,
    /// egress proxy off, copy fallback allowed, no runner Postgres unless set, small disk floor.
    pub fn dev(data_dir: PathBuf) -> Self {
        Self {
            environment: RunnerEnvironment::Dev,
            isolation: IsolationMode::Dev,
            data_dir,
            fork: ForkConfig { allow_copy_fallback: true },
            egress: EgressConfig { mode: EgressMode::Off, ..EgressConfig::default() },
            postgres: PostgresConfig { url: None, ..PostgresConfig::default() },
            disk: DiskConfig { min_free_gib: 1, ..DiskConfig::default() },
            ..Self::default()
        }
    }

    /// Load-time checks (the daemon refuses to start on any error).
    pub fn validate(&self) -> Result<(), ExecError> {
        if self.environment == RunnerEnvironment::Production && self.isolation == IsolationMode::Dev {
            return Err(ExecError::Invalid("isolation = \"dev\" is refused when environment = \"production\"".into()));
        }
        if self.isolation == IsolationMode::Enforced && !cfg!(target_os = "linux") {
            return Err(ExecError::Invalid("isolation = \"enforced\" needs Linux".into()));
        }
        for name in &self.agent.env_passthrough {
            if is_reserved_env_name(name) || name.starts_with("ANTHROPIC_") || name.starts_with("CLAUDE_") || name.starts_with("AWS_") {
                return Err(ExecError::Invalid(format!("runner.agent.env_passthrough: {name} is reserved")));
            }
        }
        let [start, end] = self.ports.range;
        if start == 0 || start > end {
            return Err(ExecError::Invalid(format!("runner.ports.range [{start}, {end}] is empty")));
        }
        if self.exec.max_timeout_s == 0 || self.exec.default_timeout_s > self.exec.max_timeout_s {
            return Err(ExecError::Invalid("runner.exec: default_timeout_s must be ≤ max_timeout_s (> 0)".into()));
        }
        if self.max_envs == 0 {
            return Err(ExecError::Invalid("runner.max_envs must be > 0".into()));
        }
        if !self.data_dir.is_absolute() {
            return Err(ExecError::Invalid("runner.data_dir must be absolute".into()));
        }
        Ok(())
    }

    /// Host of `server_url`, lowercase.
    pub fn server_host(&self) -> Option<String> {
        self.server_url.as_ref().and_then(|u| u.host_str()).map(str::to_ascii_lowercase)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn production_refuses_dev_isolation_and_reserved_passthrough() {
        let mut cfg = ExecHostConfig { isolation: IsolationMode::Dev, ..ExecHostConfig::default() };
        assert!(cfg.validate().is_err());
        cfg = ExecHostConfig::dev(PathBuf::from("/tmp/x"));
        assert!(cfg.validate().is_ok());
        cfg.agent.env_passthrough = vec!["HTTPS_PROXY".into()];
        assert!(cfg.validate().is_err());
        cfg.agent.env_passthrough = vec!["MAGE_TOKEN".into()];
        assert!(cfg.validate().is_err());
        cfg.agent.env_passthrough = vec!["NPM_CONFIG_CACHE".into()];
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn parses_doc_keys() {
        let cfg: ExecHostConfig = toml::from_str(
            r#"
            environment = "dev"
            isolation = "dev"
            data_dir = "/Users/x/.mage/runner"
            [agent]
            env_passthrough = ["NPM_TOKEN_FAKE"]
            [toolchains]
            node = { "20" = "/opt/mage/toolchains/node-20/bin" }
            [ports]
            range = [21000, 21999]
            [postgres]
            url = "postgres://mage_runner:pw@127.0.0.1:5432/postgres"
            strategy = "file_copy"
            [egress]
            mode = "off"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.ports.range, [21000, 21999]);
        assert_eq!(cfg.postgres.strategy, PgStrategy::FileCopy);
        assert!(!format!("{:?}", cfg.postgres).contains("pw@"));
        assert_eq!(cfg.toolchains["node"]["20"], PathBuf::from("/opt/mage/toolchains/node-20/bin"));
        assert_eq!(cfg.exec.max_timeout_s, 900);
    }
}
