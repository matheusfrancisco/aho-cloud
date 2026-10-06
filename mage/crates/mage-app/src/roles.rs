//! Config loading and role section routing (README "Config", server.md "Config"): one TOML file
//! plus `MAGE_<SECTION>__<KEY>` overrides; no inline secret anywhere in it; `mage start <role>`
//! deserializes its own section and refuses a file that also holds another role's section
//! (each role then holds only its own credentials); `mage start all` loads all three and is
//! refused in production.

use std::path::Path;

use anyhow::{Context, Result, bail};
use mage_infra::config::ConfigFile;
use mage_infra::telemetry::TelemetryConfig;
use mage_runner::adapters::driven::exec_host::RunnerEnvironment;

use crate::cli::StartTarget;

/// Top-level role sections.
pub const ROLE_SECTIONS: [&str; 3] = ["server", "indexer", "runner"];

/// Reads `path`, applies the `MAGE_*__*` overrides among `vars`, and refuses any credential-named
/// field that is not a secret reference.
pub fn load(path: &Path, vars: impl IntoIterator<Item = (String, String)>) -> Result<ConfigFile> {
    let file = ConfigFile::load(path)?.with_overrides(vars)?;
    file.check_no_inline_secrets()?;
    Ok(file)
}

/// The process environment as UTF-8 pairs (non-UTF-8 variables are not overrides).
pub fn env_vars() -> impl Iterator<Item = (String, String)> {
    std::env::vars_os().filter_map(|(name, value)| Some((name.into_string().ok()?, value.into_string().ok()?)))
}

/// The role configs `mage start` runs.
pub enum Roles {
    Server(Box<mage_server::Config>),
    Indexer(Box<mage_indexer::Config>),
    Runner(Box<mage_runner::Config>),
    All { server: Box<mage_server::Config>, indexer: Box<mage_indexer::Config>, runner: Box<mage_runner::Config> },
}

/// Deserializes the sections `target` needs and enforces the routing rules.
pub fn route(file: &ConfigFile, target: StartTarget) -> Result<Roles> {
    match target {
        StartTarget::Server => Ok(Roles::Server(Box::new(own_section(file, "server")?))),
        StartTarget::Indexer => Ok(Roles::Indexer(Box::new(own_section(file, "indexer")?))),
        StartTarget::Runner => Ok(Roles::Runner(Box::new(own_section(file, "runner")?))),
        StartTarget::All => all(file),
    }
}

/// Section `own`, refused when another role's section is present.
fn own_section<T: serde::de::DeserializeOwned>(file: &ConfigFile, own: &str) -> Result<T> {
    for other in ROLE_SECTIONS.into_iter().filter(|section| *section != own) {
        if file.has(other) {
            bail!(
                "{} has a [{other}] section: `mage start {own}` refuses another role's config (one file per role; \
                 `mage start all` is the development mode)",
                file.origin()
            );
        }
    }
    Ok(file.section(own)?)
}

fn all(file: &ConfigFile) -> Result<Roles> {
    let server: mage_server::Config = file.section("server")?;
    if server.is_production() {
        bail!("`mage start all` is for local development only: [server] environment is production");
    }
    let runner: mage_runner::Config = file.section("runner")?;
    if runner.executor.environment == RunnerEnvironment::Production {
        bail!("`mage start all` is for local development only: [runner] environment is production (set environment = \"dev\")");
    }
    let indexer: mage_indexer::Config = file.section("indexer")?;
    Ok(Roles::All { server: Box::new(server), indexer: Box::new(indexer), runner: Box::new(runner) })
}

