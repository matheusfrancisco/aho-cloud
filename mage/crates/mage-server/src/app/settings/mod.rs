//! Org settings (server.md catalog `…/settings*`, `…/integrations*`, UC3, UC5):
//!
//! - [`org`]: harness default, budgets, `TriageSettings`, `FixSettings`.
//! - [`model`]: model route, allowed models, `max_tokens` cap, write-only BYOK key.
//! - [`integrations`]: integration listing, connect (install state), callbacks, revoke.

use serde::Deserialize;

pub mod integrations;
pub mod model;
pub mod org;

pub use integrations::{IntegrationCallback, Integrations};
pub use model::ModelSettingsService;
pub use org::OrgSettingsService;

/// `[server.harness]` (server.md config; triage.md).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct HarnessConfig {
    pub claude_code: ClaudeCodeConfig,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ClaudeCodeConfig {
    /// Claude Code was verified end to end through the model gateway (server.md E2E tests).
    /// Until then the harness is refused in production settings.
    pub gateway_verified: bool,
}
