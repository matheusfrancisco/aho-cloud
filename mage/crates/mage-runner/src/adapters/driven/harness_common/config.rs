//! `[runner.harness]` and `[runner.models."<id>"]` config sections (harness.md "Configuration").

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;

/// `[runner.harness]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HarnessConfig {
    /// Runner-local; not visible inside any Env. Holds `<run_id>.stderr.log` and
    /// `<run_id>.transcript.jsonl`.
    #[serde(default = "default_log_dir")]
    pub log_dir: PathBuf,
    /// ≥ max `McpServerSpec.tool_timeout` + 5 min.
    #[serde(default = "default_stall")]
    pub stall_timeout_secs: u64,
    #[serde(default = "default_abort_deadline")]
    pub abort_deadline_secs: u64,
    #[serde(default = "default_close_grace")]
    pub close_grace_secs: u64,
    #[serde(default)]
    pub allow_direct_vertex: bool,
    #[serde(default)]
    pub omp: OmpConfig,
    #[serde(default)]
    pub claude: ClaudeConfig,
}

impl Default for HarnessConfig {
    fn default() -> Self {
        Self {
            log_dir: default_log_dir(),
            stall_timeout_secs: default_stall(),
            abort_deadline_secs: default_abort_deadline(),
            close_grace_secs: default_close_grace(),
            allow_direct_vertex: false,
            omp: OmpConfig::default(),
            claude: ClaudeConfig::default(),
        }
    }
}

impl HarnessConfig {
    pub fn stall_timeout(&self) -> Duration {
        Duration::from_secs(self.stall_timeout_secs)
    }

    pub fn abort_deadline(&self) -> Duration {
        Duration::from_secs(self.abort_deadline_secs)
    }

    pub fn close_grace(&self) -> Duration {
        Duration::from_secs(self.close_grace_secs)
    }
}

/// `[runner.harness.omp]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OmpConfig {
    /// Must be visible inside the Env mount namespace.
    #[serde(default = "default_omp_binary")]
    pub binary: String,
    #[serde(default = "default_omp_supported")]
    pub supported: VersionReq,
    /// LocalLogin only; `~` expands to the runner's `HOME`.
    #[serde(default = "default_omp_dev_agent_dir")]
    pub dev_agent_dir: String,
}

impl Default for OmpConfig {
    fn default() -> Self {
        Self { binary: default_omp_binary(), supported: default_omp_supported(), dev_agent_dir: default_omp_dev_agent_dir() }
    }
}

/// `[runner.harness.claude]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaudeConfig {
    #[serde(default = "default_claude_binary")]
    pub binary: String,
    #[serde(default = "default_claude_supported")]
    pub supported: VersionReq,
    /// LocalLogin only; `~` expands to the runner's `HOME`.
    #[serde(default = "default_claude_dev_config_dir")]
    pub dev_config_dir: String,
}

impl Default for ClaudeConfig {
    fn default() -> Self {
        Self { binary: default_claude_binary(), supported: default_claude_supported(), dev_config_dir: default_claude_dev_config_dir() }
    }
}

/// `[runner.models."<id>"]`: metadata omp needs for a custom provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelMeta {
    pub context_window: u64,
    pub max_tokens: u64,
    #[serde(default)]
    pub reasoning: bool,
}

impl Default for ModelMeta {
    /// Used when `runner.models` has no entry for the requested model.
    fn default() -> Self {
        Self { context_window: 200_000, max_tokens: 64_000, reasoning: true }
    }
}

/// `[runner.models]`.
pub type ModelTable = BTreeMap<String, ModelMeta>;

/// `~` / `~/…` → `$HOME/…`.
pub fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => {
            let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
            home.join(rest.trim_start_matches('/'))
        }
        _ => PathBuf::from(path),
    }
}

/// Comma-separated comparators over dotted numeric versions, e.g. `">=18.3, <19"`.
/// Missing components are zero (`18.3` = `18.3.0`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VersionReq {
    raw: String,
    clauses: Vec<(Cmp, [u64; 3])>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cmp {
    Ge,
    Gt,
    Le,
    Lt,
    Eq,
}

impl VersionReq {
    pub fn parse(raw: &str) -> Result<Self, String> {
        let mut clauses = Vec::new();
        for clause in raw.split(',').map(str::trim).filter(|c| !c.is_empty()) {
            let (cmp, rest) = if let Some(r) = clause.strip_prefix(">=") {
                (Cmp::Ge, r)
            } else if let Some(r) = clause.strip_prefix("<=") {
                (Cmp::Le, r)
            } else if let Some(r) = clause.strip_prefix('>') {
                (Cmp::Gt, r)
            } else if let Some(r) = clause.strip_prefix('<') {
                (Cmp::Lt, r)
            } else if let Some(r) = clause.strip_prefix('=') {
                (Cmp::Eq, r)
            } else {
                (Cmp::Eq, clause)
            };
            let version = parse_version(rest.trim()).ok_or_else(|| format!("bad version {rest:?} in {raw:?}"))?;
            clauses.push((cmp, version));
        }
        if clauses.is_empty() {
            return Err(format!("empty version requirement {raw:?}"));
        }
        Ok(Self { raw: raw.to_owned(), clauses })
    }