/// The subscriber settings of the process: the role's `[<role>.telemetry]`; `all` uses the
/// server's.
pub fn telemetry(file: &ConfigFile, roles: &Roles) -> Result<TelemetryConfig> {
    Ok(match roles {
        Roles::Server(server) | Roles::All { server, .. } => server.telemetry.clone(),
        Roles::Indexer(indexer) => indexer.telemetry(),
        Roles::Runner(_) => file.optional_section("runner.telemetry").context("[runner.telemetry]")?.unwrap_or_else(|| TelemetryConfig::new("mage-runner")),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::io::Write;

    use super::*;

    const EXAMPLE: &str = include_str!("../../../config/mage.example.toml");

    fn file(text: &str, vars: &[(&str, &str)]) -> Result<ConfigFile> {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(text.as_bytes()).unwrap();
        load(tmp.path(), vars.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())))
    }

    /// The example file without the sections other than `keep`.
    fn only(keep: &str) -> String {
        let mut table: toml::Table = EXAMPLE.parse().unwrap();
        table.retain(|section, _| section == keep);
        table.to_string()
    }

    fn err(result: Result<Roles>) -> String {
        match result {
            Ok(_) => panic!("routing succeeded"),
            Err(e) => format!("{e:#}"),
        }
    }

    #[test]
    fn the_example_file_starts_all_roles_in_development() {
        let config = file(EXAMPLE, &[]).unwrap();
        assert!(matches!(route(&config, StartTarget::All).unwrap(), Roles::All { .. }));
    }

    #[test]
    fn a_role_refuses_a_file_holding_another_roles_section() {
        let config = file(EXAMPLE, &[]).unwrap();
        // The runner must never be handed the server's credentials (database, vault, GitHub key).
        assert!(err(route(&config, StartTarget::Runner)).contains("[server]"));
        assert!(err(route(&config, StartTarget::Indexer)).contains("[server]"));
        assert!(err(route(&config, StartTarget::Server)).contains("[indexer]"));
        let runner_and_server = file(&format!("{}\n{}", only("runner"), only("server")), &[]).unwrap();
        assert!(err(route(&runner_and_server, StartTarget::Runner)).contains("[server]"));
    }

    #[test]
    fn a_role_starts_from_its_own_section_and_needs_it() {
        for (section, target) in [("server", StartTarget::Server), ("indexer", StartTarget::Indexer), ("runner", StartTarget::Runner)] {
            let config = file(&only(section), &[]).unwrap();
            route(&config, target).unwrap();
        }
        let empty = file("# no role sections\n", &[]).unwrap();
        assert!(err(route(&empty, StartTarget::Indexer)).contains("[indexer] is missing"));
        let runner_only = file(&only("runner"), &[]).unwrap();
        assert!(err(route(&runner_only, StartTarget::All)).contains("[server] is missing"));
    }

    #[test]
    fn all_is_refused_in_production() {
        let config = file(EXAMPLE, &[("MAGE_SERVER__ENVIRONMENT", "production")]).unwrap();
        assert!(err(route(&config, StartTarget::All)).contains("local development only"));
        let config = file(EXAMPLE, &[("MAGE_RUNNER__ENVIRONMENT", "production"), ("MAGE_RUNNER__ISOLATION", "enforced")]).unwrap();
        assert!(err(route(&config, StartTarget::All)).contains("[runner] environment is production"));
    }

    #[test]
    fn environment_overrides_beat_the_file_and_cannot_inline_secrets() {
        let vars = [
            ("MAGE_RUNNER__DAEMON__DRAIN_GRACE_SECS", "7"),
            ("MAGE_INDEXER__INTERNAL_LISTEN", "127.0.0.1:9999"),
            ("MAGE_SERVER__BIND", "127.0.0.1:9090"),
            // Not an override (no `__`): the target of a secret reference.
            ("MAGE_DATABASE_URL", "postgres://ignored"),
        ];
        let config = file(EXAMPLE, &vars).unwrap();
        let Roles::All { server, indexer, runner } = route(&config, StartTarget::All).unwrap() else { panic!("not all") };
        assert_eq!(runner.daemon.drain_grace_secs, 7);
        assert_eq!(indexer.internal_listen.port(), 9999);
        assert_eq!(server.bind.port(), 9090);
        assert!(!config.has("database_url"));

        let inline = file(EXAMPLE, &[("MAGE_INDEXER__GITHUB__APP_PRIVATE_KEY", "-----BEGIN RSA PRIVATE KEY-----")]);
        assert!(format!("{:#}", inline.err().unwrap()).contains("indexer.github.app_private_key"));
        let inline = file(&EXAMPLE.replace("file:~/.config/mage/runner-bootstrap", "mgb_inline"), &[]);
        assert!(format!("{:#}", inline.err().unwrap()).contains("runner.link.bootstrap_token"));
    }
}
