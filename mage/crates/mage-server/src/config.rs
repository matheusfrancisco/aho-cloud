//! `[server]` config section (server.md "Config"): one struct per area, deserialized by `mage-app`
//! with `ConfigFile::section::<Config>("server")` after `check_no_inline_secrets`. Credentials are
//! only `SecretSource` references (`env:` / `file:` / `gcpsm:`) or `SecretVault` names; nothing
//! here holds a secret value.

use std::net::SocketAddr;

use serde::Deserialize;
use url::Url;

use mage_infra::blob::BlobConfig;
use mage_infra::config::SecretSource;
use mage_infra::github::GitHubConfig;
use mage_infra::pg::pool::PgConfig;
use mage_infra::telemetry::TelemetryConfig;

use crate::adapters::driven::github::pull_requests::PullRequestsConfig;
use crate::adapters::driven::identity::{GitHubLoginConfig, GoogleLoginConfig};
use crate::adapters::driven::linear::LinearConfig;
use crate::adapters::driven::slack::SlackConfig;
use crate::adapters::driven::vault::VaultConfig;
use crate::adapters::driving::worker::JobsConfig;
use crate::app::auth::SessionPolicy;
use crate::app::fix::FixConfig;
use crate::app::gateway::GatewayConfig;
use crate::app::intake::lifecycle::ConnectConfig;
use crate::app::intake::{GithubInstallSeed, IntakeSection};
use crate::app::knowledge_query::KnowledgeConfig;
use crate::app::runs::RunPolicy;
use crate::app::settings::HarnessConfig;
use crate::app::triage::TriageConfig;

/// `server.environment`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Environment {
    /// Refuses dev-only backends (local vault key, harness login) and `mage start all`.
    Production,
    Development,
}

/// The `[server]` section.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub environment: Environment,
    /// Browser, OAuth callback, runner and harness base URL (`{public_url}/llm/anthropic`, …).
    pub public_url: Url,
    /// Listen address (Cloud Run: `0.0.0.0:$PORT`).
    #[serde(default = "default_bind")]
    pub bind: SocketAddr,
    /// `[server.database]`.
    pub database: PgConfig,
    /// `[server.blob]`.
    pub blob: BlobConfig,
    /// `[server.vault]`.
    pub vault: VaultConfig,
    /// `[server.github]`: the read-only GitHub App (shared with the indexer).
    pub github: GithubSection,
    /// `[server.github_fix]`: the Mage Fix App (fix.md F3); absent until F3 is set up.
    #[serde(default)]
    pub github_fix: Option<GithubFixSection>,
    /// `[server.login]`.
    pub login: LoginSection,
    /// `[server.session]`.
    #[serde(default)]
    pub session: SessionPolicy,
    /// `[server.integrations]`: connect URLs, app slugs and client ids.
    #[serde(default)]
    pub integrations: ConnectConfig,
    /// `[server.intake]`.
    #[serde(default)]
    pub intake: IntakeSection,
    /// `[server.linear]`.
    #[serde(default)]
    pub linear: LinearConfig,
    /// `[server.slack]`.
    #[serde(default)]
    pub slack: SlackConfig,
    /// `[server.triage]`; absent = triage.md defaults with `runs.default_model`.
    #[serde(default)]
    pub triage: Option<TriageConfig>,
    /// `[server.runs]` (`public_url` is always `server.public_url`).
    #[serde(default)]
    pub runs: RunPolicy,
    /// `[server.fix]`.
    #[serde(default)]
    pub fix: FixSection,
    /// `[server.gateway]`.
    #[serde(default)]
    pub gateway: GatewayConfig,
    /// `[server.harness]`.
    #[serde(default)]
    pub harness: HarnessConfig,
    /// `[server.knowledge]`: indexer link and Knowledge MCP limits.
    pub knowledge: KnowledgeConfig,
    /// `[server.jobs]`.
    #[serde(default)]
    pub jobs: JobsConfig,
    /// `[server.telemetry]`; `mage-app` installs the subscriber from it.
    #[serde(default = "default_telemetry")]
    pub telemetry: TelemetryConfig,
}

/// `[server.github]`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubSection {
    /// Read app client id (`Iv1.…`): app JWT issuer and OAuth client of the install code exchange
    /// (secret `github/oauth_client_secret` in `SecretVault`).
    pub client_id: String,
    /// The read app's PEM key, shared with the indexer (server.md "Secret inventory").
    pub app_private_key: SecretSource,
    /// `[server.github.api]`: endpoints and timeouts, shared by both apps (github.com defaults).
    #[serde(default)]
    pub api: GitHubConfig,
    /// Internal v1: installations bound to orgs at start, without the install callback.
    #[serde(default)]
    pub installations: Vec<GithubInstallSeed>,
}

/// `[server.github_fix]`. Its private key is the Platform secret `github_fix/app_private_key`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubFixSection {
    pub client_id: String,
    #[serde(default)]
    pub pull_requests: PullRequestsConfig,
}

/// `[server.login]`: GitHub is the self-host login; Google is optional. Client secrets are the
/// Platform secrets `github/oauth_client_secret` and `google/oauth_client_secret`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LoginSection {
    pub github: GitHubLoginConfig,
    #[serde(default)]
    pub google: Option<GoogleLoginConfig>,
}

