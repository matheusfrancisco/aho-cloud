//! `[indexer]` config section (server.md "Config", knowledge.md "Indexer worker concurrency",
//! "Text search, blame, and diff on Cloud Run"). `mage-app` deserializes it from the shared TOML
//! file (with `MAGE_INDEXER__…` overrides applied) and hands it to [`crate::compose::run`].
//!
//! ```toml
//! [indexer]
//! data_dir        = "/var/lib/mage/indexer"   # mirrors/, checkouts/, scip/, tmp/, home/
//! internal_listen = "10.0.0.5:8081"           # /internal/v1 (VPC only)
//!
//! [indexer.database]        # mage_infra::pg::PgConfig; statement_timeout_ms defaults to 60 s here
//! url_source = "env:MAGE_DATABASE_URL"
//!
//! [indexer.blob]            # mage_infra::blob::BlobConfig
//! backend = "gcs"
//! bucket  = "mage-artifacts"
//!
//! [indexer.github]          # the GitHub read app (shared with the server)
//! client_id       = "Iv1.xxxx"
//! app_private_key = "gcpsm:projects/p/secrets/github-read-app-key/versions/latest"
//!
//! [indexer.internal_auth]   # whose `mgi_` tokens the internal API accepts
//! issuer = "https://mage.example.com"
//!
//! [indexer.workers]         # fetch / parse / bundle_source / other pool sizes
//! [indexer.git]             # git_bin, command_timeout_secs
//! [indexer.treesitter]      # parse_threads
//! [indexer.telemetry]       # read by mage-app (one subscriber per process)
//! ```

use std::net::SocketAddr;
use std::path::PathBuf;

use mage_infra::blob::BlobConfig;
use mage_infra::config::{SecretSource, expand_home};
use mage_infra::github::GitHubConfig;
use mage_infra::pg::PgConfig;
use mage_infra::telemetry::TelemetryConfig;
use serde::{Deserialize, Deserializer};

use crate::adapters::driven::git::GitConfig;
use crate::adapters::driven::treesitter::TreeSitterConfig;
use crate::adapters::driving::http::auth::InternalAuthConfig;
use crate::adapters::driving::worker::WorkersConfig;
use crate::app::index::IndexConfig;
use crate::app::live::LiveConfig;

/// Indexer statements may scan large fact tables (storage.md: server 5 s, indexer 60 s).
pub const INDEXER_STATEMENT_TIMEOUT_MS: u64 = 60_000;

/// The `[indexer]` section.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Local disk of the indexer: mirrors, checkouts, SCIP downloads, staging (`~/` expands).
    #[serde(deserialize_with = "home_path")]
    pub data_dir: PathBuf,
    /// Listen address of the internal API (`/internal/v1`).
    pub internal_listen: SocketAddr,
    /// Worker id prefix in `jobs.leased_by`; default the `HOSTNAME` (else `indexer`).
    #[serde(default)]
    pub worker_name: Option<String>,
    /// SourceBundle size cap (default 2 GiB).
    #[serde(default)]
    pub bundle_max_bytes: Option<u64>,
    /// Rev rebuilds `index.reconcile` enqueues per tick (default 20).
    #[serde(default)]
    pub rebuilds_per_tick: Option<u32>,
    /// Checkout cache cap (default 20 GiB).
    #[serde(default)]
    pub checkout_cache_bytes: Option<u64>,
    /// Searches in flight (default 4).
    #[serde(default)]
    pub max_searches: Option<usize>,
    #[serde(deserialize_with = "indexer_pg")]
    pub database: PgConfig,
    pub blob: BlobConfig,
    pub github: GitHubAppConfig,
    pub internal_auth: InternalAuthConfig,
    #[serde(default)]
    pub workers: WorkersConfig,
    #[serde(default)]
    pub git: GitSection,
    #[serde(default)]
    pub treesitter: TreeSitterConfig,
    /// Installed by `mage-app` (one global subscriber per process).
    #[serde(default)]
    pub telemetry: Option<TelemetryConfig>,
}

