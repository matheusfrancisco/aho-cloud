//! Model route and BYOK key (server.md `…/settings/model*`, UC5). The key is write-only: it is
//! validated upstream with a cheap `count_tokens` call, stored as the org's Active
//! `model/anthropic_api_key` version, and previous versions are destroyed.

use std::collections::BTreeSet;
use std::sync::Arc;

use bytes::Bytes;

use mage_core::domain::{OrgId, UserId};
use mage_core::wire::api::{AuditActor, ModelKeyMeta, ModelRouteKind, ModelSettingsUpdate, ModelSettingsView};

use crate::app::auth::{RequestMeta, audit, event};
use crate::app::gateway::{BYOK_KEY_SECRET, GatewayConfig};
use crate::error::AppError;
use crate::ports::audit::{Actor, AuditAction, AuditLog, AuditOutcome, AuditTarget};
use crate::ports::llm::{ModelRoute, ModelSettings, ModelUpstream, UpstreamOp, UpstreamRequest};
use crate::ports::org::OrgStore;
use crate::ports::vault::{Activation, SecretBytes, SecretMeta, SecretName, SecretScope, SecretState, SecretVault};

/// Model used for the key probe when the org allows none yet: authentication is checked before
/// the model, so an unknown model still distinguishes a rejected key (401/403).
const PROBE_FALLBACK_MODEL: &str = "claude-3-5-haiku-latest";
const KEY_PREFIX: &str = "sk-ant-";
const KEY_LEN: std::ops::RangeInclusive<usize> = 32..=256;

pub struct ModelSettingsService {
    orgs: Arc<dyn OrgStore>,
    vault: Arc<dyn SecretVault>,
    upstream: Arc<dyn ModelUpstream>,
    audit: Arc<dyn AuditLog>,
    gateway: GatewayConfig,
}

impl ModelSettingsService {
    pub fn new(
        orgs: Arc<dyn OrgStore>,
        vault: Arc<dyn SecretVault>,
        upstream: Arc<dyn ModelUpstream>,
        audit: Arc<dyn AuditLog>,
        gateway: GatewayConfig,
    ) -> ModelSettingsService {
        ModelSettingsService { orgs, vault, upstream, audit, gateway }
    }

    fn key_name() -> Result<SecretName, AppError> {
        Ok(SecretName::new(BYOK_KEY_SECRET)?)
    }

    /// Metadata of the Active BYOK key, without decrypting it.
    async fn key_meta(&self, org: OrgId) -> Result<Option<ModelKeyMeta>, AppError> {
        let versions = self.vault.versions(&SecretScope::Org(org), &Self::key_name()?).await?;
        Ok(versions.into_iter().find(|m| m.state == SecretState::Active).map(key_view))
    }

    pub async fn view(&self, org: OrgId) -> Result<ModelSettingsView, AppError> {
        let settings = self.orgs.model_settings(org).await?;
        let slug = self.orgs.get_org(org).await?.slug;
        Ok(ModelSettingsView {
            route: settings.route,
            allowed_models: settings.allowed_models,
            max_tokens_cap: settings.max_tokens_cap,
            vertex_available: self.gateway.vertex_enabled_for(&slug),
            key: self.key_meta(org).await?,
        })
    }

    /// `PUT …/settings/model` (Owner): every allowed model needs a price; Vertex only when the
    /// operator enabled it for the org and every allowed model has a Vertex mapping.
    pub async fn update(&self, org: OrgId, user: UserId, update: &ModelSettingsUpdate, meta: &RequestMeta) -> Result<ModelSettingsView, AppError> {
        let current = self.orgs.model_settings(org).await?;
        let slug = self.orgs.get_org(org).await?.slug;
        let mut seen = BTreeSet::new();
        for model in &update.allowed_models {
            if model.trim().is_empty() || model.trim() != model {
                return Err(AppError::Invalid("model ids must be non-empty and untrimmed".into()));
            }
            if !seen.insert(model.as_str()) {
                return Err(AppError::Invalid(format!("model {model:?} listed twice")));
            }
            if self.gateway.price(model).is_none() {
                return Err(AppError::Invalid(format!("model {model:?} has no price configured on this server")));
            }
            if update.route == ModelRouteKind::Vertex && !current.model_map.contains_key(model) {
                return Err(AppError::Invalid(format!("model {model:?} has no Vertex model mapping")));
            }
        }
        if update.route == ModelRouteKind::Vertex && !self.gateway.vertex_enabled_for(&slug) {
            return Err(AppError::Invalid("the Vertex route is not enabled for this org".into()));
        }
        if update.max_tokens_cap == Some(0) {
            return Err(AppError::Invalid("max_tokens_cap must be positive".into()));
        }
        let next = ModelSettings {
            route: update.route,
            allowed_models: update.allowed_models.clone(),
            model_map: current.model_map.clone(),
            max_tokens_cap: update.max_tokens_cap,
        };
        if next != current {
            self.orgs.set_model_settings(org, &next).await?;
            let action = if next.route == current.route { AuditAction::SettingsChanged } else { AuditAction::ModelRouteChanged };
            audit(
                self.audit.as_ref(),
                event(Some(org), Actor::User(user), action, AuditTarget::new("settings", org.to_string()), meta)
                    .detail("areas", "model")
                    .detail("route", route_name(next.route))
                    .detail("allowed_models", next.allowed_models.join(",")),
            )
            .await?;
        }
        self.view(org).await
    }

