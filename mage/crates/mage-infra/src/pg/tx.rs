//! Transactions: tenant scoping and retry (storage.md "Tenancy enforcement", "Adapter internals").
//!
//! Every tenant transaction starts with `SELECT set_config('mage.org_id', $org, true)`
//! (transaction-local, safe with pooling); without it `current_org()` is NULL and every RLS policy
//! of the tenant role matches no row. Isolation is READ COMMITTED everywhere; a serialization
//! failure or deadlock (`40001` / `40P01`) reruns the whole transaction, at most
//! [`TX_ATTEMPTS`] times in total.
//!
//! Shape of a store method:
//!
//! ```ignore
//! retry_tx(|| async {
//!     let mut tx = begin_org(&self.pools.tenant, org).await?;
//!     let updated = sqlx::query("UPDATE … WHERE org_id = $1 AND rev = $2").bind(…).execute(&mut *tx).await?;
//!     if updated.rows_affected() == 0 {
//!         return Err(TxError::Port(MyError::Conflict));
//!     }
//!     jobs::enqueue_all_in(&mut tx, jobs, now, &*self.ids).await?;
//!     tx.commit().await?;
//!     Ok(())
//! })
//! .await
//! ```

use std::time::Duration;

use mage_core::domain::OrgId;
use sqlx::{PgConnection, PgPool, Postgres};

use super::error::DbError;

/// A pooled transaction.
pub type Tx = sqlx::Transaction<'static, Postgres>;

/// Attempts of one transaction under [`retry_tx`] (the first run plus two reruns).
pub const TX_ATTEMPTS: u32 = 3;

/// Begin a tenant transaction scoped to `org` (RLS `current_org()`).
pub async fn begin_org(pool: &PgPool, org: OrgId) -> Result<Tx, DbError> {
    let mut tx = pool.begin().await?;
    set_org(&mut tx, org).await?;
    Ok(tx)
}

/// Begin a platform transaction (platform pool; no tenant scope).
pub async fn begin_platform(pool: &PgPool) -> Result<Tx, DbError> {
    Ok(pool.begin().await?)
}

/// Scope the current transaction of `conn` to `org` (what [`begin_org`] runs after `BEGIN`).
pub async fn set_org(conn: &mut PgConnection, org: OrgId) -> Result<(), DbError> {
    sqlx::query("SELECT set_config('mage.org_id', $1, true)").bind(org.as_uuid().to_string()).execute(conn).await?;
    Ok(())
}

/// `SELECT pg_notify($channel, $payload)`: delivered at commit, dropped on rollback.
pub async fn notify(conn: &mut PgConnection, channel: &str, payload: &str) -> Result<(), DbError> {
    sqlx::query("SELECT pg_notify($1, $2)").bind(channel).bind(payload).execute(conn).await?;
    Ok(())
}

/// Failure inside a transaction body: a database error (classified, maybe retried) or the port's
/// own error (returned as is).
#[derive(Debug)]
pub enum TxError<E> {
    Db(DbError),
    Port(E),
}

impl<E> From<sqlx::Error> for TxError<E> {
    fn from(err: sqlx::Error) -> Self {
        Self::Db(err.into())
    }
}

impl<E> From<DbError> for TxError<E> {
    fn from(err: DbError) -> Self {
        Self::Db(err)
    }
}

/// Run `body` (which begins, works and commits its own transaction) and rerun it on
/// [`DbError::Retry`] with a short jittered pause, up to [`TX_ATTEMPTS`] runs; the last retryable
/// failure becomes `Unavailable`. Other database errors map through `E: From<DbError>`.
pub async fn retry_tx<T, E, F, Fut>(mut body: F) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, TxError<E>>>,
    E: From<DbError>,
{
    let mut attempt = 1;
    loop {
        match body().await {
            Ok(value) => return Ok(value),
            Err(TxError::Port(err)) => return Err(err),
            Err(TxError::Db(DbError::Retry(msg))) if attempt < TX_ATTEMPTS => {
                tracing::debug!(attempt, error = %msg, "retrying transaction after serialization failure");
                tokio::time::sleep(retry_pause(attempt)).await;
                attempt += 1;
            }
            Err(TxError::Db(DbError::Retry(msg))) => {
                return Err(E::from(DbError::Unavailable(format!("{msg} (after {TX_ATTEMPTS} attempts)"))));
            }
            Err(TxError::Db(err)) => return Err(E::from(err)),
        }
    }
}

/// `10 ms · 2^(attempt−1)`, scaled by a random factor in `[0.5, 1.5)`.
fn retry_pause(attempt: u32) -> Duration {
    let base = Duration::from_millis(10) * (1u32 << attempt.saturating_sub(1).min(8));
    let random = uuid::Uuid::new_v4().as_u128() as u64;
    let factor = 0.5 + (random >> 11) as f64 / (1u64 << 53) as f64;
    base.mul_f64(factor)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    #[tokio::test]
    async fn retry_reruns_serialization_failures_then_gives_up() {
        let runs = AtomicU32::new(0);
        let out: Result<u32, DbError> = retry_tx(|| async {
            let n = runs.fetch_add(1, Ordering::SeqCst) + 1;
            if n < 3 { Err(TxError::Db(DbError::Retry("40001".into()))) } else { Ok(n) }
        })
        .await;
        assert_eq!(out, Ok(3), "the third run succeeds");

        runs.store(0, Ordering::SeqCst);
        let out: Result<(), DbError> = retry_tx(|| async {
            runs.fetch_add(1, Ordering::SeqCst);
            Err(TxError::Db(DbError::Retry("40P01".into())))
        })
        .await;
        assert!(matches!(out, Err(DbError::Unavailable(_))), "{out:?}");
        assert_eq!(runs.load(Ordering::SeqCst), TX_ATTEMPTS);
    }

    #[tokio::test]
    async fn port_and_other_db_errors_are_not_retried() {
        let runs = AtomicU32::new(0);
        let out: Result<(), DbError> = retry_tx(|| async {
            runs.fetch_add(1, Ordering::SeqCst);
            Err(TxError::Db(DbError::conflict()))
        })
        .await;
        assert_eq!((out, runs.load(Ordering::SeqCst)), (Err(DbError::conflict()), 1));
        #[derive(Debug, PartialEq)]
        enum PortError {
            Db(DbError),
            CasMiss,
        }
        impl From<DbError> for PortError {
            fn from(err: DbError) -> Self {
                Self::Db(err)
            }
        }
        let out: Result<(), PortError> = retry_tx(|| async { Err(TxError::Port(PortError::CasMiss)) }).await;
        assert_eq!(out, Err(PortError::CasMiss));
        let out: Result<(), PortError> = retry_tx(|| async { Err(TxError::Db(DbError::NotFound)) }).await;
        assert_eq!(out, Err(PortError::Db(DbError::NotFound)));
    }
}
