//! `Forward` (server.md "Request handling", steps 2–10; step 1, the credential header, is HTTP).

use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use futures::Stream;
use parking_lot::Mutex;
use serde::de::{self, Deserializer as _, IgnoredAny, MapAccess, Visitor};

use mage_core::domain::{OrgId, RunError, RunErrorCode, RunId, RunStatus, Usage};
use mage_core::ports::Clock;
use mage_core::wire::api::ModelRouteKind;

use super::usage::{ObservedUsage, UsageTap};
use super::{BYOK_KEY_SECRET, GatewayConfig, GatewayError, ModelPrice, RunMeter};
use crate::app::auth::tokens::TokenService;
use crate::app::runs::UsageVerdict;
use crate::error::AppError;
use crate::ports::audit::{Actor, AuditAction, AuditEvent, AuditLog, AuditOutcome, AuditTarget};
use crate::ports::auth::{TokenKind, TokenScope};
use crate::ports::feed::{ChangeFeed, FeedKind};
use crate::ports::llm::{
    ByteStream, ModelRoute, ModelSettings, ModelUpstream, UpstreamBody, UpstreamError, UpstreamHead, UpstreamOp, UpstreamRequest,
};
use crate::ports::org::OrgStore;
use crate::ports::runs::{RunStore, RunStoreError};
use crate::ports::vault::{SecretName, SecretScope, SecretVault, VaultError};

/// Collaborators of [`Forward`].
pub struct GatewayDeps {
    /// Signature, kind and jti revocation of the `gateway_token`.
    pub tokens: Arc<TokenService>,
    pub runs: Arc<dyn RunStore>,
    pub orgs: Arc<dyn OrgStore>,
    pub vault: Arc<dyn SecretVault>,
    pub upstream: Arc<dyn ModelUpstream>,
    pub meter: Arc<dyn RunMeter>,
    pub audit: Arc<dyn AuditLog>,
    pub feed: Arc<dyn ChangeFeed>,
    pub clock: Arc<dyn Clock>,
}

/// One harness call as the HTTP adapter received it.
pub struct GatewayRequest {
    pub op: UpstreamOp,
    /// `mgg_…` from `x-api-key` or `Authorization: Bearer`.
    pub token: String,
    pub anthropic_version: Option<String>,
    /// `anthropic-beta` values, split on commas.
    pub anthropic_beta: Vec<String>,
    /// Client JSON as received; forwarded unchanged on the Anthropic route.
    pub body: Bytes,
    pub request_id: Option<String>,
}

impl fmt::Debug for GatewayRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GatewayRequest")
            .field("op", &self.op)
            .field("anthropic_version", &self.anthropic_version)
            .field("anthropic_beta", &self.anthropic_beta)
            .field("body_bytes", &self.body.len())
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub struct GatewayResponse {
    /// Upstream status and allowlisted headers.
    pub head: UpstreamHead,
    pub body: GatewayBody,
}

#[derive(Debug)]
pub enum GatewayBody {
    Full(Bytes),
    Stream(Box<MeteredStream>),
}

/// What step 2 established about the caller.
struct RunCall {
    org: OrgId,
    run: RunId,
    models: Vec<String>,
}

pub struct Forward {
    deps: GatewayDeps,
    config: GatewayConfig,
    in_flight: Arc<InFlight>,
}

impl Forward {
    pub fn new(deps: GatewayDeps, config: GatewayConfig) -> Result<Forward, AppError> {
        config.validate()?;
        Ok(Forward { deps, config, in_flight: Arc::new(InFlight::default()) })
    }

    pub fn config(&self) -> &GatewayConfig {
        &self.config
    }

