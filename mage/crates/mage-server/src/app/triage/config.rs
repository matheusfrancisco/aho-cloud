//! Triage server config (`[server.triage]`), the org-settings clamp, and the harness policy
//! (triage.md "Harness choice", "Budget defaults").

use std::time::Duration;

use serde::Deserialize;

use mage_core::domain::budget::duration_ms;
use mage_core::domain::{Budget, HarnessKind};
use mage_core::wire::api::TriageSettings;

/// Upper bound of `TriageSpec.max_causes` (the `submit_triage` schema's `causes.maxItems`).
pub const MAX_CAUSES: u8 = 5;

/// `[server.triage]`. Every field but `default_model` has the triage.md default.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TriageConfig {
    /// Model used when the org's `TriageSettings.model` is `None`.
    pub default_model: String,
    /// Org budgets are clamped field by field to this ceiling.
    #[serde(default = "default_budget_ceiling")]
    pub budget_ceiling: Budget,
    /// `Files` not `Ready` this long after `RepoVersion.created_at` → `Skipped(IndexTimeout)`.
    #[serde(default = "default_files_wait", rename = "files_wait_ms", with = "duration_ms")]
    pub files_wait: Duration,
    /// Deferral step while waiting on index layers.
    #[serde(default = "default_defer_step", rename = "defer_step_ms", with = "duration_ms")]
    pub defer_step: Duration,
    /// Timeout of server-side candidate seeding.
    #[serde(default = "default_seed_timeout", rename = "seed_timeout_ms", with = "duration_ms")]
    pub seed_timeout: Duration,
    /// Candidates requested from the retriever for the prompt.
    #[serde(default = "default_seed_candidates")]
    pub seed_candidates: u32,
    /// Delay of the single automatic `Requeue` after an `Errored` run.
    #[serde(default = "default_requeue_delay", rename = "requeue_delay_ms", with = "duration_ms")]
    pub requeue_delay: Duration,
    /// Production (`server.environment = "production"`): `ClaudeCode` needs `claude_code_gateway_verified`.
    #[serde(default)]
    pub production: bool,
    /// `[server.harness.claude_code] gateway_verified`.
    #[serde(default)]
    pub claude_code_gateway_verified: bool,
}

fn default_budget_ceiling() -> Budget {
    Budget { max_wall: Duration::from_secs(20 * 60), max_turns: 100, max_cost_micros: 10_000_000, max_gate_attempts: 3 }
}

fn default_files_wait() -> Duration {
    Duration::from_secs(30 * 60)
}

fn default_defer_step() -> Duration {
    Duration::from_secs(30)
}

fn default_seed_timeout() -> Duration {
    Duration::from_secs(10)
}

fn default_seed_candidates() -> u32 {
    20
}

fn default_requeue_delay() -> Duration {
    Duration::from_secs(2 * 60)
}

impl TriageConfig {
    /// triage.md defaults with the given default model.
    pub fn new(default_model: impl Into<String>) -> Self {
        Self {
            default_model: default_model.into(),
            budget_ceiling: default_budget_ceiling(),
            files_wait: default_files_wait(),
            defer_step: default_defer_step(),
            seed_timeout: default_seed_timeout(),
            seed_candidates: default_seed_candidates(),
            requeue_delay: default_requeue_delay(),
            production: false,
            claude_code_gateway_verified: false,
        }
    }

    /// `ClaudeCode` is allowed for triage in development, and in production only once the gateway
    /// path is verified (open question 4).
    pub fn harness_allowed(&self, harness: HarnessKind) -> bool {
        match harness {
            HarnessKind::Omp => true,
            HarnessKind::ClaudeCode => !self.production || self.claude_code_gateway_verified,
        }
    }

