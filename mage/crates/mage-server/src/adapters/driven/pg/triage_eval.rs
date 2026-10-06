//! `PgTriageEvalStore` (triage.md "Evaluation harness"): tables `triage_evals`,
//! `triage_eval_cases`, `triage_labels`, tenant pool. A case row is created with zeroed counters
//! and counts as done once `record_case_result` sets `finished_at`.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use mage_core::domain::{OrgId, RepoPath, RunId, RunStatus, TicketId, TriageEvalId, Usage};
use mage_core::ports::clock::Clock;
use mage_core::ports::ids::IdGen;
use mage_infra::pg::{DbError, PgPools, TxError, begin_org, retry_tx};
use sqlx::types::Json;
use time::OffsetDateTime;
use uuid::Uuid;

use super::codec::{parse_enum, to_unsigned};
use crate::ports::triage_eval::{
    EvalCase, EvalCaseResult, EvalStoreError, GroundTruth, LabelSource, LabelStrength, NewTriageEval, TriageEval, TriageEvalStore,
};

pub struct PgTriageEvalStore {
    pools: PgPools,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGen>,
}

impl PgTriageEvalStore {
    pub fn new(pools: PgPools, clock: Arc<dyn Clock>, ids: Arc<dyn IdGen>) -> Self {
        Self { pools, clock, ids }
    }
}

impl From<DbError> for EvalStoreError {
    fn from(err: DbError) -> Self {
        match err {
            DbError::NotFound => EvalStoreError::NotFound,
            DbError::Conflict { .. } => EvalStoreError::Conflict,
            DbError::Invalid(msg) => EvalStoreError::Invalid(msg),
            DbError::Unavailable(msg) | DbError::Retry(msg) => EvalStoreError::Unavailable(msg),
            DbError::Timeout => EvalStoreError::Unavailable("statement timeout".into()),
        }
    }
}

const RUN_STATUSES: [RunStatus; 10] = [
    RunStatus::Queued,
    RunStatus::Leased,
    RunStatus::Preparing,
    RunStatus::Running,
    RunStatus::AwaitingGate,
    RunStatus::Succeeded,
    RunStatus::Failed,
    RunStatus::TimedOut,
    RunStatus::Cancelled,
    RunStatus::Errored,
];

fn parse_run_status(s: &str) -> Option<RunStatus> {
    RUN_STATUSES.into_iter().find(|v| v.name() == s)
}

fn smallint<T: Copy + Into<i64>>(value: T, what: &str) -> Result<i16, EvalStoreError> {
    let wide: i64 = value.into();
    i16::try_from(wide).map_err(|_| EvalStoreError::Invalid(format!("{what} {wide} out of range")))
}

fn millis(d: Duration, what: &str) -> Result<i64, EvalStoreError> {
    i64::try_from(d.as_millis()).map_err(|_| EvalStoreError::Invalid(format!("{what} out of range")))
}

fn unsigned<T: TryFrom<i64>>(value: i16, column: &str) -> Result<T, DbError> {
    to_unsigned(i64::from(value), column)
}

fn ms(value: Option<i64>, column: &str) -> Result<Duration, DbError> {
    let v = value.ok_or_else(|| DbError::Invalid(format!("{column}: NULL on a finished case")))?;
    Ok(Duration::from_millis(to_unsigned(v, column)?))
}

#[derive(sqlx::FromRow)]
struct ResultRow {
    ticket_id: Uuid,
    repeat: i16,
    run_id: Option<Uuid>,
    status: Option<String>,
    hit_rank: Option<i16>,
    primary_hit_rank: Option<i16>,
    causes: i16,
    usage: Option<Json<Usage>>,
    queue_wait_ms: Option<i64>,
    agent_time_ms: Option<i64>,
    gate_attempts: i16,
    dropped_causes: i16,
    adjusted_citations: i16,
}

