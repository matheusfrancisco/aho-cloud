//! `[runner]` config section (executor.md "Runner config (executor keys)", harness.md
//! "Configuration", runs.md "Startup and recovery", server.md "Per-role credentials").
//! `mage-app` deserializes it from the shared TOML file (with `MAGE_RUNNER__…` overrides applied)
//! and hands it to [`crate::compose::run`].
//!
//! Layout: the executor keys sit directly in `[runner]` ([`ExecHostConfig`]); every other area
//! is a sub-table:
//!
//! | Table | Type | Notes |
//! |---|---|---|
//! | `[runner]` | [`ExecHostConfig`] | `environment`, `isolation`, `data_dir` (`~/` expands), `server_url`, `max_sessions_per_repo`, `[runner.agent]`, `[runner.ports]`, `[runner.postgres]`, … |
//! | `[runner.link]` | [`LinkConfig`] | `name`; `server_url` defaults to `runner.server_url`; `credential_file` defaults to `{data_dir}/credentials.json`; `bootstrap_token` is a secret reference |
//! | `[runner.task]` | [`TaskConfig`] | `data_dir` comes from `runner.data_dir` |
//! | `[runner.daemon]` | [`DaemonConfig`] | `max_sessions_per_repo` comes from `runner.max_sessions_per_repo` |
//! | `[runner.harness]`, `.omp`, `.claude` | [`HarnessConfig`] | `log_dir` defaults to `{data_dir}/harness-logs` |
//! | `[runner.models."<id>"]` | [`ModelTable`] | |
//! | `[runner.playwright]` | [`PlaywrightConfig`] | |
//! | `[runner.scip]` | [`ScipConfig`] | |
//! | `[runner.mcp]` | [`RunToolsConfig`] | `script_timeout_s` comes from `runner.verify.script_timeout_s` |
//! | `[runner.verify]`, `.media` | [`VerifyConfig`] | |
//! | `[runner.telemetry]` | — | read by `mage-app`, which installs the process-wide subscriber |
//! | `state_file` | path | SQLite `RunnerState`; default `{data_dir}/state.db` |
//!
//! `environment = "dev"` switches the defaults of absent keys to the dev profile
//! ([`ExecHostConfig::dev`]: copy fork fallback, egress proxy off, no runner Postgres, 1 GiB disk
//! floor; Playwright from `npx` and `node` on `PATH`). `isolation = "dev"` is never defaulted: it
//! must be set explicitly and is refused with `environment = "production"`; it is also what allows
//! the developer's own harness login (`LocalLogin`).
//!
//! The runner holds no server credential: the only secrets here are the one-time bootstrap token
//! and the runner-local Postgres password, both `env:`/`file:` references resolved at start.

use std::fmt;
use std::path::PathBuf;

use serde::de::Error as _;
use serde::{Deserialize, Deserializer};
use serde_json::{Map, Value};
use url::Url;

use crate::adapters::driven::exec_host::{ExecHostConfig, RunnerEnvironment};
use crate::adapters::driven::harness_common::config::expand_home;
use crate::adapters::driven::harness_common::{HarnessConfig, ModelTable};
use crate::adapters::driven::link::LinkConfig;
use crate::adapters::driven::playwright::PlaywrightConfig;
use crate::adapters::driving::mcp::RunMcpConfig;
use crate::app::runner::daemon::DaemonConfig;
use crate::app::runner::scip::ScipConfig;
use crate::app::runner::task::TaskConfig;
use crate::app::runner::tools::RunToolsConfig;
use crate::app::runner::verify::VerifyConfig;

/// Fallback source of `runner.postgres.password` when the file names none (executor.md).
pub const POSTGRES_PASSWORD_ENV: &str = "MAGE_RUNNER_POSTGRES_PASSWORD";

/// The `[runner]` section, split into the per-area configs.
#[derive(Debug, Clone)]
pub struct Config {
    /// Executor keys; `postgres.password` is filled at start from [`Config::postgres_password`].
    pub executor: ExecHostConfig,
    /// `bootstrap_token` is filled at start from [`Config::bootstrap_token`].
    pub link: LinkConfig,
    /// One-time `mgb_` token (`runner.link.bootstrap_token`), used until the credential file
    /// holds a runner id.
    pub bootstrap_token: Option<SecretRef>,
    /// `runner.postgres.password`; `None` = [`POSTGRES_PASSWORD_ENV`] when set.
    pub postgres_password: Option<SecretRef>,
    pub task: TaskConfig,
    pub daemon: DaemonConfig,
    pub harness: HarnessConfig,
    pub models: ModelTable,
    pub playwright: PlaywrightConfig,
    pub scip: ScipConfig,
    pub tools: RunToolsConfig,
    pub verify: VerifyConfig,
    /// Run MCP listeners share `runner.ports.range` with env ports.
    pub run_mcp: RunMcpConfig,
    /// SQLite `RunnerState`.
    pub state_file: PathBuf,
}

