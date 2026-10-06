//! `PgWebhookInbox` (storage.md "Webhook inbox semantics"): table `webhook_inbox`, platform pool
//! (deliveries precede tenant resolution). `accept` stores the row and enqueues its intake job in
//! one transaction; both unique keys (`(source, delivery_id)`, `(source, body_sha256)`) make a
//! replay a duplicate.

use std::sync::Arc;

use async_trait::async_trait;
use mage_core::domain::OrgId;
use mage_core::ports::clock::Clock;
use mage_core::ports::ids::IdGen;
use mage_core::ports::jobs::{Enqueued, NewJob};
use mage_infra::pg::jobs::enqueue_in;
use mage_infra::pg::{DbError, PgPools, TxError, begin_platform, retry_tx};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;
use uuid::Uuid;

use super::codec::{fixed, parse_enum};
use crate::ports::inbox::{
    Accepted, Delivery, INBOX_PURGE_BATCH, InboxError, InboxOutcome, NewDelivery, WebhookInbox, WebhookSource, check_delivery_job,
};

pub struct PgWebhookInbox {
    pools: PgPools,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGen>,
}

impl PgWebhookInbox {
    pub fn new(pools: PgPools, clock: Arc<dyn Clock>, ids: Arc<dyn IdGen>) -> Self {
        Self { pools, clock, ids }
    }
}

impl From<DbError> for InboxError {
    fn from(err: DbError) -> Self {
        match err {
            DbError::NotFound => InboxError::NotFound,
            DbError::Conflict { .. } => InboxError::Conflict,
            DbError::Invalid(msg) => InboxError::Invalid(msg),
            DbError::Unavailable(msg) | DbError::Retry(msg) => InboxError::Unavailable(msg),
            DbError::Timeout => InboxError::Unavailable("statement timeout".into()),
        }
    }
}

fn parse_source(s: &str) -> Option<WebhookSource> {
    [WebhookSource::GitHub, WebhookSource::GitHubFix, WebhookSource::Linear, WebhookSource::Slack].into_iter().find(|v| v.as_str() == s)
}

const COLUMNS: &str = "source, delivery_id, event, org_id, received_at, body, body_sha256, outcome, outcome_note, processed_at";

#[derive(sqlx::FromRow)]
struct DeliveryRow {
    source: String,
    delivery_id: String,
    event: String,
    org_id: Option<Uuid>,
    received_at: OffsetDateTime,
    body: Option<Vec<u8>>,
    body_sha256: Vec<u8>,
    outcome: Option<String>,
    outcome_note: Option<String>,
    processed_at: Option<OffsetDateTime>,
}

impl DeliveryRow {
    fn into_delivery(self) -> Result<Delivery, DbError> {
        let outcome = self
            .outcome
            .map(|o| InboxOutcome::from_parts(&o, self.outcome_note).ok_or_else(|| DbError::Invalid(format!("webhook_inbox.outcome: unknown value {o:?}"))))
            .transpose()?;
        Ok(Delivery {
            source: parse_enum(&self.source, "webhook_inbox.source", parse_source)?,
            delivery_id: self.delivery_id,
            event: self.event,
            org_id: self.org_id.map(OrgId::from_uuid),
            received_at: self.received_at,
            body: self.body,
            body_sha256: fixed(self.body_sha256, "webhook_inbox.body_sha256")?,
            outcome,
            processed_at: self.processed_at,
        })
    }
}

impl PgWebhookInbox {
    /// Runs one purge statement (`$1` cutoff, `$2` batch size) until a batch comes back short.
    async fn purge_batches(&self, sql: &str, before: OffsetDateTime) -> Result<u64, InboxError> {
        let mut total = 0u64;
        loop {
            let n = retry_tx(|| async {
                let mut tx = begin_platform(&self.pools.platform).await?;
                let done = sqlx::query(sql).bind(before).bind(INBOX_PURGE_BATCH).execute(&mut *tx).await?;
                tx.commit().await?;
                Ok::<_, TxError<InboxError>>(done.rows_affected())
            })
            .await?;
            total += n;
            if n < INBOX_PURGE_BATCH as u64 {
                return Ok(total);
            }
        }
    }
}

#[async_trait]
impl WebhookInbox for PgWebhookInbox {
    async fn accept(&self, d: NewDelivery, job: NewJob) -> Result<Accepted, InboxError> {
        check_delivery_job(&d, &job)?;
        let sha: [u8; 32] = Sha256::digest(&d.body).into();
        let bytes = i32::try_from(d.body.len()).map_err(|_| InboxError::Invalid(format!("delivery body of {} bytes is too large", d.body.len())))?;
        let now = self.clock.now();
        let (source, org) = (d.source.as_str(), d.org_id.map(|o| o.as_uuid()));
        retry_tx(|| async {
            let mut tx = begin_platform(&self.pools.platform).await?;
            let inserted: Option<OffsetDateTime> = sqlx::query_scalar(
                "INSERT INTO webhook_inbox (source, delivery_id, org_id, event, body, body_sha256, body_bytes, received_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
                 ON CONFLICT DO NOTHING \
                 RETURNING received_at",
            )
            .bind(source)
            .bind(&d.delivery_id)
            .bind(org)
            .bind(&d.event)
            .bind(&d.body)
            .bind(sha.as_slice())
            .bind(bytes)
            .bind(now)
            .fetch_optional(&mut *tx)
            .await?;
            let accepted = if inserted.is_some() {
                let (Enqueued::New(job_id) | Enqueued::Duplicate(job_id)) = enqueue_in(&mut tx, &job, now, &*self.ids).await?;
                sqlx::query("UPDATE webhook_inbox SET job_id = $3 WHERE source = $1 AND delivery_id = $2")
                    .bind(source)
                    .bind(&d.delivery_id)
                    .bind(job_id.as_uuid())
                    .execute(&mut *tx)
                    .await?;
                Accepted::New { job_id }
            } else {
                let first: Option<OffsetDateTime> = sqlx::query_scalar(
                    "SELECT min(received_at) FROM webhook_inbox WHERE source = $1 AND (delivery_id = $2 OR body_sha256 = $3)",
                )
                .bind(source)
                .bind(&d.delivery_id)
                .bind(sha.as_slice())
                .fetch_one(&mut *tx)
                .await?;
                // The conflicting row was purged between the INSERT and this read: start over.
                let first_seen_at = first.ok_or_else(|| DbError::Retry("conflicting delivery vanished".into()))?;
                Accepted::Duplicate { first_seen_at }
            };
            tx.commit().await?;
            Ok::<_, TxError<InboxError>>(accepted)
        })
        .await
    }