impl ResultRow {
    fn into_result(self, eval: TriageEvalId) -> Result<EvalCaseResult, DbError> {
        let missing = |column: &str| DbError::Invalid(format!("triage_eval_cases.{column}: NULL on a finished case"));
        let status = self.status.ok_or_else(|| missing("status"))?;
        Ok(EvalCaseResult {
            eval,
            ticket: TicketId::from_uuid(self.ticket_id),
            repeat: unsigned(self.repeat, "triage_eval_cases.repeat")?,
            run: RunId::from_uuid(self.run_id.ok_or_else(|| missing("run_id"))?),
            status: parse_enum(&status, "triage_eval_cases.status", parse_run_status)?,
            hit_rank: self.hit_rank.map(|v| unsigned(v, "triage_eval_cases.hit_rank")).transpose()?,
            primary_hit_rank: self.primary_hit_rank.map(|v| unsigned(v, "triage_eval_cases.primary_hit_rank")).transpose()?,
            causes: unsigned(self.causes, "triage_eval_cases.causes")?,
            usage: self.usage.ok_or_else(|| missing("usage"))?.0,
            queue_wait: ms(self.queue_wait_ms, "triage_eval_cases.queue_wait_ms")?,
            agent_time: ms(self.agent_time_ms, "triage_eval_cases.agent_time_ms")?,
            gate_attempts: unsigned(self.gate_attempts, "triage_eval_cases.gate_attempts")?,
            dropped_causes: unsigned(self.dropped_causes, "triage_eval_cases.dropped_causes")?,
            adjusted_citations: unsigned(self.adjusted_citations, "triage_eval_cases.adjusted_citations")?,
        })
    }
}

#[derive(sqlx::FromRow)]
struct LabelRow {
    ticket_id: Uuid,
    paths: Vec<String>,
    strength: String,
    source: String,
}

impl LabelRow {
    fn into_label(self) -> Result<GroundTruth, DbError> {
        let paths = self
            .paths
            .into_iter()
            .map(|p| RepoPath::new(p.clone()).map_err(|e| DbError::Invalid(format!("triage_labels.paths: {p:?}: {e}"))))
            .collect::<Result<BTreeSet<_>, _>>()?;
        Ok(GroundTruth {
            ticket: TicketId::from_uuid(self.ticket_id),
            paths,
            strength: parse_enum(&self.strength, "triage_labels.strength", LabelStrength::parse)?,
            source: parse_enum(&self.source, "triage_labels.source", LabelSource::parse)?,
        })
    }
}