    pub fn matches(&self, version: &str) -> bool {
        let Some(v) = parse_version(version) else { return false };
        self.clauses.iter().all(|(cmp, want)| match cmp {
            Cmp::Ge => v >= *want,
            Cmp::Gt => v > *want,
            Cmp::Le => v <= *want,
            Cmp::Lt => v < *want,
            Cmp::Eq => v == *want,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.raw
    }
}

impl<'de> Deserialize<'de> for VersionReq {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(d)?;
        Self::parse(&raw).map_err(serde::de::Error::custom)
    }
}

/// `"18.3.1"` → `[18, 3, 1]`; 1 to 3 numeric components.
fn parse_version(s: &str) -> Option<[u64; 3]> {
    let mut out = [0u64; 3];
    let mut n = 0;
    for part in s.split('.') {
        if n == 3 || part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        out[n] = part.parse().ok()?;
        n += 1;
    }
    (n > 0).then_some(out)
}

/// First `\d+(\.\d+){0,2}` in a `--version` output (`"omp/18.3.1"`, `"2.1.269 (Claude Code)"`).
pub fn extract_version(output: &str) -> Option<String> {
    let bytes = output.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let start = i;
            let mut dots = 0;
            while i < bytes.len() && (bytes[i].is_ascii_digit() || (bytes[i] == b'.' && dots < 2 && i + 1 < bytes.len() && bytes[i + 1].is_ascii_digit())) {
                if bytes[i] == b'.' {
                    dots += 1;
                }
                i += 1;
            }
            if dots > 0 {
                return Some(output[start..i].to_owned());
            }
        }
        i += 1;
    }
    None
}

fn default_log_dir() -> PathBuf {
    PathBuf::from("/var/lib/mage/runner/harness-logs")
}
fn default_stall() -> u64 {
    2100
}
fn default_abort_deadline() -> u64 {
    15
}
fn default_close_grace() -> u64 {
    10
}
fn default_omp_binary() -> String {
    "/opt/mage/bin/omp".into()
}
fn default_omp_supported() -> VersionReq {
    VersionReq::parse(">=18.3, <19").unwrap_or_else(|_| unreachable!("constant requirement parses"))
}
fn default_omp_dev_agent_dir() -> String {
    "~/.omp-mage/agent".into()
}
fn default_claude_binary() -> String {
    "/opt/mage/bin/claude".into()
}
fn default_claude_supported() -> VersionReq {
    VersionReq::parse(">=2.1.269, <2.2").unwrap_or_else(|_| unreachable!("constant requirement parses"))
}
fn default_claude_dev_config_dir() -> String {
    "~/.claude-mage".into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_requirements_bound_both_ends() {
        let omp = default_omp_supported();
        assert!(omp.matches("18.3.1"));
        assert!(omp.matches("18.3"));
        assert!(omp.matches("18.9.0"));
        assert!(!omp.matches("18.2.9"));
        assert!(!omp.matches("19.0.0"));
        let claude = default_claude_supported();
        assert!(claude.matches("2.1.269"));
        assert!(claude.matches("2.1.300"));
        assert!(!claude.matches("2.1.268"));
        assert!(!claude.matches("2.2.0"));
        assert!(!claude.matches("garbage"));
        assert!(VersionReq::parse("").is_err());
        assert!(VersionReq::parse(">=1.x").is_err());
    }

    #[test]
    fn version_extraction_from_cli_output() {
        assert_eq!(extract_version("omp/18.3.1\n").as_deref(), Some("18.3.1"));
        assert_eq!(extract_version("2.1.269 (Claude Code)").as_deref(), Some("2.1.269"));
        assert_eq!(extract_version("v3 build 7"), None);
    }

    #[test]
    fn config_section_defaults_and_unknown_keys() {
        let cfg: HarnessConfig = toml::from_str("stall_timeout_secs = 60\n[omp]\nbinary = \"/x/omp\"\n").unwrap();
        assert_eq!(cfg.stall_timeout(), Duration::from_secs(60));
        assert_eq!(cfg.omp.binary, "/x/omp");
        assert_eq!(cfg.claude, ClaudeConfig::default());
        assert_eq!(cfg.abort_deadline(), Duration::from_secs(15));
        assert!(toml::from_str::<HarnessConfig>("bogus = 1").is_err());
        assert!(toml::from_str::<HarnessConfig>("[omp]\nsupported = \">=x\"").is_err());
        let models: ModelTable = toml::from_str("[\"claude-sonnet-4-5\"]\ncontext_window = 200000\nmax_tokens = 64000\nreasoning = true\n").unwrap();
        assert_eq!(models["claude-sonnet-4-5"], ModelMeta { context_window: 200_000, max_tokens: 64_000, reasoning: true });
    }
}