    pub async fn execute(&self, req: GatewayRequest) -> Result<GatewayResponse, GatewayError> {
        let call = self.authorize(&req.token).await?;
        let top = TopLevel::parse(&req.body)?;
        let settings = self.deps.orgs.model_settings(call.org).await.map_err(AppError::from)?;
        let model = self.model_policy(&call, &settings, &top, req.op)?;
        let price = *self
            .config
            .price(&model)
            .ok_or_else(|| GatewayError::Invalid(format!("model {model} has no pricing configured on this server")))?;
        let (route, key_fingerprint) = self.route(call.org, &settings, &model).await?;
        if !self.deps.upstream.supports(&route, req.op) {
            return Err(GatewayError::NotFound(format!("{} is not available on this route", op_path(req.op))));
        }
        let permit =
            self.in_flight.try_acquire(call.run, self.config.per_run_in_flight).ok_or(GatewayError::RateLimited { retry_after_secs: 1 })?;

        let route_kind = route.kind();
        let anthropic_beta = match route_kind {
            ModelRouteKind::Anthropic => req.anthropic_beta,
            ModelRouteKind::Vertex => req.anthropic_beta.into_iter().filter(|b| self.config.beta_allowlist_vertex.contains(b)).collect(),
        };
        let stream = req.op == UpstreamOp::Messages && top.stream;
        let sent_at = std::time::Instant::now();
        let response = self
            .deps
            .upstream
            .send(UpstreamRequest { op: req.op, route, stream, anthropic_version: req.anthropic_version, anthropic_beta, body: req.body })
            .await?;
        tracing::info!(
            org = %call.org,
            run = %call.run,
            model = %model,
            route = ?route_kind,
            stream,
            status = response.head.status,
            ttfb_ms = u64::try_from(sent_at.elapsed().as_millis()).unwrap_or(u64::MAX),
            "gateway: upstream answered"
        );

        if route_kind == ModelRouteKind::Anthropic && matches!(response.head.status, 401 | 403) {
            self.tenant_key_rejected(&call, response.head.status, key_fingerprint, req.request_id).await;
            return Err(GatewayError::Forbidden("tenant model key rejected".into()));
        }

        let settlement = (req.op == UpstreamOp::Messages && response.head.is_success()).then(|| Settlement {
            meter: Arc::clone(&self.deps.meter),
            org: call.org,
            run: call.run,
            model,
            price,
        });
        let body = match response.body {
            UpstreamBody::Full(bytes) => {
                drop(permit);
                if let Some(settlement) = settlement {
                    // Detached: a client disconnect cannot cancel metering.
                    tokio::spawn(settlement.settle(ObservedUsage::from_message(&bytes)));
                }
                GatewayBody::Full(bytes)
            }
            UpstreamBody::Stream(chunks) => GatewayBody::Stream(Box::new(MeteredStream::new(chunks, settlement, permit))),
        };
        Ok(GatewayResponse { head: response.head, body })
    }

    /// Step 2: token, revocation, run state, attempt, budget.
    async fn authorize(&self, token: &str) -> Result<RunCall, GatewayError> {
        let claims = self.deps.tokens.verify(token, TokenKind::Gateway).await.map_err(|e| match e {
            AppError::Unavailable(m) => GatewayError::Unavailable(m),
            AppError::Internal(m) => GatewayError::Internal(m),
            _ => GatewayError::Unauthenticated("invalid gateway token".into()),
        })?;
        let TokenScope::Gateway { org, run, attempt, models } = claims.scope else {
            return Err(GatewayError::Unauthenticated("invalid gateway token".into()));
        };
        let not_active = || GatewayError::Unauthenticated("run is not active".into());
        let record = match self.deps.runs.get(org, run).await {
            Ok(record) => record,
            Err(RunStoreError::NotFound) => return Err(not_active()),
            Err(e) => return Err(AppError::from(e).into()),
        };
        if !matches!(record.run.status, RunStatus::Running | RunStatus::AwaitingGate) || record.run.attempt != attempt {
            return Err(not_active());
        }
        if let Some(limit) = record.effective_usage(self.deps.clock.now()).exceeds(&record.run.budget) {
            return Err(GatewayError::Forbidden(format!("run budget exceeded ({limit})")));
        }
        Ok(RunCall { org, run, models })
    }