#[async_trait]
impl TriageEvalStore for PgTriageEvalStore {
    async fn create_eval(&self, org: OrgId, eval: NewTriageEval) -> Result<TriageEvalId, EvalStoreError> {
        let id = TriageEvalId::from_uuid(self.ids.uuid_v7());
        let now = self.clock.now();
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            sqlx::query(
                "INSERT INTO triage_evals (org_id, id, name, corpus, variant_toml, repeat, created_by, created_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            )
            .bind(org.as_uuid())
            .bind(id.as_uuid())
            .bind(&eval.name)
            .bind(&eval.corpus)
            .bind(&eval.variant_toml)
            .bind(i16::from(eval.repeat))
            .bind(eval.created_by.as_uuid())
            .bind(now)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            Ok::<_, TxError<EvalStoreError>>(id)
        })
        .await
    }

    async fn add_cases(&self, org: OrgId, eval: TriageEvalId, cases: &[EvalCase]) -> Result<(), EvalStoreError> {
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let exists: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM triage_evals WHERE org_id = $1 AND id = $2)")
                .bind(org.as_uuid())
                .bind(eval.as_uuid())
                .fetch_one(&mut *tx)
                .await?;
            if !exists {
                return Err(TxError::Port(EvalStoreError::NotFound));
            }
            for case in cases {
                sqlx::query(
                    "INSERT INTO triage_eval_cases (org_id, eval_id, ticket_id, repeat, input_run, causes, gate_attempts, dropped_causes, adjusted_citations) \
                     SELECT $1, $2, $3, r, $4, 0, 0, 0, 0 FROM generate_series(0, $5 - 1) AS r \
                     ON CONFLICT DO NOTHING",
                )
                .bind(org.as_uuid())
                .bind(eval.as_uuid())
                .bind(case.ticket.as_uuid())
                .bind(case.input_run.map(|r| r.as_uuid()))
                .bind(i32::from(case.repeats))
                .execute(&mut *tx)
                .await?;
            }
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    async fn record_case_run(&self, org: OrgId, eval: TriageEvalId, ticket: TicketId, repeat: u8, run: RunId) -> Result<(), EvalStoreError> {
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let updated = sqlx::query(
                "UPDATE triage_eval_cases SET run_id = $5 \
                 WHERE org_id = $1 AND eval_id = $2 AND ticket_id = $3 AND repeat = $4 AND (run_id IS NULL OR run_id = $5)",
            )
            .bind(org.as_uuid())
            .bind(eval.as_uuid())
            .bind(ticket.as_uuid())
            .bind(i16::from(repeat))
            .bind(run.as_uuid())
            .execute(&mut *tx)
            .await?;
            if updated.rows_affected() == 0 {
                let exists: bool = sqlx::query_scalar(
                    "SELECT EXISTS (SELECT 1 FROM triage_eval_cases WHERE org_id = $1 AND eval_id = $2 AND ticket_id = $3 AND repeat = $4)",
                )
                .bind(org.as_uuid())
                .bind(eval.as_uuid())
                .bind(ticket.as_uuid())
                .bind(i16::from(repeat))
                .fetch_one(&mut *tx)
                .await?;
                return Err(TxError::Port(if exists { EvalStoreError::Conflict } else { EvalStoreError::NotFound }));
            }
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    async fn record_case_result(&self, org: OrgId, result: &EvalCaseResult) -> Result<(), EvalStoreError> {
        let hit_rank = result.hit_rank.map(|v| smallint(v, "hit_rank")).transpose()?;
        let primary_hit_rank = result.primary_hit_rank.map(|v| smallint(v, "primary_hit_rank")).transpose()?;
        let queue_wait = millis(result.queue_wait, "queue_wait")?;
        let agent_time = millis(result.agent_time, "agent_time")?;
        let now = self.clock.now();
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let updated = sqlx::query(
                "UPDATE triage_eval_cases SET run_id = $5, status = $6, hit_rank = $7, primary_hit_rank = $8, causes = $9, usage = $10, \
                        queue_wait_ms = $11, agent_time_ms = $12, gate_attempts = $13, dropped_causes = $14, adjusted_citations = $15, \
                        finished_at = $16 \
                 WHERE org_id = $1 AND eval_id = $2 AND ticket_id = $3 AND repeat = $4",
            )
            .bind(org.as_uuid())
            .bind(result.eval.as_uuid())
            .bind(result.ticket.as_uuid())
            .bind(i16::from(result.repeat))
            .bind(result.run.as_uuid())
            .bind(result.status.name())
            .bind(hit_rank)
            .bind(primary_hit_rank)
            .bind(i16::from(result.causes))
            .bind(Json(&result.usage))
            .bind(queue_wait)
            .bind(agent_time)
            .bind(i16::from(result.gate_attempts))
            .bind(i16::from(result.dropped_causes))
            .bind(i16::from(result.adjusted_citations))
            .bind(now)
            .execute(&mut *tx)
            .await?;
            if updated.rows_affected() == 0 {
                return Err(TxError::Port(EvalStoreError::NotFound));
            }
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    async fn eval(&self, org: OrgId, eval: TriageEvalId) -> Result<TriageEval, EvalStoreError> {
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let row: Option<(String, String, OffsetDateTime, i64, i64)> = sqlx::query_as(
                "SELECT e.name, e.variant_toml, e.created_at, \
                        (SELECT count(*) FROM triage_eval_cases c WHERE c.org_id = e.org_id AND c.eval_id = e.id), \
                        (SELECT count(*) FROM triage_eval_cases c WHERE c.org_id = e.org_id AND c.eval_id = e.id AND c.finished_at IS NOT NULL) \
                 FROM triage_evals e WHERE e.org_id = $1 AND e.id = $2",
            )
            .bind(org.as_uuid())
            .bind(eval.as_uuid())
            .fetch_optional(&mut *tx)
            .await?;
            tx.commit().await?;
            let (name, variant_toml, created_at, cases, done) = row.ok_or(TxError::Port(EvalStoreError::NotFound))?;
            Ok::<_, TxError<EvalStoreError>>(TriageEval {
                id: eval,
                name,
                variant_toml,
                created_at,
                cases: to_unsigned(cases, "triage_eval_cases count")?,
                done: to_unsigned(done, "triage_eval_cases done count")?,
            })
        })
        .await
    }

    async fn case_results(&self, org: OrgId, eval: TriageEvalId) -> Result<Vec<EvalCaseResult>, EvalStoreError> {
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let rows: Vec<ResultRow> = sqlx::query_as(
                "SELECT ticket_id, repeat, run_id, status, hit_rank, primary_hit_rank, causes, usage, queue_wait_ms, agent_time_ms, \
                        gate_attempts, dropped_causes, adjusted_citations \
                 FROM triage_eval_cases WHERE org_id = $1 AND eval_id = $2 AND finished_at IS NOT NULL \
                 ORDER BY ticket_id, repeat",
            )
            .bind(org.as_uuid())
            .bind(eval.as_uuid())
            .fetch_all(&mut *tx)
            .await?;
            tx.commit().await?;
            Ok::<_, TxError<EvalStoreError>>(rows.into_iter().map(|r| r.into_result(eval)).collect::<Result<Vec<_>, _>>()?)
        })
        .await
    }

    async fn put_label(&self, org: OrgId, label: &GroundTruth) -> Result<(), EvalStoreError> {
        if label.paths.is_empty() {
            return Err(EvalStoreError::Invalid("a label needs at least one path".into()));
        }
        // BTreeSet order: sorted by bytes.
        let paths: Vec<&str> = label.paths.iter().map(RepoPath::as_str).collect();
        let now = self.clock.now();
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            sqlx::query(
                "INSERT INTO triage_labels (org_id, ticket_id, paths, strength, source, labeled_by, updated_at) \
                 VALUES ($1, $2, $3, $4, $5, NULL, $6) \
                 ON CONFLICT (org_id, ticket_id) DO UPDATE \
                 SET paths = EXCLUDED.paths, strength = EXCLUDED.strength, source = EXCLUDED.source, \
                     labeled_by = EXCLUDED.labeled_by, updated_at = EXCLUDED.updated_at",
            )
            .bind(org.as_uuid())
            .bind(label.ticket.as_uuid())
            .bind(&paths)
            .bind(label.strength.as_str())
            .bind(label.source.as_str())
            .bind(now)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            Ok::<_, TxError<EvalStoreError>>(())
        })
        .await
    }

    async fn labels(&self, org: OrgId, tickets: &[TicketId]) -> Result<Vec<GroundTruth>, EvalStoreError> {
        let ids: Vec<Uuid> = tickets.iter().map(|t| t.as_uuid()).collect();
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let rows: Vec<LabelRow> = sqlx::query_as(
                "SELECT ticket_id, paths, strength, source FROM triage_labels WHERE org_id = $1 AND ticket_id = ANY($2) ORDER BY ticket_id",
            )
            .bind(org.as_uuid())
            .bind(&ids)
            .fetch_all(&mut *tx)
            .await?;
            tx.commit().await?;
            Ok::<_, TxError<EvalStoreError>>(rows.into_iter().map(LabelRow::into_label).collect::<Result<Vec<_>, _>>()?)
        })
        .await
    }

    async fn cases(&self, org: OrgId, eval: TriageEvalId) -> Result<Vec<EvalCase>, EvalStoreError> {
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let rows: Vec<(Uuid, Option<Uuid>, i64)> = sqlx::query_as(
                "SELECT ticket_id, (array_agg(input_run ORDER BY repeat))[1], count(*) \
                 FROM triage_eval_cases WHERE org_id = $1 AND eval_id = $2 GROUP BY ticket_id ORDER BY ticket_id",
            )
            .bind(org.as_uuid())
            .bind(eval.as_uuid())
            .fetch_all(&mut *tx)
            .await?;
            tx.commit().await?;
            rows.into_iter()
                .map(|(ticket, input_run, repeats)| {
                    Ok(EvalCase {
                        ticket: TicketId::from_uuid(ticket),
                        input_run: input_run.map(RunId::from_uuid),
                        repeats: to_unsigned(repeats, "triage_eval_cases repeats")?,
                    })
                })
                .collect::<Result<Vec<_>, TxError<EvalStoreError>>>()
        })
        .await
    }

    async fn case_run(&self, org: OrgId, eval: TriageEvalId, ticket: TicketId, repeat: u8) -> Result<Option<RunId>, EvalStoreError> {
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let row: Option<Option<Uuid>> = sqlx::query_scalar(
                "SELECT run_id FROM triage_eval_cases WHERE org_id = $1 AND eval_id = $2 AND ticket_id = $3 AND repeat = $4",
            )
            .bind(org.as_uuid())
            .bind(eval.as_uuid())
            .bind(ticket.as_uuid())
            .bind(i16::from(repeat))
            .fetch_optional(&mut *tx)
            .await?;
            tx.commit().await?;
            let run = row.ok_or(TxError::Port(EvalStoreError::NotFound))?;
            Ok::<_, TxError<EvalStoreError>>(run.map(RunId::from_uuid))
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use mage_core::testing::SeqIdGen;

    use super::*;
    use crate::testing::contract::{EvalFixture, contract_triage_eval_store};
    use crate::testing::pg::with_pg;

    #[tokio::test]
    async fn pg_triage_eval_store_passes_the_contract() {
        with_pg("pg_triage_eval_contract", |env| async move {
            contract_triage_eval_store(|clock| {
                let env = env.clone();
                async move {
                    let db = env.fresh().await;
                    EvalFixture { evals: Arc::new(PgTriageEvalStore::new(db.pools.clone(), clock.clone(), Arc::new(SeqIdGen::new()))), seed: db.seed(clock) }
                }
            })
            .await
        })
        .await;
    }
}
