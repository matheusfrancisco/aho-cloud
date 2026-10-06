//! `PgAuditLog` (server.md "Org feed and audit"): one `INSERT` per record into the append-only
//! `audit_log` (no role may UPDATE it; only the platform role deletes past retention). Org rows go
//! through the tenant pool, platform rows (`org = None`) through the platform pool. The outcome and
//! the ordered detail pairs live in the `details` jsonb (`{"outcome": …, "items": [[k, v], …]}`).

use std::borrow::Cow;
use std::sync::Arc;

use async_trait::async_trait;
use mage_core::domain::OrgId;
use mage_core::ports::clock::Clock;
use mage_core::ports::ids::IdGen;
use mage_infra::pg::{DbError, PgPools};
use serde::{Deserialize, Serialize};
use sqlx::types::Json;
use time::OffsetDateTime;

use super::codec;
use crate::ports::audit::{AUDIT_LIST_MAX, Actor, AuditAction, AuditError, AuditEvent, AuditFilter, AuditLog, AuditOutcome, AuditRecord, AuditTarget};

pub struct PgAuditLog {
    pools: PgPools,
    clock: Arc<dyn Clock>,
}

impl PgAuditLog {
    pub fn new(pools: PgPools, clock: Arc<dyn Clock>, _ids: Arc<dyn IdGen>) -> Self {
        Self { pools, clock }
    }
}

impl From<DbError> for AuditError {
    fn from(err: DbError) -> Self {
        match err {
            DbError::NotFound => AuditError::NotFound,
            DbError::Conflict { .. } => AuditError::Conflict,
            DbError::Invalid(msg) => AuditError::Invalid(msg),
            DbError::Unavailable(msg) | DbError::Retry(msg) => AuditError::Unavailable(msg),
            DbError::Timeout => AuditError::Unavailable("statement timeout".into()),
        }
    }
}

fn db(err: sqlx::Error) -> AuditError {
    DbError::from(err).into()
}

/// `audit_log.details`.
#[derive(Serialize, Deserialize)]
struct Details {
    outcome: AuditOutcome,
    items: Vec<(String, String)>,
}

#[derive(sqlx::FromRow)]
struct Row {
    id: i64,
    org_id: Option<uuid::Uuid>,
    actor_kind: String,
    actor_id: Option<String>,
    action: String,
    target_kind: Option<String>,
    target_id: Option<String>,
    ip: Option<String>,
    request_id: Option<String>,
    at: OffsetDateTime,
    details: Json<Details>,
}

impl Row {
    fn into_record(self) -> Result<AuditRecord, DbError> {
        let actor = Actor::from_parts(&self.actor_kind, self.actor_id.as_deref()).map_err(|e| DbError::Invalid(format!("audit_log.actor: {e}")))?;
        let action: AuditAction = self.action.parse().map_err(|e| DbError::Invalid(format!("audit_log.action: {e}")))?;
        let Json(details) = self.details;
        Ok(AuditRecord {
            id: self.id,
            at: self.at,
            event: AuditEvent {
                org: self.org_id.map(OrgId::from_uuid),
                actor,
                action,
                target: AuditTarget { kind: Cow::Owned(self.target_kind.unwrap_or_default()), id: self.target_id.unwrap_or_default() },
                outcome: details.outcome,
                ip: codec::parse_ip(self.ip, "audit_log.ip")?,
                request_id: self.request_id,
                details: details.items.into_iter().map(|(k, v)| (Cow::Owned(k), v)).collect(),
            },
        })
    }
}

#[async_trait]
impl AuditLog for PgAuditLog {
    async fn record(&self, event: AuditEvent) -> Result<i64, AuditError> {
        let pool = if event.org.is_some() { &self.pools.tenant } else { &self.pools.platform };
        let details = Details { outcome: event.outcome, items: event.details.iter().map(|(k, v)| (k.to_string(), v.clone())).collect() };
        sqlx::query_scalar(
            "INSERT INTO audit_log (org_id, actor_kind, actor_id, action, target_kind, target_id, ip, request_id, at, details) \
             VALUES ($1, $2, $3, $4, $5, $6, $7::text::inet, $8, $9, $10) RETURNING id",
        )
        .bind(event.org.map(|o| o.as_uuid()))
        .bind(event.actor.kind())
        .bind(event.actor.id())
        .bind(event.action.as_str())
        .bind(event.target.kind.as_ref())
        .bind(&event.target.id)
        .bind(codec::ip_text(event.ip))
        .bind(&event.request_id)
        .bind(self.clock.now())
        .bind(Json(details))
        .fetch_one(pool)
        .await
        .map_err(db)
    }

    async fn list(&self, org: OrgId, filter: AuditFilter, limit: u32) -> Result<Vec<AuditRecord>, AuditError> {
        let limit = i64::from(limit.clamp(1, AUDIT_LIST_MAX));
        let (actor_kind, actor_id) = match &filter.actor {
            Some(actor) => (Some(actor.kind()), actor.id()),
            None => (None, None),
        };
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT id, org_id, actor_kind, actor_id, action, target_kind, target_id, host(ip) AS ip, request_id, at, details \
             FROM audit_log \
             WHERE org_id = $1 \
               AND ($2::bigint IS NULL OR id < $2) \
               AND ($3::text IS NULL OR action = $3) \
               AND ($4::text IS NULL OR (actor_kind = $4 AND actor_id IS NOT DISTINCT FROM $5)) \
             ORDER BY id DESC LIMIT $6",
        )
        .bind(org.as_uuid())
        .bind(filter.before)
        .bind(filter.action.map(AuditAction::as_str))
        .bind(actor_kind)
        .bind(actor_id)
        .bind(limit)
        .fetch_all(&self.pools.tenant)
        .await
        .map_err(db)?;
        rows.into_iter().map(|r| r.into_record().map_err(AuditError::from)).collect()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use std::sync::Arc;

    use mage_core::testing::SeqIdGen;

    use super::*;
    use crate::testing::contract::{AuditFixture, contract_audit_log};
    use crate::testing::pg::with_pg;

    #[tokio::test]
    async fn pg_audit_log_passes_the_contract() {
        with_pg("pg_audit_log_passes_the_contract", |env| async move {
            contract_audit_log(|clock| {
                let env = env.clone();
                async move {
                    let db = env.fresh().await;
                    AuditFixture { audit: Arc::new(PgAuditLog::new(db.pools.clone(), clock, Arc::new(SeqIdGen::new()))) }
                }
            })
            .await;
        })
        .await;
    }

    /// Append-only is a grant: the tenant role may insert and read but not rewrite history.
    #[tokio::test]
    async fn pg_tenant_role_cannot_update_or_delete_audit_rows() {
        with_pg("pg_tenant_role_cannot_update_or_delete_audit_rows", |env| async move {
            let db = env.fresh().await;
            let tenant_is_distinct: bool = sqlx::query_scalar("SELECT current_user <> 'postgres' AND NOT rolsuper FROM pg_roles WHERE rolname = current_user")
                .fetch_one(&db.pools.tenant)
                .await
                .unwrap();
            if !tenant_is_distinct {
                eprintln!("skipping: tenant pool is not a restricted role");
                return;
            }
            let update = sqlx::query("UPDATE audit_log SET action = 'x'").execute(&db.pools.tenant).await;
            let delete = sqlx::query("DELETE FROM audit_log").execute(&db.pools.tenant).await;
            for result in [update, delete] {
                let err = result.expect_err("tenant role must not rewrite the audit log");
                assert_eq!(err.as_database_error().and_then(|e| e.code()).as_deref(), Some("42501"), "{err}");
            }
        })
        .await;
    }
}