    /// Step 4: model ∈ token `models` (empty = org allowlist) ∩ org allowlist; `max_tokens` cap.
    fn model_policy(&self, call: &RunCall, settings: &ModelSettings, top: &TopLevel, op: UpstreamOp) -> Result<String, GatewayError> {
        let model = top.model.clone().ok_or_else(|| GatewayError::Invalid("model is required".into()))?;
        let token_allows = call.models.is_empty() || call.models.contains(&model);
        if !token_allows || !settings.allowed_models.contains(&model) {
            return Err(GatewayError::Invalid(format!("model {model} is not allowed for this run")));
        }
        if op == UpstreamOp::Messages
            && let (Some(cap), Some(requested)) = (settings.max_tokens_cap, top.max_tokens)
            && requested > u64::from(cap)
        {
            return Err(GatewayError::Invalid(format!("max_tokens {requested} exceeds the org cap of {cap}")));
        }
        Ok(model)
    }

    /// Step 5: BYOK key from the vault, or Vertex when the operator enabled it for the org.
    async fn route(&self, org: OrgId, settings: &ModelSettings, model: &str) -> Result<(ModelRoute, Option<String>), GatewayError> {
        match settings.route {
            ModelRouteKind::Vertex => {
                let slug = self.deps.orgs.get_org(org).await.map_err(AppError::from)?.slug;
                let vertex = self
                    .config
                    .vertex
                    .as_ref()
                    .filter(|_| self.config.vertex_enabled_for(&slug))
                    .ok_or_else(|| GatewayError::Forbidden("the Vertex route is not enabled for this org".into()))?;
                let vertex_model =
                    settings.model_map.get(model).ok_or_else(|| GatewayError::Invalid(format!("model {model} has no Vertex mapping")))?;
                let route =
                    ModelRoute::Vertex { project: vertex.project.clone(), region: vertex.region.clone(), model: vertex_model.clone() };
                Ok((route, None))
            }
            ModelRouteKind::Anthropic => {
                let name = SecretName::new(BYOK_KEY_SECRET).map_err(AppError::from)?;
                match self.deps.vault.load(&SecretScope::Org(org), &name).await {
                    Ok((meta, api_key)) => Ok((ModelRoute::AnthropicApi { api_key }, Some(meta.fingerprint))),
                    Err(VaultError::NotFound) => Err(GatewayError::Forbidden("no model key configured".into())),
                    Err(e) => Err(AppError::from(e).into()),
                }
            }
        }
    }

    /// Step 8: the tenant key was rejected upstream. The run ends `Errored` (`ModelAuth`) so it
    /// stops burning budget; Owners see it in the audit log and through the settings feed.
    async fn tenant_key_rejected(&self, call: &RunCall, status: u16, fingerprint: Option<String>, request_id: Option<String>) {
        let error =
            RunError { code: RunErrorCode::ModelAuth, message: format!("the model provider rejected the org's API key (HTTP {status})") };
        if let Err(e) = self.deps.meter.fail_run(call.org, call.run, error).await {
            tracing::error!(org = %call.org, run = %call.run, error = %e, "gateway: fail_run after key rejection failed");
        }
        let mut details = vec![(Cow::Borrowed("run"), call.run.to_string()), (Cow::Borrowed("upstream_status"), status.to_string())];
        if let Some(fingerprint) = fingerprint {
            details.push((Cow::Borrowed("fingerprint"), fingerprint));
        }
        let event = AuditEvent {
            org: Some(call.org),
            actor: Actor::System,
            action: AuditAction::ByokKeyRejected,
            target: AuditTarget::new("secret", BYOK_KEY_SECRET),
            outcome: AuditOutcome::Failed,
            ip: None,
            request_id,
            details,
        };
        if let Err(e) = self.deps.audit.record(event).await {
            tracing::error!(org = %call.org, error = %e, "gateway: audit byok.key_rejected failed");
        }
        if let Err(e) = self.deps.feed.append(call.org, FeedKind::Settings, &call.org.to_string()).await {
            tracing::warn!(org = %call.org, error = %e, "gateway: settings feed notice failed");
        }
    }
}

