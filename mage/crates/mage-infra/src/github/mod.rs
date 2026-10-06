//! GitHub App client and `SourceHost` adapter (intake.md "`mage_infra::github` (shared client,
//! `SourceHost`) and `mage_server::adapters::driven::github`").
//!
//! [`GitHubClient`] owns the HTTP client, the app JWT and the installation-token cache of ONE GitHub
//! App: the read app and the Fix App are separate instances. [`GitHubSourceHost`] implements
//! `SourceHost` on top of it; `mage_server::adapters::driven::github` builds the server-only ports
//! on the same client through [`GitHubClient::get`] / [`GitHubClient::send`].
//!
//! Tokens never leave memory and print as `***` (`AccessToken`); nothing here logs them.

mod client;
mod errors;
mod etag;
mod link;
mod source_host;
mod tokens;
mod wire;

use std::fmt;

use serde::Deserialize;
use url::Url;

pub use client::{ApiResponse, Auth, GitHubClient};
pub use source_host::GitHubSourceHost;

/// HTTP endpoints and timeouts. Every field has a serde default (github.com).
#[derive(Debug, Clone, Deserialize)]
pub struct GitHubConfig {
    /// REST API root; may carry a path prefix (GitHub Enterprise `…/api/v3/`, test servers).
    #[serde(default = "default_api_base")]
    pub api_base: Url,
    /// Web root: git fetch URLs (`{web_base}{owner}/{name}.git`) and the OAuth endpoints.
    #[serde(default = "default_web_base")]
    pub web_base: Url,
    #[serde(default = "default_connect_timeout_ms")]
    pub connect_timeout_ms: u64,
    #[serde(default = "default_request_timeout_ms")]
    pub request_timeout_ms: u64,
}

impl Default for GitHubConfig {
    fn default() -> Self {
        Self {
            api_base: default_api_base(),
            web_base: default_web_base(),
            connect_timeout_ms: default_connect_timeout_ms(),
            request_timeout_ms: default_request_timeout_ms(),
        }
    }
}

#[allow(clippy::expect_used)]
fn default_api_base() -> Url {
    Url::parse("https://api.github.com/").expect("static URL")
}

#[allow(clippy::expect_used)]
fn default_web_base() -> Url {
    Url::parse("https://github.com/").expect("static URL")
}

fn default_connect_timeout_ms() -> u64 {
    5_000
}

fn default_request_timeout_ms() -> u64 {
    20_000
}

/// A GitHub App's identity: the client id (JWT `iss`) and its PEM private key (PKCS#1 or PKCS#8).
#[derive(Clone)]
pub struct AppCredentials {
    pub client_id: String,
    pub private_key_pem: String,
}

impl fmt::Debug for AppCredentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AppCredentials").field("client_id", &self.client_id).field("private_key_pem", &"***").finish()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn config_defaults_apply_per_field() {
        let config: GitHubConfig = serde_json::from_str(r#"{"request_timeout_ms": 7}"#).unwrap();
        assert_eq!(config.api_base.as_str(), "https://api.github.com/");
        assert_eq!(config.web_base.as_str(), "https://github.com/");
        assert_eq!((config.connect_timeout_ms, config.request_timeout_ms), (5_000, 7));
    }

    #[test]
    fn credentials_debug_redacts_the_key() {
        let creds = AppCredentials { client_id: "Iv1.abc".into(), private_key_pem: "-----BEGIN RSA PRIVATE KEY-----secret".into() };
        let printed = format!("{creds:?}");
        assert!(printed.contains("Iv1.abc") && !printed.contains("secret"), "{printed}");
    }
}
