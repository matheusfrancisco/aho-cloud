//! Linear adapters (intake.md "`mage_server::adapters::driven::linear`"): [`LinearTracker`]
//! (`TicketTracker` over Linear's GraphQL API) and [`LinearDecoder`] (`WebhookDecoder` for
//! `/hooks/linear`).
//!
//! GraphQL operations are hand-written query strings with serde response types (`queries`), not
//! `graphql_client` codegen. Credentials come from `SecretVault` (Org scope) per the workspace the
//! org's active Linear integration names; see `auth`.

mod auth;
mod client;
mod queries;
mod tracker;
mod webhook;

#[cfg(test)]
mod tests;

use serde::Deserialize;
use url::Url;

pub use tracker::LinearTracker;
pub use webhook::LinearDecoder;

/// `[server.linear]`: endpoints and OAuth client of the Linear integration.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LinearConfig {
    /// GraphQL endpoint.
    pub api_url: Url,
    /// OAuth token endpoint (code exchange and refresh).
    pub oauth_token_url: Url,
    /// Public OAuth app client id; required for OAuth mode (refresh, `complete_install`, app id in
    /// `identity`). The client secret is the Platform secret `linear/oauth_client_secret`.
    pub oauth_client_id: Option<String>,
    pub connect_timeout_ms: u64,
    /// GraphQL and token requests.
    pub request_timeout_ms: u64,
    /// The PUT of `upload_file` bytes to the pre-signed URL (videos up to ~10 MB).
    pub upload_timeout_ms: u64,
}

impl Default for LinearConfig {
    fn default() -> Self {
        LinearConfig {
            api_url: Url::parse("https://api.linear.app/graphql").expect("static url"),
            oauth_token_url: Url::parse("https://api.linear.app/oauth/token").expect("static url"),
            oauth_client_id: None,
            connect_timeout_ms: 5_000,
            request_timeout_ms: 20_000,
            upload_timeout_ms: 120_000,
        }
    }
}
