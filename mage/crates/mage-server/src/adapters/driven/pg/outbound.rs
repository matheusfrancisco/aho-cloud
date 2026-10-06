//! `PgOutboundLog` (intake.md "`OutboundLog`", "`mage_server::adapters::driven::pg`"): table
//! `outbound_messages`, tenant pool. `begin` is `INSERT … ON CONFLICT (org_id, ticket_id, target,
//! notice_key) DO NOTHING` followed by a read, so concurrent workers agree on one `client_id`.

use std::sync::Arc;

use async_trait::async_trait;
use mage_core::domain::{OrgId, TicketId};
use mage_core::ports::clock::Clock;
use mage_core::ports::ids::IdGen;
use mage_infra::pg::{DbError, PgPools, TxError, begin_org, retry_tx};
use sqlx::PgConnection;
use time::OffsetDateTime;
use uuid::Uuid;

use super::codec::{fixed, parse_enum, to_unsigned};
use crate::ports::outbound::{OutboundError, OutboundKey, OutboundLog, OutboundRecord, OutboundState, OutboundTarget};

pub struct PgOutboundLog {
    pools: PgPools,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGen>,
}

impl PgOutboundLog {
    pub fn new(pools: PgPools, clock: Arc<dyn Clock>, ids: Arc<dyn IdGen>) -> Self {
        Self { pools, clock, ids }
    }
}

impl From<DbError> for OutboundError {
    fn from(err: DbError) -> Self {
        match err {
            DbError::NotFound => OutboundError::NotFound,
            DbError::Conflict { .. } => OutboundError::Conflict,
            DbError::Invalid(msg) => OutboundError::Invalid(msg),
            DbError::Unavailable(msg) | DbError::Retry(msg) => OutboundError::Unavailable(msg),
            DbError::Timeout => OutboundError::Unavailable("statement timeout".into()),
        }
    }
}

const COLUMNS: &str = "ticket_id, target, notice_key, client_id, state, uncertain, external_id, body_sha256, attempts, last_error, created_at, sent_at";

/// `WHERE` clause of one key (`$1` org, `$2` ticket, `$3` target, `$4` notice key).
const KEY: &str = "org_id = $1 AND ticket_id = $2 AND target = $3 AND notice_key = $4";

#[derive(sqlx::FromRow)]
struct OutboundRow {
    ticket_id: Uuid,
    target: String,
    notice_key: String,
    client_id: Uuid,
    state: String,
    uncertain: bool,
    external_id: Option<String>,
    body_sha256: Vec<u8>,
    attempts: i16,
    last_error: Option<String>,
    created_at: OffsetDateTime,
    sent_at: Option<OffsetDateTime>,
}

impl OutboundRow {
    fn into_record(self) -> Result<OutboundRecord, DbError> {
        let state = match self.state.as_str() {
            "pending" => OutboundState::Pending { uncertain: self.uncertain },
            "sent" => OutboundState::Sent,
            "skipped" => OutboundState::Skipped { reason: self.last_error.clone().unwrap_or_default() },
            "failed" => OutboundState::Failed { reason: self.last_error.clone().unwrap_or_default() },
            other => return Err(DbError::Invalid(format!("outbound_messages.state: unknown value {other:?}"))),
        };
        Ok(OutboundRecord {
            key: OutboundKey {
                ticket_id: TicketId::from_uuid(self.ticket_id),
                target: parse_enum(&self.target, "outbound_messages.target", OutboundTarget::parse)?,
                notice_key: self.notice_key,
            },
            client_id: self.client_id,
            state,
            attempts: to_unsigned(i64::from(self.attempts), "outbound_messages.attempts")?,
            body_sha256: fixed(self.body_sha256, "outbound_messages.body_sha256")?,
            external_id: self.external_id,
            last_error: self.last_error,
            created_at: self.created_at,
            sent_at: self.sent_at,
        })
    }
}

/// `(state, external_id, last_error)` of the key's row, if any.
async fn current(conn: &mut PgConnection, org: OrgId, key: &OutboundKey) -> Result<Option<(String, Option<String>, Option<String>)>, DbError> {
    Ok(sqlx::query_as(&format!("SELECT state, external_id, last_error FROM outbound_messages WHERE {KEY}"))
        .bind(org.as_uuid())
        .bind(key.ticket_id.as_uuid())
        .bind(key.target.as_str())
        .bind(&key.notice_key)
        .fetch_optional(conn)
        .await?)
}

impl PgOutboundLog {
    /// Pending → `state` with the reason in `last_error`; the same final state and reason again is
    /// a no-op, anything else `Conflict`.
    async fn finish(&self, org: OrgId, key: &OutboundKey, state: &'static str, reason: &str) -> Result<(), OutboundError> {
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let updated = sqlx::query(&format!("UPDATE outbound_messages SET state = $5, last_error = $6 WHERE {KEY} AND state = 'pending'"))
                .bind(org.as_uuid())
                .bind(key.ticket_id.as_uuid())
                .bind(key.target.as_str())
                .bind(&key.notice_key)
                .bind(state)
                .bind(reason)
                .execute(&mut *tx)
                .await?;
            if updated.rows_affected() == 0 {
                return match current(&mut tx, org, key).await? {
                    None => Err(TxError::Port(OutboundError::NotFound)),
                    Some((s, _, last_error)) if s == state && last_error.as_deref() == Some(reason) => Ok(()),
                    Some(_) => Err(TxError::Port(OutboundError::Conflict)),
                };
            }
            tx.commit().await?;
            Ok(())
        })
        .await
    }
}

