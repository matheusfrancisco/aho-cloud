//! `GET`/`PUT …/settings`: non-secret org settings (general, triage, fix).

use std::sync::Arc;

use mage_core::domain::{Budget, HarnessKind, OrgId, UserId};
use mage_core::wire::api::{GeneralSettings, OrgSettings, TriageSettings};

use crate::app::auth::{RequestMeta, audit, event};
use crate::app::settings::HarnessConfig;
use crate::error::AppError;
use crate::ports::audit::{Actor, AuditAction, AuditLog, AuditTarget};
use crate::ports::org::OrgStore;

/// triage.md `TriageSpec.max_causes`: 1..=5.
const MAX_CAUSES: std::ops::RangeInclusive<u8> = 1..=5;

pub struct OrgSettingsService {
    orgs: Arc<dyn OrgStore>,
    audit: Arc<dyn AuditLog>,
    harness: HarnessConfig,
    production: bool,
}

impl OrgSettingsService {
    /// `production`: `environment = "production"` (gates the ClaudeCode harness).
    pub fn new(orgs: Arc<dyn OrgStore>, audit: Arc<dyn AuditLog>, harness: HarnessConfig, production: bool) -> OrgSettingsService {
        OrgSettingsService { orgs, audit, harness, production }
    }

    pub async fn get(&self, org: OrgId) -> Result<OrgSettings, AppError> {
        Ok(OrgSettings {
            general: self.orgs.general_settings(org).await?,
            triage: self.orgs.triage_settings(org).await?,
            fix: self.orgs.fix_settings(org).await?,
        })
    }

    /// Validates, writes the changed areas, audits `settings.changed` with the area names.
    pub async fn put(&self, org: OrgId, user: UserId, next: &OrgSettings, meta: &RequestMeta) -> Result<OrgSettings, AppError> {
        self.validate(next)?;
        let current = self.get(org).await?;
        let mut changed = Vec::new();
        if current.general != next.general {
            self.orgs.set_general_settings(org, &next.general).await?;
            changed.push("general");
        }
        if current.triage != next.triage {
            self.orgs.set_triage_settings(org, &next.triage).await?;
            changed.push("triage");
        }
        if current.fix != next.fix {
            self.orgs.set_fix_settings(org, &next.fix).await?;
            changed.push("fix");
        }
        if !changed.is_empty() {
            audit(
                self.audit.as_ref(),
                event(Some(org), Actor::User(user), AuditAction::SettingsChanged, AuditTarget::new("settings", org.to_string()), meta)
                    .detail("areas", changed.join(",")),
            )
            .await?;
        }
        Ok(next.clone())
    }

    fn validate(&self, s: &OrgSettings) -> Result<(), AppError> {
        self.check_harness(s.general.harness)?;
        self.check_harness(s.triage.harness)?;
        check_general(&s.general)?;
        check_triage(&s.triage)?;
        s.fix.validate()?;
        Ok(())
    }

    /// triage.md: ClaudeCode is refused in production until the gateway path is verified.
    fn check_harness(&self, harness: HarnessKind) -> Result<(), AppError> {
        if harness == HarnessKind::ClaudeCode && self.production && !self.harness.claude_code.gateway_verified {
            return Err(AppError::Invalid("the claude_code harness is not enabled on this server (harness.claude_code.gateway_verified)".into()));
        }
        Ok(())
    }
}

fn check_budget(area: &str, b: &Budget) -> Result<(), AppError> {
    if b.max_wall.is_zero() || b.max_turns == 0 || b.max_cost_micros == 0 || b.max_gate_attempts == 0 {
        return Err(AppError::Invalid(format!("{area} budget: every limit must be positive")));
    }
    Ok(())
}

fn check_general(g: &GeneralSettings) -> Result<(), AppError> {
    check_budget("learn", &g.budgets.learn)?;
    check_budget("diagnose", &g.budgets.diagnose)?;
    check_budget("fix", &g.budgets.fix)
}

fn check_triage(t: &TriageSettings) -> Result<(), AppError> {
    check_budget("triage", &t.budget)?;
    if !MAX_CAUSES.contains(&t.max_causes) {
        return Err(AppError::Invalid("triage max_causes must be 1..=5".into()));
    }
    if t.model.as_deref().is_some_and(|m| m.trim().is_empty()) {
        return Err(AppError::Invalid("triage model must be non-empty when set".into()));
    }
    Ok(())
}