/// `[indexer.github]`: the GitHub read app the indexer mints installation tokens with (same keys
/// as `[server.github]`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitHubAppConfig {
    /// App client id (JWT `iss`).
    pub client_id: String,
    /// PEM private key (secret reference).
    pub app_private_key: SecretSource,
    /// `[indexer.github.api]`: REST / web roots and timeouts (github.com defaults).
    #[serde(default)]
    pub api: GitHubConfig,
}

/// `[indexer.git]` (the data dir is `indexer.data_dir`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitSection {
    /// Default `git` from `PATH`.
    #[serde(default)]
    pub git_bin: Option<PathBuf>,
    /// Default 600 s.
    #[serde(default)]
    pub command_timeout_secs: Option<u64>,
}

impl Config {
    pub fn index_config(&self) -> IndexConfig {
        let mut config = IndexConfig::new(self.data_dir.clone());
        if let Some(bytes) = self.bundle_max_bytes {
            config.bundle_max_bytes = bytes;
        }
        if let Some(n) = self.rebuilds_per_tick {
            config.rebuilds_per_tick = n;
        }
        config
    }

    pub fn live_config(&self) -> LiveConfig {
        let mut config = LiveConfig::new(self.data_dir.clone());
        if let Some(bytes) = self.checkout_cache_bytes {
            config.checkout_cache_bytes = bytes;
        }
        if let Some(n) = self.max_searches {
            config.max_searches = n;
        }
        config
    }

    pub fn git_config(&self) -> GitConfig {
        GitConfig {
            data_dir: self.data_dir.clone(),
            git_bin: self.git.git_bin.clone().unwrap_or_else(|| PathBuf::from("git")),
            command_timeout_secs: self.git.command_timeout_secs.unwrap_or(600),
            allow_file_protocol: false,
        }
    }

    /// `[indexer.telemetry]`, default JSON lines as `mage-indexer`.
    pub fn telemetry(&self) -> TelemetryConfig {
        self.telemetry.clone().unwrap_or_else(|| TelemetryConfig::new("mage-indexer"))
    }
}

fn home_path<'de, D: Deserializer<'de>>(d: D) -> Result<PathBuf, D::Error> {
    Ok(expand_home(&String::deserialize(d)?))
}

/// `PgConfig` with the indexer's statement timeout as the default.
fn indexer_pg<'de, D: Deserializer<'de>>(d: D) -> Result<PgConfig, D::Error> {
    let mut table = serde_json::Map::<String, serde_json::Value>::deserialize(d)?;
    table.entry("statement_timeout_ms").or_insert_with(|| serde_json::Value::from(INDEXER_STATEMENT_TIMEOUT_MS));
    serde_json::from_value(serde_json::Value::Object(table)).map_err(serde::de::Error::custom)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
        data_dir = "/var/lib/mage/indexer"
        internal_listen = "127.0.0.1:8081"
        [database]
        url_source = "env:MAGE_DATABASE_URL"
        [blob]
        backend = "local"
        root = "/var/lib/mage/blob"
        [github]
        client_id = "Iv1.test"
        app_private_key = "file:/etc/mage/github.pem"
        [internal_auth]
        issuer = "http://127.0.0.1:8080"
    "#;

    #[test]
    fn database_defaults_to_the_indexer_statement_timeout_unless_set() {
        let config: Config = toml::from_str(MINIMAL).unwrap();
        assert_eq!(config.database.statement_timeout_ms, INDEXER_STATEMENT_TIMEOUT_MS);
        let explicit = MINIMAL.replace("url_source = \"env:MAGE_DATABASE_URL\"", "url_source = \"env:MAGE_DATABASE_URL\"\nstatement_timeout_ms = 1234");
        let config: Config = toml::from_str(&explicit).unwrap();
        assert_eq!(config.database.statement_timeout_ms, 1234);
    }

    #[test]
    fn database_still_refuses_unknown_keys_and_inline_urls() {
        let unknown = MINIMAL.replace("url_source = \"env:MAGE_DATABASE_URL\"", "url_source = \"env:MAGE_DATABASE_URL\"\npool_size = 3");
        assert!(toml::from_str::<Config>(&unknown).is_err());
        let inline = MINIMAL.replace("env:MAGE_DATABASE_URL", "postgres://u:p@h/db");
        assert!(toml::from_str::<Config>(&inline).is_err());
    }
}