    async fn get(&self, source: WebhookSource, delivery_id: &str) -> Result<Delivery, InboxError> {
        retry_tx(|| async {
            let mut tx = begin_platform(&self.pools.platform).await?;
            let row: Option<DeliveryRow> = sqlx::query_as(&format!("SELECT {COLUMNS} FROM webhook_inbox WHERE source = $1 AND delivery_id = $2"))
                .bind(source.as_str())
                .bind(delivery_id)
                .fetch_optional(&mut *tx)
                .await?;
            tx.commit().await?;
            let row = row.ok_or(TxError::Port(InboxError::NotFound))?;
            Ok::<_, TxError<InboxError>>(row.into_delivery()?)
        })
        .await
    }

    async fn resolve_org(&self, source: WebhookSource, delivery_id: &str, org: OrgId) -> Result<(), InboxError> {
        retry_tx(|| async {
            let mut tx = begin_platform(&self.pools.platform).await?;
            let updated = sqlx::query(
                "UPDATE webhook_inbox SET org_id = $3 WHERE source = $1 AND delivery_id = $2 AND (org_id IS NULL OR org_id = $3)",
            )
            .bind(source.as_str())
            .bind(delivery_id)
            .bind(org.as_uuid())
            .execute(&mut *tx)
            .await?;
            if updated.rows_affected() == 0 {
                let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM webhook_inbox WHERE source = $1 AND delivery_id = $2)")
                    .bind(source.as_str())
                    .bind(delivery_id)
                    .fetch_one(&mut *tx)
                    .await?;
                return Err(TxError::Port(if exists { InboxError::Conflict } else { InboxError::NotFound }));
            }
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    async fn mark_processed(&self, source: WebhookSource, delivery_id: &str, outcome: InboxOutcome) -> Result<(), InboxError> {
        let now = self.clock.now();
        retry_tx(|| async {
            let mut tx = begin_platform(&self.pools.platform).await?;
            let updated = sqlx::query(
                "UPDATE webhook_inbox SET outcome = $3, outcome_note = $4, processed_at = $5 WHERE source = $1 AND delivery_id = $2",
            )
            .bind(source.as_str())
            .bind(delivery_id)
            .bind(outcome.as_str())
            .bind(outcome.note())
            .bind(now)
            .execute(&mut *tx)
            .await?;
            if updated.rows_affected() == 0 {
                return Err(TxError::Port(InboxError::NotFound));
            }
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    async fn purge(&self, bodies_before: OffsetDateTime, rows_before: OffsetDateTime) -> Result<u64, InboxError> {
        let bodies = self
            .purge_batches(
                "UPDATE webhook_inbox SET body = NULL WHERE (source, delivery_id) IN \
                 (SELECT source, delivery_id FROM webhook_inbox WHERE processed_at < $1 AND body IS NOT NULL LIMIT $2)",
                bodies_before,
            )
            .await?;
        let rows = self
            .purge_batches(
                "DELETE FROM webhook_inbox WHERE (source, delivery_id) IN \
                 (SELECT source, delivery_id FROM webhook_inbox WHERE received_at < $1 LIMIT $2)",
                rows_before,
            )
            .await?;
        Ok(bodies + rows)
    }
}

#[cfg(test)]
mod tests {
    use mage_core::testing::SeqIdGen;
    use mage_infra::pg::PgJobQueue;

    use super::*;
    use crate::testing::contract::{InboxFixture, contract_webhook_inbox};
    use crate::testing::pg::with_pg;

    #[tokio::test]
    async fn pg_webhook_inbox_passes_the_contract() {
        with_pg("pg_inbox_contract", |env| async move {
            contract_webhook_inbox(|clock| {
                let env = env.clone();
                async move {
                    let db = env.fresh().await;
                    // One id source for the inbox's outbox inserts and the queue (jobs.id is unique).
                    let ids: Arc<dyn IdGen> = Arc::new(SeqIdGen::new());
                    InboxFixture {
                        inbox: Arc::new(PgWebhookInbox::new(db.pools.clone(), clock.clone(), ids.clone())),
                        jobs: Arc::new(PgJobQueue::new(db.pools.clone(), clock.clone(), ids)),
                        seed: db.seed(clock),
                    }
                }
            })
            .await
        })
        .await;
    }
}