    /// The settings the use cases act on: budget fields clamped to `1..=ceiling`, `max_causes` to
    /// `1..=5`, `layer_wait` to at most `files_wait` (waiting longer than the required layer makes
    /// no difference).
    pub fn clamp(&self, mut s: TriageSettings) -> TriageSettings {
        let ceiling = &self.budget_ceiling;
        s.budget = Budget {
            max_wall: s.budget.max_wall.clamp(Duration::from_secs(1), ceiling.max_wall.max(Duration::from_secs(1))),
            max_turns: s.budget.max_turns.clamp(1, ceiling.max_turns.max(1)),
            max_cost_micros: s.budget.max_cost_micros.clamp(1, ceiling.max_cost_micros.max(1)),
            max_gate_attempts: s.budget.max_gate_attempts.clamp(1, ceiling.max_gate_attempts.max(1)),
        };
        s.max_causes = s.max_causes.clamp(1, MAX_CAUSES);
        s.layer_wait = s.layer_wait.min(self.files_wait);
        s
    }

    /// Harness for a live triage run: the org setting, or `Omp` when the setting is not allowed
    /// here. Returns the notice to put on the run when it fell back.
    pub fn choose_harness(&self, setting: HarnessKind) -> (HarnessKind, Option<String>) {
        if self.harness_allowed(setting) {
            (setting, None)
        } else {
            (
                HarnessKind::Omp,
                Some("harness claude_code is not allowed for triage until the model gateway path is verified; using omp".to_owned()),
            )
        }
    }

    pub fn model(&self, settings: &TriageSettings) -> String {
        settings.model.clone().filter(|m| !m.trim().is_empty()).unwrap_or_else(|| self.default_model.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> TriageSettings {
        TriageSettings::default()
    }

    #[test]
    fn clamp_caps_every_budget_field_at_the_ceiling_and_floors_at_one() {
        let cfg = TriageConfig::new("m");
        let mut s = settings();
        s.budget = Budget { max_wall: Duration::from_secs(3600), max_turns: 0, max_cost_micros: 99_000_000, max_gate_attempts: 9 };
        s.max_causes = 0;
        s.layer_wait = Duration::from_secs(3 * 3600);
        let c = cfg.clamp(s);
        assert_eq!(c.budget, Budget { max_wall: Duration::from_secs(1200), max_turns: 1, max_cost_micros: 10_000_000, max_gate_attempts: 3 });
        assert_eq!(c.max_causes, 1);
        assert_eq!(c.layer_wait, cfg.files_wait);

        let mut s = settings();
        s.max_causes = 50;
        assert_eq!(cfg.clamp(s).max_causes, MAX_CAUSES);
    }

    #[test]
    fn clamp_keeps_values_inside_the_ceiling() {
        let cfg = TriageConfig::new("m");
        let s = settings();
        assert_eq!(cfg.clamp(s.clone()), s, "triage.md defaults are inside the ceiling");
    }

    #[test]
    fn claude_code_needs_gateway_verification_only_in_production() {
        let mut cfg = TriageConfig::new("m");
        assert_eq!(cfg.choose_harness(HarnessKind::ClaudeCode), (HarnessKind::ClaudeCode, None));
        cfg.production = true;
        let (h, notice) = cfg.choose_harness(HarnessKind::ClaudeCode);
        assert_eq!(h, HarnessKind::Omp);
        assert!(notice.is_some());
        assert_eq!(cfg.choose_harness(HarnessKind::Omp), (HarnessKind::Omp, None));
        cfg.claude_code_gateway_verified = true;
        assert_eq!(cfg.choose_harness(HarnessKind::ClaudeCode), (HarnessKind::ClaudeCode, None));
    }

    #[test]
    fn model_falls_back_to_the_server_default() {
        let cfg = TriageConfig::new("default-model");
        let mut s = settings();
        s.model = None;
        assert_eq!(cfg.model(&s), "default-model");
        s.model = Some("  ".into());
        assert_eq!(cfg.model(&s), "default-model");
        s.model = Some("org-model".into());
        assert_eq!(cfg.model(&s), "org-model");
    }

    #[test]
    fn config_deserializes_with_defaults() {
        let cfg: TriageConfig = toml::from_str("default_model = \"x\"\nfiles_wait_ms = 60000\n").unwrap();
        assert_eq!(cfg.files_wait, Duration::from_secs(60));
        assert_eq!(cfg.defer_step, Duration::from_secs(30));
        assert_eq!(cfg.budget_ceiling.max_gate_attempts, 3);
        assert!(toml::from_str::<TriageConfig>("files_wait_ms = 1").is_err(), "default_model is required");
    }
}