/// A credential reference: `env:NAME` or `file:/path` (`~/` expands). Inline values and cloud
/// secret managers are refused: runner hosts have no cloud identity.
#[derive(Clone, PartialEq, Eq)]
pub enum SecretRef {
    Env(String),
    File(PathBuf),
}

impl SecretRef {
    pub fn parse(s: &str) -> Result<Self, String> {
        if let Some(name) = s.strip_prefix("env:") {
            if name.is_empty() || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
                return Err("env: needs a variable name".into());
            }
            return Ok(SecretRef::Env(name.to_owned()));
        }
        if let Some(path) = s.strip_prefix("file:") {
            if path.is_empty() {
                return Err("file: needs a path".into());
            }
            return Ok(SecretRef::File(expand_home(path)));
        }
        Err("must be an env:NAME or file:/path reference (never an inline value; the runner has no cloud secret manager)".into())
    }

    /// Reads the value (a trailing newline is dropped); empty is an error.
    pub fn resolve(&self) -> Result<String, String> {
        let value = match self {
            SecretRef::Env(name) => std::env::var(name).map_err(|e| format!("{self}: {e}"))?,
            SecretRef::File(path) => std::fs::read_to_string(path).map_err(|e| format!("{self}: {e}"))?,
        };
        let value = value.strip_suffix('\n').map_or(value.as_str(), |v| v.strip_suffix('\r').unwrap_or(v)).to_owned();
        if value.is_empty() { Err(format!("{self} is empty")) } else { Ok(value) }
    }
}

impl fmt::Display for SecretRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SecretRef::Env(name) => write!(f, "env:{name}"),
            SecretRef::File(path) => write!(f, "file:{}", path.display()),
        }
    }
}

impl fmt::Debug for SecretRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

impl<'de> Deserialize<'de> for Config {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Config::from_table(Map::deserialize(d)?).map_err(D::Error::custom)
    }
}

fn table(raw: &mut Map<String, Value>, key: &str) -> Result<Map<String, Value>, String> {
    match raw.remove(key) {
        None => Ok(Map::new()),
        Some(Value::Object(map)) => Ok(map),
        Some(_) => Err(format!("runner.{key} must be a table")),
    }
}

fn has(map: &Map<String, Value>, path: &[&str]) -> bool {
    let Some((first, rest)) = path.split_first() else { return false };
    match (map.get(*first), rest.is_empty()) {
        (Some(_), true) => true,
        (Some(Value::Object(inner)), false) => has(inner, rest),
        _ => false,
    }
}

fn parse<T: serde::de::DeserializeOwned>(map: Map<String, Value>, path: &str) -> Result<T, String> {
    serde_json::from_value(Value::Object(map)).map_err(|e| format!("{path}: {e}"))
}

fn refuse(map: &Map<String, Value>, section: &str, key: &str, instead: &str) -> Result<(), String> {
    if map.contains_key(key) { Err(format!("runner.{section}.{key} is not set here; set {instead}")) } else { Ok(()) }
}

fn path_string(path: &std::path::Path) -> Value {
    Value::String(path.to_string_lossy().into_owned())
}

/// `~/…` in a string-valued key.
fn expand_key(map: &mut Map<String, Value>, key: &str) {
    if let Some(Value::String(s)) = map.get(key) {
        let expanded = path_string(&expand_home(s));
        map.insert(key.to_owned(), expanded);
    }
}

fn secret_ref(value: Option<Value>, path: &str) -> Result<Option<SecretRef>, String> {
    match value {
        None => Ok(None),
        Some(Value::String(s)) => SecretRef::parse(&s).map(Some).map_err(|e| format!("{path}: {e}")),
        Some(_) => Err(format!("{path} must be a secret reference string")),
    }
}