    /// UC5: sanity check, upstream validation (rejected → 422, not stored), store as Active,
    /// destroy every previous version, audit `byok.key_set` (fingerprint only).
    pub async fn set_key(&self, org: OrgId, user: UserId, api_key: &str, meta: &RequestMeta) -> Result<ModelKeyMeta, AppError> {
        let key = api_key.trim();
        if !key.starts_with(KEY_PREFIX) || !KEY_LEN.contains(&key.len()) || !key.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(AppError::Invalid(format!("not an Anthropic API key (expected {KEY_PREFIX}…)")));
        }
        let key = SecretBytes::from(key.to_owned());
        let fingerprint = key.fingerprint();
        let allowed = self.orgs.model_settings(org).await?.allowed_models;
        let probe_model = allowed.first().map_or(PROBE_FALLBACK_MODEL, String::as_str);
        let body = serde_json::to_vec(&serde_json::json!({
            "model": probe_model,
            "messages": [{ "role": "user", "content": "ping" }],
        }))
        .map_err(|e| AppError::Internal(e.to_string()))?;
        let probe = UpstreamRequest {
            op: UpstreamOp::CountTokens,
            route: ModelRoute::AnthropicApi { api_key: key.clone() },
            stream: false,
            anthropic_version: None,
            anthropic_beta: Vec::new(),
            body: Bytes::from(body),
        };
        let response = self.upstream.send(probe).await.map_err(|e| AppError::Unavailable(format!("could not validate the key: {e}")))?;
        let status = response.head.status;
        drop(response);
        if status == 401 || status == 403 {
            let mut rejected = event(Some(org), Actor::User(user), AuditAction::ByokKeyRejected, AuditTarget::new("secret", BYOK_KEY_SECRET), meta)
                .detail("fingerprint", fingerprint)
                .detail("status", status.to_string());
            rejected.0.outcome = AuditOutcome::Denied;
            audit(self.audit.as_ref(), rejected).await?;
            return Err(AppError::Invalid("key rejected by Anthropic".into()));
        }
        if status == 429 || status >= 500 {
            return Err(AppError::Unavailable(format!("could not validate the key now (upstream status {status})")));
        }

        let scope = SecretScope::Org(org);
        let name = Self::key_name()?;
        let actor = Actor::User(user);
        let stored = self.vault.put(&scope, &name, key, Activation::Now, &actor).await?;
        self.destroy_versions(&scope, &name, Some(stored.version), &actor).await?;
        audit(
            self.audit.as_ref(),
            event(Some(org), actor, AuditAction::ByokKeySet, AuditTarget::new("secret", BYOK_KEY_SECRET), meta)
                .detail("fingerprint", stored.fingerprint.clone())
                .detail("version", stored.version.to_string()),
        )
        .await?;
        Ok(key_view(stored))
    }

    /// `DELETE …/settings/model/key` (Owner): crypto-shred every version.
    pub async fn delete_key(&self, org: OrgId, user: UserId, meta: &RequestMeta) -> Result<(), AppError> {
        let scope = SecretScope::Org(org);
        let name = Self::key_name()?;
        let actor = Actor::User(user);
        let destroyed = self.destroy_versions(&scope, &name, None, &actor).await?;
        if destroyed.is_empty() {
            return Ok(());
        }
        let fingerprints: Vec<String> = destroyed.iter().map(|m| m.fingerprint.clone()).collect();
        audit(
            self.audit.as_ref(),
            event(Some(org), actor, AuditAction::ByokKeyDeleted, AuditTarget::new("secret", BYOK_KEY_SECRET), meta).detail("fingerprints", fingerprints.join(",")),
        )
        .await
    }

    /// Destroys every non-destroyed version except `keep` (Active → Retired → Destroyed).
    async fn destroy_versions(
        &self,
        scope: &SecretScope,
        name: &SecretName,
        keep: Option<crate::ports::vault::SecretVersion>,
        actor: &Actor,
    ) -> Result<Vec<SecretMeta>, AppError> {
        let mut destroyed = Vec::new();
        for meta in self.vault.versions(scope, name).await? {
            if Some(meta.version) == keep || meta.state == SecretState::Destroyed {
                continue;
            }
            if meta.state == SecretState::Active {
                self.vault.set_state(scope, name, meta.version, SecretState::Retired, actor).await?;
            }
            destroyed.push(self.vault.set_state(scope, name, meta.version, SecretState::Destroyed, actor).await?);
        }
        Ok(destroyed)
    }
}

fn key_view(meta: SecretMeta) -> ModelKeyMeta {
    ModelKeyMeta { fingerprint: meta.fingerprint, set_at: meta.created_at, set_by: AuditActor::from(&meta.created_by) }
}

fn route_name(route: ModelRouteKind) -> &'static str {
    match route {
        ModelRouteKind::Anthropic => "anthropic",
        ModelRouteKind::Vertex => "vertex",
    }
}
