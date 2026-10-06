//! Probes (server.md "Probes, static, keys"): `/health` touches nothing; `/ready` checks every
//! registered probe (DB ping, key ring loaded, org-feed listener, migrations) and answers
//! `ok` / `not ready` without details. Cloud Run reserves paths ending in `z`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use axum::Router;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;

/// One readiness condition.
#[async_trait]
pub trait ReadyProbe: Send + Sync {
    fn name(&self) -> &'static str;
    async fn ready(&self) -> bool;
}

/// A probe over a synchronous check (e.g. `move || signer.is_ready()`).
pub struct FnProbe<F> {
    name: &'static str,
    check: F,
}

impl<F: Fn() -> bool + Send + Sync> FnProbe<F> {
    pub fn new(name: &'static str, check: F) -> FnProbe<F> {
        FnProbe { name, check }
    }
}

#[async_trait]
impl<F: Fn() -> bool + Send + Sync> ReadyProbe for FnProbe<F> {
    fn name(&self) -> &'static str {
        self.name
    }

    async fn ready(&self) -> bool {
        (self.check)()
    }
}

/// DB ping and migrations at the binary's version (`mage_infra::pg::pool::check_schema`).
pub struct PgReady {
    pool: sqlx::PgPool,
}

impl PgReady {
    pub fn new(pool: sqlx::PgPool) -> PgReady {
        PgReady { pool }
    }
}

#[async_trait]
impl ReadyProbe for PgReady {
    fn name(&self) -> &'static str {
        "database"
    }

    async fn ready(&self) -> bool {
        match mage_infra::pg::pool::check_schema(&self.pool).await {
            Ok(_) => true,
            Err(e) => {
                tracing::warn!(error = %e, "database not ready");
                false
            }
        }
    }
}

#[derive(Clone)]
pub struct HealthState {
    probes: Arc<Vec<Arc<dyn ReadyProbe>>>,
    draining: Arc<AtomicBool>,
}

impl HealthState {
    pub fn new(probes: Vec<Arc<dyn ReadyProbe>>) -> HealthState {
        HealthState { probes: Arc::new(probes), draining: Arc::new(AtomicBool::new(false)) }
    }

    /// Shutdown step 1: `/ready` fails from now on.
    pub fn mark_draining(&self) {
        self.draining.store(true, Ordering::SeqCst);
    }

    pub async fn is_ready(&self) -> bool {
        if self.draining.load(Ordering::SeqCst) {
            return false;
        }
        for probe in self.probes.iter() {
            if !probe.ready().await {
                tracing::warn!(probe = probe.name(), "not ready");
                return false;
            }
        }
        true
    }
}

pub fn router(state: HealthState) -> Router {
    Router::new().route("/health", get(health)).route("/ready", get(ready)).with_state(state)
}

async fn health() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

async fn ready(State(state): State<HealthState>) -> impl IntoResponse {
    if state.is_ready().await { (StatusCode::OK, "ok") } else { (StatusCode::SERVICE_UNAVAILABLE, "not ready") }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    use super::*;

    async fn status(router: &Router, path: &str) -> StatusCode {
        router.clone().oneshot(Request::get(path).body(Body::empty()).expect("request")).await.expect("response").status()
    }

    #[tokio::test]
    async fn ready_follows_probes_and_draining_while_health_stays_up() {
        let flag = Arc::new(AtomicBool::new(false));
        let probe_flag = flag.clone();
        let state = HealthState::new(vec![Arc::new(FnProbe::new("key ring", move || probe_flag.load(Ordering::SeqCst)))]);
        let router = router(state.clone());
        assert_eq!(status(&router, "/health").await, StatusCode::OK);
        assert_eq!(status(&router, "/ready").await, StatusCode::SERVICE_UNAVAILABLE);
        flag.store(true, Ordering::SeqCst);
        assert_eq!(status(&router, "/ready").await, StatusCode::OK);
        state.mark_draining();
        assert_eq!(status(&router, "/ready").await, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(status(&router, "/health").await, StatusCode::OK);
    }
}