impl Config {
    fn from_table(mut raw: Map<String, Value>) -> Result<Self, String> {
        raw.remove("telemetry");
        let mut link = table(&mut raw, "link")?;
        let mut task = table(&mut raw, "task")?;
        let mut daemon = table(&mut raw, "daemon")?;
        let mut harness = table(&mut raw, "harness")?;
        let models = table(&mut raw, "models")?;
        let mut playwright = table(&mut raw, "playwright")?;
        let scip = table(&mut raw, "scip")?;
        let mcp = table(&mut raw, "mcp")?;
        let verify = table(&mut raw, "verify")?;
        let state_file = raw.remove("state_file");

        // Executor keys (what is left of `[runner]`).
        expand_key(&mut raw, "data_dir");
        let postgres_password =
            secret_ref(raw.get_mut("postgres").and_then(Value::as_object_mut).and_then(|pg| pg.remove("password")), "runner.postgres.password")?;
        let set_fork = has(&raw, &["fork", "allow_copy_fallback"]);
        let set_egress = has(&raw, &["egress", "mode"]);
        let set_pg_url = has(&raw, &["postgres", "url"]);
        let set_disk_floor = has(&raw, &["disk", "min_free_gib"]);
        let mut executor: ExecHostConfig = parse(raw, "runner")?;
        let dev = executor.environment == RunnerEnvironment::Dev;
        if dev {
            let profile = ExecHostConfig::dev(executor.data_dir.clone());
            if !set_fork {
                executor.fork = profile.fork;
            }
            if !set_egress {
                executor.egress.mode = profile.egress.mode;
            }
            if !set_pg_url {
                executor.postgres.url = profile.postgres.url;
            }
            if !set_disk_floor {
                executor.disk.min_free_gib = profile.disk.min_free_gib;
            }
        }
        let data_dir = executor.data_dir.clone();

        // `[runner.link]`: one server URL for the executor's egress allowlist and the link.
        let link_url = match link.get("server_url") {
            None => None,
            Some(Value::String(s)) => Some(Url::parse(s).map_err(|e| format!("runner.link.server_url: {e}"))?),
            Some(_) => return Err("runner.link.server_url must be a string".into()),
        };
        match (&executor.server_url, link_url) {
            (Some(url), None) => {
                link.insert("server_url".into(), Value::String(url.as_str().trim_end_matches('/').to_owned()));
            }
            (None, Some(url)) => executor.server_url = Some(url),
            (Some(a), Some(b)) if *a != b => return Err(format!("runner.server_url ({a}) and runner.link.server_url ({b}) differ")),
            (Some(_), Some(_)) => {}
            (None, None) => return Err("runner.server_url is required (the server's public_url)".into()),
        }
        expand_key(&mut link, "credential_file");
        link.entry("credential_file").or_insert_with(|| path_string(&data_dir.join("credentials.json")));
        let bootstrap_token = secret_ref(link.remove("bootstrap_token"), "runner.link.bootstrap_token")?;
        let link: LinkConfig = parse(link, "runner.link")?;

        refuse(&task, "task", "data_dir", "runner.data_dir")?;
        task.insert("data_dir".into(), path_string(&data_dir));
        let task: TaskConfig = parse(task, "runner.task")?;

        refuse(&daemon, "daemon", "max_sessions_per_repo", "runner.max_sessions_per_repo")?;
        daemon.insert("max_sessions_per_repo".into(), Value::from(executor.max_sessions_per_repo));
        let daemon: DaemonConfig = parse(daemon, "runner.daemon")?;

        expand_key(&mut harness, "log_dir");
        harness.entry("log_dir").or_insert_with(|| path_string(&data_dir.join("harness-logs")));
        let harness: HarnessConfig = parse(harness, "runner.harness")?;
        let models: ModelTable = parse(models, "runner.models")?;

        expand_key(&mut playwright, "root");
        expand_key(&mut playwright, "node");
        let dev_root = dev && !playwright.contains_key("root");
        let dev_node = dev && !playwright.contains_key("node");
        let mut playwright: PlaywrightConfig = parse(playwright, "runner.playwright")?;
        if dev_root {
            playwright.root = None;
        }
        if dev_node {
            playwright.node = None;
        }

        let scip: ScipConfig = parse(scip, "runner.scip")?;
        let verify: VerifyConfig = parse(verify, "runner.verify")?;
        refuse(&mcp, "mcp", "script_timeout_s", "runner.verify.script_timeout_s")?;
        let mut tools: RunToolsConfig = parse(mcp, "runner.mcp")?;
        tools.script_timeout_s = verify.script_timeout_s;

        let state_file = match state_file {
            None => executor.state_db_path(),
            Some(Value::String(s)) => expand_home(&s),
            Some(_) => return Err("runner.state_file must be a path".into()),
        };
        let run_mcp = RunMcpConfig { port_range: executor.ports.range };
        Ok(Config {
            executor,
            link,
            bootstrap_token,
            postgres_password,
            task,
            daemon,
            harness,
            models,
            playwright,
            scip,
            tools,
            verify,
            run_mcp,
            state_file,
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::adapters::driven::exec_host::EgressMode;

    fn parse_runner(text: &str) -> Result<Config, String> {
        toml::from_str::<Config>(text).map_err(|e| e.to_string())
    }

    const DEV: &str = r#"
        environment = "dev"
        isolation = "dev"
        data_dir = "/srv/mage-runner"
        server_url = "http://127.0.0.1:8080"
        [link]
        name = "laptop"
        bootstrap_token = "file:/srv/mage-runner/bootstrap"
    "#;

    #[test]
    fn dev_environment_fills_absent_keys_from_the_dev_profile_but_keeps_explicit_ones() {
        let config = parse_runner(DEV).unwrap();
        assert_eq!(config.executor.egress.mode, EgressMode::Off);
        assert!(config.executor.fork.allow_copy_fallback);
        assert_eq!(config.executor.postgres.url, None);
        assert_eq!(config.playwright.root, None);
        let explicit = format!("{DEV}\n[playwright]\nroot = \"/opt/pw\"\n");
        let explicit = explicit.replace("server_url = \"http://127.0.0.1:8080\"", "server_url = \"http://127.0.0.1:8080\"\n[egress]\nmode = \"proxy\"");
        let config = parse_runner(&explicit).unwrap();
        assert_eq!(config.executor.egress.mode, EgressMode::Proxy);
        assert_eq!(config.playwright.root, Some(PathBuf::from("/opt/pw")));
        let prod = parse_runner(&DEV.replace("environment = \"dev\"\n        isolation = \"dev\"", "")).unwrap();
        assert_eq!(prod.executor.egress.mode, EgressMode::Proxy);
        assert!(prod.executor.postgres.url.is_some());
    }

    #[test]
    fn shared_keys_have_one_source() {
        let config = parse_runner(DEV).unwrap();
        assert_eq!(config.link.server_url, "http://127.0.0.1:8080");
        assert_eq!(config.task.data_dir, PathBuf::from("/srv/mage-runner"));
        assert_eq!(config.link.credential_file, PathBuf::from("/srv/mage-runner/credentials.json"));
        assert_eq!(config.state_file, PathBuf::from("/srv/mage-runner/state.db"));
        let differing = DEV.replace("name = \"laptop\"", "name = \"laptop\"\nserver_url = \"http://other:8080\"");
        assert!(parse_runner(&differing).unwrap_err().contains("differ"));
        let task_dir = format!("{DEV}\n[task]\ndata_dir = \"/elsewhere\"\n");
        assert!(parse_runner(&task_dir).unwrap_err().contains("runner.data_dir"));
        let no_url = DEV.replace("server_url = \"http://127.0.0.1:8080\"", "");
        assert!(parse_runner(&no_url).unwrap_err().contains("server_url is required"));
    }

    #[test]
    fn credentials_must_be_env_or_file_references() {
        let config = parse_runner(DEV).unwrap();
        assert_eq!(config.bootstrap_token, Some(SecretRef::File(PathBuf::from("/srv/mage-runner/bootstrap"))));
        assert_eq!(config.link.bootstrap_token, None);
        for bad in ["mgb_inline-token-value", "gcpsm:projects/p/secrets/s/versions/1", "env:"] {
            let text = DEV.replace("file:/srv/mage-runner/bootstrap", bad);
            assert!(parse_runner(&text).unwrap_err().contains("bootstrap_token"), "{bad} accepted");
        }
        let pg = format!("{DEV}\n[postgres]\nurl = \"postgres://r@127.0.0.1/postgres\"\npassword = \"hunter2\"\n");
        assert!(parse_runner(&pg).unwrap_err().contains("runner.postgres.password"));
        let pg = pg.replace("hunter2", "env:PG_PW");
        assert_eq!(parse_runner(&pg).unwrap().postgres_password, Some(SecretRef::Env("PG_PW".into())));
    }

    #[test]
    fn server_sections_are_unknown_keys() {
        let text = format!("{DEV}\n[database]\nurl_source = \"env:MAGE_DATABASE_URL\"\n");
        assert!(parse_runner(&text).is_err());
    }
}