#[async_trait]
impl OutboundLog for PgOutboundLog {
    async fn begin(&self, org: OrgId, key: &OutboundKey, client_id: Uuid, body_sha256: [u8; 32]) -> Result<OutboundRecord, OutboundError> {
        let now = self.clock.now();
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            sqlx::query(
                "INSERT INTO outbound_messages (org_id, id, ticket_id, target, notice_key, client_id, state, uncertain, body_sha256, attempts, created_at) \
                 VALUES ($1, $5, $2, $3, $4, $6, 'pending', false, $7, 0, $8) \
                 ON CONFLICT (org_id, ticket_id, target, notice_key) DO NOTHING",
            )
            .bind(org.as_uuid())
            .bind(key.ticket_id.as_uuid())
            .bind(key.target.as_str())
            .bind(&key.notice_key)
            .bind(self.ids.uuid_v7())
            .bind(client_id)
            .bind(body_sha256.as_slice())
            .bind(now)
            .execute(&mut *tx)
            .await?;
            // READ COMMITTED: a fresh snapshot sees the row whichever transaction inserted it.
            let row: OutboundRow = sqlx::query_as(&format!("SELECT {COLUMNS} FROM outbound_messages WHERE {KEY}"))
                .bind(org.as_uuid())
                .bind(key.ticket_id.as_uuid())
                .bind(key.target.as_str())
                .bind(&key.notice_key)
                .fetch_one(&mut *tx)
                .await?;
            tx.commit().await?;
            Ok::<_, TxError<OutboundError>>(row.into_record()?)
        })
        .await
    }

    async fn mark_attempt(&self, org: OrgId, key: &OutboundKey, uncertain: bool, error: Option<&str>) -> Result<(), OutboundError> {
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let updated = sqlx::query(&format!(
                "UPDATE outbound_messages SET attempts = LEAST(attempts + 1, 32767), uncertain = $5, last_error = $6 \
                 WHERE {KEY} AND state = 'pending'"
            ))
            .bind(org.as_uuid())
            .bind(key.ticket_id.as_uuid())
            .bind(key.target.as_str())
            .bind(&key.notice_key)
            .bind(uncertain)
            .bind(error)
            .execute(&mut *tx)
            .await?;
            if updated.rows_affected() == 0 {
                let found = current(&mut tx, org, key).await?;
                return Err(TxError::Port(if found.is_some() { OutboundError::Conflict } else { OutboundError::NotFound }));
            }
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    async fn mark_sent(&self, org: OrgId, key: &OutboundKey, external_id: &str, at: OffsetDateTime) -> Result<(), OutboundError> {
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let updated = sqlx::query(&format!(
                "UPDATE outbound_messages SET state = 'sent', external_id = $5, sent_at = $6 WHERE {KEY} AND state = 'pending'"
            ))
            .bind(org.as_uuid())
            .bind(key.ticket_id.as_uuid())
            .bind(key.target.as_str())
            .bind(&key.notice_key)
            .bind(external_id)
            .bind(at)
            .execute(&mut *tx)
            .await?;
            if updated.rows_affected() == 0 {
                return match current(&mut tx, org, key).await? {
                    None => Err(TxError::Port(OutboundError::NotFound)),
                    Some((state, ext, _)) if state == "sent" && ext.as_deref() == Some(external_id) => Ok(()),
                    Some(_) => Err(TxError::Port(OutboundError::Conflict)),
                };
            }
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    async fn mark_skipped(&self, org: OrgId, key: &OutboundKey, reason: &str) -> Result<(), OutboundError> {
        self.finish(org, key, "skipped", reason).await
    }

    async fn mark_failed(&self, org: OrgId, key: &OutboundKey, reason: &str) -> Result<(), OutboundError> {
        self.finish(org, key, "failed", reason).await
    }

    async fn list_for_ticket(&self, org: OrgId, ticket: TicketId) -> Result<Vec<OutboundRecord>, OutboundError> {
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let rows: Vec<OutboundRow> =
                sqlx::query_as(&format!("SELECT {COLUMNS} FROM outbound_messages WHERE org_id = $1 AND ticket_id = $2 ORDER BY created_at, id"))
                    .bind(org.as_uuid())
                    .bind(ticket.as_uuid())
                    .fetch_all(&mut *tx)
                    .await?;
            tx.commit().await?;
            Ok::<_, TxError<OutboundError>>(rows.into_iter().map(OutboundRow::into_record).collect::<Result<Vec<_>, _>>()?)
        })
        .await
    }

    async fn find_by_external(&self, org: OrgId, target: OutboundTarget, external_id: &str) -> Result<Option<OutboundRecord>, OutboundError> {
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let row: Option<OutboundRow> = sqlx::query_as(&format!(
                "SELECT {COLUMNS} FROM outbound_messages WHERE org_id = $1 AND target = $2 AND external_id = $3 ORDER BY created_at, id LIMIT 1"
            ))
            .bind(org.as_uuid())
            .bind(target.as_str())
            .bind(external_id)
            .fetch_optional(&mut *tx)
            .await?;
            tx.commit().await?;
            Ok::<_, TxError<OutboundError>>(row.map(OutboundRow::into_record).transpose()?)
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use mage_core::testing::SeqIdGen;

    use super::*;
    use crate::testing::contract::{OutboundFixture, contract_outbound_log};
    use crate::testing::pg::with_pg;

    #[tokio::test]
    async fn pg_outbound_log_passes_the_contract() {
        with_pg("pg_outbound_contract", |env| async move {
            contract_outbound_log(|clock| {
                let env = env.clone();
                async move {
                    let db = env.fresh().await;
                    OutboundFixture { log: Arc::new(PgOutboundLog::new(db.pools.clone(), clock.clone(), Arc::new(SeqIdGen::new()))), seed: db.seed(clock) }
                }
            })
            .await
        })
        .await;
    }
}
