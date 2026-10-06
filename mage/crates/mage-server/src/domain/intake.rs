//! Intake domain types owned by the server role (intake.md "Configuration", "Additions"):
//! per-org [`IntakeConfig`] (jsonb in `org_intake_config`), [`ThreadSnapshot`] (jsonb in
//! `ticket_thread_snapshots`) and per-kind [`IntegrationSettings`] (jsonb in
//! `integrations.settings`).

use std::collections::BTreeMap;
use std::time::Duration;

use mage_core::domain::SlackThreadRef;
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

/// Per-org intake configuration, edited in the UI and seeded from `[server.intake.orgs.<slug>]`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IntakeConfig {
    pub linear: LinearIntake,
    /// First match wins.
    pub routes: Vec<RepoRoute>,
    /// `owner/name` used when no route matches.
    pub default_repo: Option<String>,
    pub environments: Environments,
    pub github: GithubIntake,
    pub notify: NotifyConfig,
    pub debounce: Debounce,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LinearIntake {
    /// Linear team keys; empty = no team accepted.
    pub teams: Vec<String>,
    /// Any-of; empty = no requirement.
    pub require_labels: Vec<String>,
    pub exclude_labels: Vec<String>,
    /// Linear workflow state *types* (`triage`, `backlog`, `unstarted`, `started`) in which an
    /// issue is triaged.
    pub triage_states: Vec<String>,
    pub customer_label_prefix: String,
    /// Honor `mage-sha:` / `mage-ref:` lines in descriptions.
    pub directives: bool,
    /// Accepted age of Linear's signed `webhookTimestamp`.
    pub max_skew_secs: u64,
}

impl Default for LinearIntake {
    fn default() -> Self {
        Self {
            teams: Vec::new(),
            require_labels: Vec::new(),
            exclude_labels: vec!["mage:ignore".into()],
            triage_states: ["triage", "backlog", "unstarted", "started"].map(String::from).to_vec(),
            customer_label_prefix: "customer:".into(),
            directives: true,
            max_skew_secs: 60,
        }
    }
}

/// `when` matches on team key, label and project; every given field must match.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoRoute {
    #[serde(default)]
    pub when: RouteWhen,
    /// `owner/name`.
    pub repo: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RouteWhen {
    pub team: Option<String>,
    pub label: Option<String>,
    pub project: Option<String>,
}