fn op_path(op: UpstreamOp) -> &'static str {
    match op {
        UpstreamOp::Messages => "/v1/messages",
        UpstreamOp::CountTokens => "/v1/messages/count_tokens",
    }
}

/// Step 3: the top-level fields the gateway reads. The body must be one JSON object without
/// duplicate top-level keys (a duplicate `model` could otherwise pass the policy as one value and
/// reach the provider as another); nested values are skipped, not parsed.
#[derive(Debug, Default, PartialEq, Eq)]
struct TopLevel {
    model: Option<String>,
    stream: bool,
    max_tokens: Option<u64>,
}

impl TopLevel {
    fn parse(body: &[u8]) -> Result<TopLevel, GatewayError> {
        let invalid = |e: serde_json::Error| GatewayError::Invalid(format!("request body: {e}"));
        let mut de = serde_json::Deserializer::from_slice(body);
        let top = de.deserialize_map(TopLevelVisitor).map_err(invalid)?;
        de.end().map_err(invalid)?;
        Ok(top)
    }
}

struct TopLevelVisitor;

impl<'de> Visitor<'de> for TopLevelVisitor {
    type Value = TopLevel;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON object")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<TopLevel, A::Error> {
        let mut seen = BTreeSet::new();
        let mut top = TopLevel::default();
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "model" => top.model = Some(map.next_value::<String>()?),
                "stream" => top.stream = map.next_value::<Option<bool>>()?.unwrap_or(false),
                "max_tokens" => top.max_tokens = map.next_value::<Option<u64>>()?,
                _ => {
                    map.next_value::<IgnoredAny>()?;
                }
            }
            if !seen.insert(key) {
                return Err(de::Error::custom("duplicate top-level field"));
            }
        }
        Ok(top)
    }
}

/// Per-run in-flight counter for this instance (step 6).
#[derive(Default)]
struct InFlight {
    counts: Mutex<HashMap<RunId, u32>>,
}

impl InFlight {
    fn try_acquire(self: &Arc<Self>, run: RunId, cap: u32) -> Option<InFlightPermit> {
        let mut counts = self.counts.lock();
        let count = counts.entry(run).or_insert(0);
        if *count >= cap {
            return None;
        }
        *count += 1;
        Some(InFlightPermit { in_flight: Arc::clone(self), run })
    }
}

struct InFlightPermit {
    in_flight: Arc<InFlight>,
    run: RunId,
}

impl Drop for InFlightPermit {
    fn drop(&mut self) {
        let mut counts = self.in_flight.counts.lock();
        if let Some(count) = counts.get_mut(&self.run) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                counts.remove(&self.run);
            }
        }
    }
}

/// Step 10: observed usage → delta → `RecordUsage`.
struct Settlement {
    meter: Arc<dyn RunMeter>,
    org: OrgId,
    run: RunId,
    /// The requested (Anthropic) model id; prices are keyed by it.
    model: String,
    price: ModelPrice,
}

impl Settlement {
    async fn settle(self, observed: ObservedUsage) {
        let delta = observed.usage(&self.price);
        tracing::info!(
            org = %self.org,
            run = %self.run,
            model = %self.model,
            input_tokens = delta.input_tokens,
            output_tokens = delta.output_tokens,
            cost_micros = delta.cost_micros,
            estimated = observed.estimated,
            completed = observed.completed,
            "gateway: model call metered"
        );
        if delta == Usage::default() {
            return;
        }
        match self.meter.record_usage(self.org, self.run, delta).await {
            Ok(UsageVerdict::Allow) => {}
            // The run manager owns the TimedOut transition and Abort; the next call fails at
            // step 2 because the run is terminal or over budget.
            Ok(UsageVerdict::Deny(reason)) => tracing::info!(org = %self.org, run = %self.run, %reason, "gateway: usage denied"),
            Err(e) => tracing::error!(org = %self.org, run = %self.run, error = %e, "gateway: record_usage failed; call unmetered"),
        }
    }
}

