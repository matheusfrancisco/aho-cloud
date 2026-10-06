//! Org settings, model route and BYOK key, integrations (server.md catalog; triage.md
//! `TriageSettings`; fix.md `FixSettings`; storage.md `org_settings`).
//!
//! `GeneralSettings` and `TriageSettings` are the serde types of the `org_settings` rows
//! (`OrgStore`), so the server stores and serves these exact types.

use std::time::Duration;

use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use ts_rs::TS;

use crate::domain::budget::duration_ms;
use crate::domain::fix::FixSettings;
use crate::domain::{Budget, HarnessKind, Integration, IntegrationKind};
use crate::wire::api::AuditActor;

/// Default run budgets per harness kind the UI starts (runs.md "Default budgets"); Triage has its
/// own in [`TriageSettings`], Verify budgets are fixed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct RunBudgets {
    pub learn: Budget,
    pub diagnose: Budget,
    pub fix: Budget,
}

/// Harness default and budgets (`org_settings` area `general`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct GeneralSettings {
    pub harness: HarnessKind,
    pub budgets: RunBudgets,
}

/// Per-org triage policy (triage.md Additions; `org_settings` area `triage`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct TriageSettings {
    /// Triage new and changed tickets without a user request.
    pub auto_triage: bool,
    pub harness: HarnessKind,
    /// `None`: server default triage model.
    pub model: Option<String>,
    pub budget: Budget,
    pub max_causes: u8,
    /// How long triage waits for preferred layers before running degraded.
    #[serde(rename = "layer_wait_ms", with = "duration_ms")]
    #[ts(type = "number")]
    pub layer_wait: Duration,
    pub auto_retriage_per_day: u8,
    pub ticket_cost_cap_micros: u64,
}

const fn budget(wall_min: u64, turns: u32, cost_micros: u64, gate_attempts: u8) -> Budget {
    Budget {
        max_wall: Duration::from_secs(wall_min * 60),
        max_turns: turns,
        max_cost_micros: cost_micros,
        max_gate_attempts: gate_attempts,
    }
}

/// runs.md "Default budgets per kind".
impl Default for RunBudgets {
    fn default() -> Self {
        RunBudgets { learn: budget(60, 200, 20_000_000, 3), diagnose: budget(45, 150, 15_000_000, 2), fix: budget(45, 150, 15_000_000, 3) }
    }
}

/// Default harness is omp (triage.md "Harness choice").
impl Default for GeneralSettings {
    fn default() -> Self {
        GeneralSettings { harness: HarnessKind::Omp, budgets: RunBudgets::default() }
    }
}

/// triage.md "Budget defaults" and Additions: auto triage on, omp, server default model, 8 min /
/// 40 turns / USD 2 / 2 gate attempts, 5 causes, 10 min layer wait, 3 auto retriages per day,
/// USD 10 per ticket.
impl Default for TriageSettings {
    fn default() -> Self {
        TriageSettings {
            auto_triage: true,
            harness: HarnessKind::Omp,
            model: None,
            budget: budget(8, 40, 2_000_000, 2),
            max_causes: 5,
            layer_wait: Duration::from_secs(10 * 60),
            auto_retriage_per_day: 3,
            ticket_cost_cap_micros: 10_000_000,
        }
    }
}

/// `GET …/settings` and the body of `PUT …/settings` (non-secret settings only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct OrgSettings {
    pub general: GeneralSettings,
    pub triage: TriageSettings,
    pub fix: FixSettings,
}

/// Where the gateway sends model calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ModelRouteKind {
    /// Anthropic API with the org's BYOK key.
    Anthropic,
    /// Mage's Vertex project (only if the operator enabled it for the org).
    Vertex,
}

/// Display-safe metadata of the stored BYOK key; the key itself is write-only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ModelKeyMeta {
    /// e.g. `sk-ant-…a1b2`.
    pub fingerprint: String,
    #[serde(with = "time::serde::rfc3339")]
    #[ts(type = "string")]
    pub set_at: OffsetDateTime,
    pub set_by: AuditActor,
}

/// `GET …/settings/model` (Admin).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ModelSettingsView {
    pub route: ModelRouteKind,
    /// Model ids the gateway accepts (empty = none configured).
    pub allowed_models: Vec<String>,
    pub max_tokens_cap: Option<u32>,
    /// The operator enabled Vertex for this org.
    pub vertex_available: bool,
    pub key: Option<ModelKeyMeta>,
}

/// `PUT …/settings/model` (Owner). Response: `ModelSettingsView`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct ModelSettingsUpdate {
    pub route: ModelRouteKind,
    pub allowed_models: Vec<String>,
    #[serde(default)]
    #[ts(optional)]
    pub max_tokens_cap: Option<u32>,
}

/// `PUT …/settings/model/key` (Owner): validated upstream before it is stored. Response:
/// `ModelKeyMeta`. `Debug` never prints the key.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(deny_unknown_fields)]
pub struct SetModelKeyRequest {
    pub api_key: String,
}

impl std::fmt::Debug for SetModelKeyRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SetModelKeyRequest").field("api_key", &"<redacted>").finish()
    }
}

/// `GET …/integrations` (Admin).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct IntegrationList {
    pub integrations: Vec<Integration>,
}

/// `POST …/integrations/{kind}/connect`: the provider URL to send the browser to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ConnectIntegrationResponse {
    pub url: String,
}

/// `{kind}` path segment of the integration routes (`github`, `github-fix`, `linear`, `slack`).
pub const fn integration_slug(kind: IntegrationKind) -> &'static str {
    match kind {
        IntegrationKind::GitHub => "github",
        IntegrationKind::GitHubFix => "github-fix",
        IntegrationKind::Linear => "linear",
        IntegrationKind::Slack => "slack",
    }
}

/// Inverse of [`integration_slug`].
pub fn integration_from_slug(slug: &str) -> Option<IntegrationKind> {
    [IntegrationKind::GitHub, IntegrationKind::GitHubFix, IntegrationKind::Linear, IntegrationKind::Slack]
        .into_iter()
        .find(|k| integration_slug(*k) == slug)
}