impl RouteWhen {
    /// Team keys and labels compare case-insensitively (Linear treats them so); projects by name.
    pub fn matches(&self, team_key: &str, labels: &[String], project: Option<&str>) -> bool {
        let team_ok = self.team.as_deref().is_none_or(|t| t.eq_ignore_ascii_case(team_key));
        let label_ok = self.label.as_deref().is_none_or(|l| labels.iter().any(|x| x.eq_ignore_ascii_case(l)));
        let project_ok = self.project.as_deref().is_none_or(|p| project.is_some_and(|x| x.eq_ignore_ascii_case(p)));
        team_ok && label_ok && project_ok
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Environments {
    /// GitHub environment globs (`*`, `?`) recorded as deployments.
    pub allow: Vec<String>,
    /// Alias → canonical environment name, applied before `allow`.
    pub aliases: BTreeMap<String, String>,
    /// Used when the customer has no environment.
    pub default: String,
}

impl Default for Environments {
    fn default() -> Self {
        Self { allow: vec!["production".into(), "staging".into()], aliases: BTreeMap::new(), default: "production".into() }
    }
}

impl Environments {
    /// Alias applied (one level).
    pub fn canonical<'a>(&'a self, name: &'a str) -> &'a str {
        self.aliases.get(name).map_or(name, String::as_str)
    }

    /// Canonical name if it passes the `allow` globs.
    pub fn allowed(&self, name: &str) -> Option<String> {
        let canonical = self.canonical(name);
        self.allow.iter().any(|g| wildcard_match(g, canonical)).then(|| canonical.to_owned())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GithubIntake {
    /// Tags matching this glob (`*`, `?`) become `Release(tag)` versions; empty = none.
    pub release_tag_glob: String,
}

impl Default for GithubIntake {
    fn default() -> Self {
        Self { release_tag_glob: "v*".into() }
    }
}

impl GithubIntake {
    pub fn is_release_tag(&self, tag: &str) -> bool {
        !self.release_tag_glob.is_empty() && wildcard_match(&self.release_tag_glob, tag)
    }
}

/// Notice kinds named in `notify.linear` / `notify.slack`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoticeKind {
    TriageReady,
    DiagnosisVerdict,
    FixBriefReady,
    FixReady,
    FixFailed,
    PrOpened,
}

impl NoticeKind {
    pub fn of(notice: &mage_core::domain::Notice) -> Self {
        use mage_core::domain::Notice as N;
        match notice {
            N::TriageReady { .. } => Self::TriageReady,
            N::DiagnosisVerdict { .. } => Self::DiagnosisVerdict,
            N::FixBriefReady { .. } => Self::FixBriefReady,
            N::FixReady { .. } => Self::FixReady,
            N::FixFailed { .. } => Self::FixFailed,
            N::PrOpened { .. } => Self::PrOpened,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SlackDetail {
    /// Verdict and a Mage link only (Slack Connect threads can include the customer).
    #[default]
    Minimal,
    Full,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NotifyConfig {
    pub linear: Vec<NoticeKind>,
    pub slack: Vec<NoticeKind>,
    pub slack_detail: SlackDetail,
    /// Linear workflow state set when a PR opens; empty = never change state.
    pub linear_state_on_pr: String,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        use NoticeKind::*;
        Self {
            linear: vec![TriageReady, DiagnosisVerdict, FixBriefReady, FixReady, FixFailed, PrOpened],
            slack: vec![TriageReady, DiagnosisVerdict, PrOpened],
            slack_detail: SlackDetail::Minimal,
            linear_state_on_pr: String::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Debounce {
    pub issue_secs: u64,
    pub push_index_secs: u64,
    /// Public non-Marketplace Slack apps: >= 3600.
    pub thread_refresh_secs: u64,
}

impl Default for Debounce {
    fn default() -> Self {
        Self { issue_secs: 10, push_index_secs: 60, thread_refresh_secs: 600 }
    }
}

impl Debounce {
    pub fn issue(&self) -> Duration {
        Duration::from_secs(self.issue_secs)
    }
    pub fn push_index(&self) -> Duration {
        Duration::from_secs(self.push_index_secs)
    }
    pub fn thread_refresh(&self) -> Duration {
        Duration::from_secs(self.thread_refresh_secs)
    }
}

/// Stored Slack thread content of a ticket (refreshed at most every `debounce.thread_refresh`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadSnapshot {
    pub thread: SlackThreadRef,
    #[serde(with = "time::serde::rfc3339")]
    pub fetched_at: OffsetDateTime,
    /// Oldest first; human messages only.
    pub messages: Vec<ThreadMessage>,
    /// Why the thread could not be read (e.g. "not_in_channel"); the UI asks to invite the bot.
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadMessage {
    pub ts: String,
    pub author_id: String,
    pub text: String,
    pub file_names: Vec<String>,
}

/// How Mage authenticates to a Linear workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinearAuthMode {
    /// Internal v1: `integration/linear/{workspace}/api_key`.
    ApiKey,
    /// Public: `integration/linear/{workspace}/oauth`.
    OAuth,
}

/// Non-secret per-integration settings (`integrations.settings`); fields apply per kind.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct IntegrationSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub linear_auth: Option<LinearAuthMode>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub linear_workspace_name: Option<String>,
    /// Linear user / app actor Mage acts as (own-content detection).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub linear_bot_actor_id: Option<String>,
    /// `{team_domain}.slack.com`, maps thread URLs to the team.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slack_team_domain: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slack_bot_user_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub github_account_login: Option<String>,
}

/// SecretVault names of integration credentials (server.md secret catalog). Provider ids are
/// lowercased because secret names are `[a-z0-9_./-]` (Slack team ids are uppercase); integration
/// lookups still compare `external_id` exactly.
pub mod secret_names {
    /// Platform scope.
    pub const GITHUB_OAUTH_CLIENT_SECRET: &str = "github/oauth_client_secret";
    /// Platform scope (fix.md F3 app).
    pub const GITHUB_FIX_OAUTH_CLIENT_SECRET: &str = "github_fix/oauth_client_secret";
    /// Platform scope.
    pub const LINEAR_OAUTH_CLIENT_SECRET: &str = "linear/oauth_client_secret";
    /// Platform scope.
    pub const SLACK_OAUTH_CLIENT_SECRET: &str = "slack/oauth_client_secret";
    /// Platform scope webhook secrets, by route.
    pub const GITHUB_WEBHOOK_SECRET: &str = "github/webhook_secret";
    pub const GITHUB_FIX_WEBHOOK_SECRET: &str = "github_fix/webhook_secret";
    pub const LINEAR_WEBHOOK_SECRET: &str = "linear/webhook_secret";
    pub const SLACK_SIGNING_SECRET: &str = "slack/signing_secret";

    /// Org scope: internal v1 API key.
    pub fn linear_api_key(workspace_id: &str) -> String {
        format!("integration/linear/{}/api_key", workspace_id.to_ascii_lowercase())
    }

    /// Org scope: OAuth grant JSON `{access_token, refresh_token, expires_at}`.
    pub fn linear_oauth(workspace_id: &str) -> String {
        format!("integration/linear/{}/oauth", workspace_id.to_ascii_lowercase())
    }

    /// Org scope.
    pub fn slack_bot_token(team_id: &str) -> String {
        format!("integration/slack/{}/bot_token", team_id.to_ascii_lowercase())
    }
}

/// `*` matches any run of characters, `?` exactly one; everything else literally.
pub fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_semantics() {
        assert!(wildcard_match("v*", "v1.2.3"));
        assert!(wildcard_match("v*", "v"));
        assert!(!wildcard_match("v*", "release-1"));
        assert!(wildcard_match("pr-?", "pr-1"));
        assert!(!wildcard_match("pr-?", "pr-12"));
        assert!(wildcard_match("*-prod", "eu-prod"));
        assert!(wildcard_match("a*b*c", "axxbyyc"));
        assert!(!wildcard_match("a*b*c", "axxbyy"));
        assert!(wildcard_match("production", "production"));
        assert!(!wildcard_match("production", "production-2"));
    }

    #[test]
    fn environments_apply_alias_then_allow() {
        let env = Environments {
            allow: vec!["production".into(), "staging*".into()],
            aliases: BTreeMap::from([("prod".into(), "production".into())]),
            default: "production".into(),
        };
        assert_eq!(env.allowed("prod").as_deref(), Some("production"));
        assert_eq!(env.allowed("staging-eu").as_deref(), Some("staging-eu"));
        assert_eq!(env.allowed("preview-42"), None);
    }

    #[test]
    fn route_when_requires_every_given_field() {
        let when = RouteWhen { team: Some("ENG".into()), label: Some("frontend".into()), project: None };
        assert!(when.matches("eng", &["Frontend".into()], None));
        assert!(!when.matches("ENG", &["backend".into()], None));
        assert!(!when.matches("SUP", &["frontend".into()], Some("x")));
        assert!(RouteWhen::default().matches("ANY", &[], None));
    }

    #[test]
    fn config_parses_doc_toml() {
        let toml = r#"
            [linear]
            teams = ["ENG", "SUP"]
            exclude_labels = ["mage:ignore"]
            customer_label_prefix = "customer:"
            directives = true
            max_skew_secs = 60

            [[routes]]
            when = { team = "ENG", label = "frontend" }
            repo = "hoophq/webapp"
            [[routes]]
            when = { team = "ENG" }
            repo = "hoophq/hoop"

            [environments]
            allow = ["production", "staging"]
            aliases = { "prod" = "production" }
            default = "production"

            [github]
            release_tag_glob = "v*"

            [notify]
            linear = ["triage_ready", "fix_failed"]
            slack = ["pr_opened"]
            slack_detail = "full"
            linear_state_on_pr = "In Review"

            [debounce]
            issue_secs = 5
        "#;
        let cfg: IntakeConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.linear.teams, ["ENG", "SUP"]);
        assert_eq!(cfg.linear.triage_states, ["triage", "backlog", "unstarted", "started"]);
        assert_eq!(cfg.routes.len(), 2);
        assert_eq!(cfg.routes[1].when.label, None);
        assert_eq!(cfg.notify.linear, [NoticeKind::TriageReady, NoticeKind::FixFailed]);
        assert_eq!(cfg.notify.slack_detail, SlackDetail::Full);
        assert_eq!(cfg.debounce, Debounce { issue_secs: 5, push_index_secs: 60, thread_refresh_secs: 600 });
        let json = serde_json::to_value(&cfg).unwrap();
        assert_eq!(serde_json::from_value::<IntakeConfig>(json).unwrap(), cfg);
    }

    #[test]
    fn empty_settings_object_is_default() {
        assert_eq!(serde_json::from_str::<IntegrationSettings>("{}").unwrap(), IntegrationSettings::default());
        assert_eq!(serde_json::to_string(&IntegrationSettings::default()).unwrap(), "{}");
    }
}