/// The upstream stream as forwarded to the client: every chunk passes through unchanged while
/// [`UsageTap`] reads it. On end, upstream error or drop (client disconnect), the tap state is
/// handed to a spawned settle task and the in-flight slot is released.
pub struct MeteredStream {
    inner: ByteStream,
    tap: UsageTap,
    settlement: Option<Settlement>,
    permit: Option<InFlightPermit>,
    runtime: tokio::runtime::Handle,
}

impl MeteredStream {
    fn new(inner: ByteStream, settlement: Option<Settlement>, permit: InFlightPermit) -> MeteredStream {
        MeteredStream { inner, tap: UsageTap::new(), settlement, permit: Some(permit), runtime: tokio::runtime::Handle::current() }
    }

    fn finish(&mut self) {
        self.permit = None;
        if let Some(settlement) = self.settlement.take() {
            self.runtime.spawn(settlement.settle(self.tap.observe()));
        }
    }
}

impl fmt::Debug for MeteredStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MeteredStream").field("settled", &self.settlement.is_none()).finish_non_exhaustive()
    }
}

impl Stream for MeteredStream {
    type Item = Result<Bytes, UpstreamError>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        match this.inner.as_mut().poll_next(cx) {
            Poll::Ready(Some(Ok(chunk))) => {
                this.tap.feed(&chunk);
                Poll::Ready(Some(Ok(chunk)))
            }
            Poll::Ready(Some(Err(e))) => {
                this.finish();
                Poll::Ready(Some(Err(e)))
            }
            Poll::Ready(None) => {
                this.finish();
                Poll::Ready(None)
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Drop for MeteredStream {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_level_reads_only_the_policy_fields() {
        let top = TopLevel::parse(br#"{"model":"m","max_tokens":1024,"stream":true,"messages":[{"role":"user","content":"x"}]}"#).unwrap();
        assert_eq!(top, TopLevel { model: Some("m".into()), stream: true, max_tokens: Some(1024) });
        let top = TopLevel::parse(br#"{"model":"m","stream":null}"#).unwrap();
        assert!(!top.stream);
    }

    #[test]
    fn top_level_rejects_non_objects_duplicates_and_trailing_bytes() {
        for body in [
            &br#"["m", true, 5]"#[..],
            br#"{"model":"allowed","model":"other"}"#,
            br#"{"model":"m"} {"model":"n"}"#,
            br#"{"model":5}"#,
            br#"{"max_tokens":-1}"#,
            b"",
        ] {
            assert!(matches!(TopLevel::parse(body), Err(GatewayError::Invalid(_))), "{}", String::from_utf8_lossy(body));
        }
    }

    #[test]
    fn in_flight_cap_is_per_run_and_released_on_drop() {
        let in_flight = Arc::new(InFlight::default());
        let a = RunId::from_uuid(uuid::Uuid::from_u128(1));
        let b = RunId::from_uuid(uuid::Uuid::from_u128(2));
        let first = in_flight.try_acquire(a, 2).unwrap();
        let _second = in_flight.try_acquire(a, 2).unwrap();
        assert!(in_flight.try_acquire(a, 2).is_none(), "third call for the same run is over the cap");
        assert!(in_flight.try_acquire(b, 2).is_some(), "other runs are independent");
        drop(first);
        assert!(in_flight.try_acquire(a, 2).is_some(), "a finished call frees its slot");
    }
}
