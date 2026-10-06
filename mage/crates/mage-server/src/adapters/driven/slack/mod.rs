//! Slack adapter (intake.md "`mage_server::adapters::driven::slack`"): [`SlackNotifier`]
//! implements `ChatNotifier` over the Slack Web API, [`SlackDecoder`] implements
//! `WebhookDecoder` for Events API deliveries.
//!
//! Credentials: the bot token per team lives in SecretVault `Org(org)` under
//! `secret_names::slack_bot_token(team)`; the OAuth client secret (public app only) in `Platform`
//! under `secret_names::SLACK_OAUTH_CLIENT_SECRET`.

mod client;
mod notifier;
mod webhook;
#[cfg(test)]
mod notifier_tests;

pub use notifier::SlackNotifier;
pub use webhook::SlackDecoder;

use serde::Deserialize;
use url::Url;

use crate::domain::intake::secret_names;
use crate::ports::vault::{SecretName, VaultError};

/// SecretVault name of a team's bot token (`Org` scope).
pub(crate) fn bot_token_secret_name(team_id: &str) -> Result<SecretName, VaultError> {
    SecretName::new(secret_names::slack_bot_token(team_id))
}

/// `[server.intake.slack]`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SlackConfig {
    /// Web API base, `https://slack.com/api/`.
    pub api_base: Url,
    /// Public (distributed) app only: `oauth.v2.access` client id. `None` = internal app without
    /// an OAuth connect flow.
    pub oauth_client_id: Option<String>,
    /// `conversations.replies` page size: 200 for the internal app, 15 for a non-Marketplace
    /// distributed app (Slack's limit since 2025-05-29).
    pub replies_page_limit: u16,
    /// Minimum spacing of `chat.postMessage` calls per channel (Slack allows about 1 per second).
    pub post_interval_ms: u64,
    /// How long a loaded bot token is reused before SecretVault is read again.
    pub token_cache_secs: u64,
    pub connect_timeout_ms: u64,
    pub request_timeout_ms: u64,
}

impl Default for SlackConfig {
    #[allow(clippy::expect_used)]
    fn default() -> Self {
        SlackConfig {
            api_base: Url::parse("https://slack.com/api/").expect("static url"),
            oauth_client_id: None,
            replies_page_limit: 200,
            post_interval_ms: 1_000,
            token_cache_secs: 300,
            connect_timeout_ms: 5_000,
            request_timeout_ms: 15_000,
        }
    }
}

/// Slack mrkdwn escaping of the three control characters (`&amp; &lt; &gt;`); everything else is
/// sent literally, so `<!channel>` or `<url|x>` in the input render inert.
pub(crate) fn escape_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

/// `<url|label>`: the label loses `|` and line breaks and is escaped; the url has `|` percent-encoded
/// (`<`, `>` and spaces are already encoded by `Url`) and `&` escaped as Slack requires.
pub(crate) fn render_link(url: &Url, label: &str) -> String {
    let label: String = label.chars().filter(|c| *c != '|').map(|c| if c.is_control() { ' ' } else { c }).collect();
    let url = url.as_str().replace('|', "%7C").replace('<', "%3C").replace('>', "%3E");
    format!("<{}|{}>", escape_text(&url), escape_text(label.trim()))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn escaping_neutralizes_mentions_and_links() {
        assert_eq!(escape_text("a & b <!channel> <https://evil|x>"), "a &amp; b &lt;!channel&gt; &lt;https://evil|x&gt;");
    }

    #[test]
    fn link_label_cannot_break_out() {
        let url = Url::parse("https://mage.example/tickets/1?a=1&b=x|y").unwrap();
        assert_eq!(render_link(&url, "Open|<b>\nnow"), "<https://mage.example/tickets/1?a=1&amp;b=x%7Cy|Open&lt;b&gt; now>");
    }

    #[test]
    fn bot_token_name_is_valid_for_upper_case_team_ids() {
        assert_eq!(bot_token_secret_name("T024BE7LD").unwrap().as_str(), "integration/slack/t024be7ld/bot_token");
    }

    #[test]
    fn config_defaults_and_partial_override() {
        let cfg: SlackConfig = toml::from_str("replies_page_limit = 15").unwrap();
        assert_eq!(cfg.replies_page_limit, 15);
        assert_eq!(cfg.api_base.as_str(), "https://slack.com/api/");
        assert!(toml::from_str::<SlackConfig>("nope = 1").is_err());
    }
}