/// `[server.fix]`: `FixConfig` without `public_url` (always `server.public_url`); absent fields
/// keep fix.md's defaults.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixSection {
    #[serde(default)]
    pub github_web: Option<Url>,
    #[serde(default)]
    pub script_inline_max_bytes: Option<u64>,
    #[serde(default)]
    pub rebase_poll_secs: Option<u64>,
    #[serde(default)]
    pub brief_url_ttl_secs: Option<u64>,
}

impl FixSection {
    pub fn fix_config(&self, public_url: Url) -> FixConfig {
        let mut config = FixConfig::new(public_url);
        if let Some(github_web) = &self.github_web {
            config.github_web = github_web.clone();
        }
        if let Some(bytes) = self.script_inline_max_bytes {
            config.script_inline_max_bytes = bytes;
        }
        if let Some(secs) = self.rebase_poll_secs {
            config.rebase_poll_secs = secs;
        }
        if let Some(secs) = self.brief_url_ttl_secs {
            config.brief_url_ttl_secs = secs;
        }
        config
    }
}

fn default_bind() -> SocketAddr {
    SocketAddr::from(([0, 0, 0, 0], 8080))
}

fn default_telemetry() -> TelemetryConfig {
    TelemetryConfig::new("mage-server")
}

impl Config {
    pub fn is_production(&self) -> bool {
        self.environment == Environment::Production
    }

    /// `public_url` without a trailing slash: token issuer and the base of every derived URL.
    pub fn public_base(&self) -> String {
        self.public_url.as_str().trim_end_matches('/').to_owned()
    }

    /// `[server.triage]`, or its defaults with `runs.default_model`; `production` and the Claude
    /// Code verification flag always follow `environment` and `[server.harness]`.
    pub fn triage_config(&self) -> TriageConfig {
        let mut triage = self.triage.clone().unwrap_or_else(|| TriageConfig::new(self.runs.default_model.clone()));
        triage.production = self.is_production();
        triage.claude_code_gateway_verified = self.harness.claude_code.gateway_verified;
        triage
    }

    /// `[server.runs]` with `public_url` from `server.public_url`.
    pub fn run_policy(&self) -> RunPolicy {
        RunPolicy { public_url: self.public_base(), ..self.runs.clone() }
    }

    /// `[server.integrations]` with the Linear and Slack client ids taken from `[server.linear]` /
    /// `[server.slack]` when left unset there.
    pub fn connect_config(&self) -> ConnectConfig {
        let mut connect = self.integrations.clone();
        if connect.linear_client_id.is_none() {
            connect.linear_client_id = self.linear.oauth_client_id.clone();
        }
        if connect.slack_client_id.is_none() {
            connect.slack_client_id = self.slack.oauth_client_id.clone();
        }
        connect
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
        environment = "development"
        public_url = "http://localhost:8080/"
        [database]
        url_source = "env:MAGE_DATABASE_URL"
        [blob]
        backend = "local"
        [vault]
        backend = "local-key-file"
        local_key_file = "~/.config/mage/dev.key"
        [github]
        client_id = "Iv1.read"
        app_private_key = "file:~/.config/mage/github-read.pem"
        [login.github]
        client_id = "Iv1.read"
        [knowledge]
        indexer_url = "http://127.0.0.1:8081"
    "#;

    #[test]
    fn derived_sections_follow_the_top_level_fields() {
        let mut config: Config = toml::from_str(MINIMAL).unwrap();
        config.harness.claude_code.gateway_verified = true;
        config.runs.public_url = "http://elsewhere".into();
        assert_eq!(config.bind, SocketAddr::from(([0, 0, 0, 0], 8080)));
        assert_eq!(config.run_policy().public_url, "http://localhost:8080");
        let triage = config.triage_config();
        assert_eq!(triage.default_model, config.runs.default_model);
        assert!(!triage.production);
        assert!(triage.claude_code_gateway_verified);
        config.environment = Environment::Production;
        assert!(config.triage_config().production);
        let fix = FixSection { rebase_poll_secs: Some(5), ..FixSection::default() }.fix_config(config.public_url.clone());
        assert_eq!((fix.rebase_poll_secs, fix.public_url.as_str()), (5, "http://localhost:8080/"));
    }

    #[test]
    fn unknown_fields_and_inline_secrets_are_refused() {
        let unknown = format!("trace_viewer = true\n{MINIMAL}");
        assert!(toml::from_str::<Config>(&unknown).is_err());
        let inline = MINIMAL.replace("file:~/.config/mage/github-read.pem", "-----BEGIN RSA PRIVATE KEY-----");
        assert!(toml::from_str::<Config>(&inline).is_err());
    }

    #[test]
    fn shipped_example_config_loads() {
        let file = mage_infra::config::ConfigFile::parse(include_str!("../../../config/mage.example.toml"), "mage.example.toml").unwrap();
        file.check_no_inline_secrets().unwrap();
        let config: Config = file.section("server").unwrap();
        assert_eq!(config.environment, Environment::Development);
    }
}
