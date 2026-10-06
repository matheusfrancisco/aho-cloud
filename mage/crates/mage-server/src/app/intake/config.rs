//! Process-level intake configuration (`[server.intake]`, `[server.github] installations`) and the
//! idempotent start-up seeding of internal v1 (intake.md "Configuration", use case 6).

use std::collections::BTreeMap;
use std::time::Duration;

use serde::Deserialize;

use crate::domain::intake::IntakeConfig;

/// `"webhook"`: providers push; `"poll"`: no public URL (local dev), Linear resync every minute.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntakeMode {
    #[default]
    Webhook,
    Poll,
}

impl IntakeMode {
    /// Period of the `intake.linear_resync` chain.
    pub fn linear_resync_every(self) -> Duration {
        match self {
            Self::Webhook => Duration::from_secs(15 * 60),
            Self::Poll => Duration::from_secs(60),
        }
    }
}

/// `[server.intake]`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IntakeSection {
    pub mode: IntakeMode,
    /// Org slug → per-org config, written with `OrgStore::put_intake_config` at start.
    pub orgs: BTreeMap<String, IntakeConfig>,
}

impl IntakeSection {
    /// Accepted age of Linear's signed `webhookTimestamp` for the `/hooks/linear` handler, which
    /// verifies before the tenant is known: the largest per-org `max_skew_secs` (60 s if none).
    pub fn linear_max_skew(&self) -> Duration {
        let secs = self.orgs.values().map(|c| c.linear.max_skew_secs).max().unwrap_or(60);
        Duration::from_secs(secs)
    }
}

/// One entry of `[server.github] installations = [{ id, org }]` (internal v1: bound without the
/// install callback).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GithubInstallSeed {
    pub id: u64,
    /// Org slug.
    pub org: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn section_parses_and_derives_skew() {
        let section: IntakeSection = toml::from_str(
            r#"
            mode = "poll"
            [orgs.hoop.linear]
            teams = ["ENG"]
            max_skew_secs = 120
            [orgs.other.linear]
            teams = ["X"]
            "#,
        )
        .unwrap();
        assert_eq!(section.mode, IntakeMode::Poll);
        assert_eq!(section.linear_max_skew(), Duration::from_secs(120));
        assert_eq!(IntakeSection::default().linear_max_skew(), Duration::from_secs(60));
        assert_eq!(section.mode.linear_resync_every(), Duration::from_secs(60));
    }
}
