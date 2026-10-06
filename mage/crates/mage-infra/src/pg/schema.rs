//! Embedded Postgres migrations: every table of every role lives in
//! `crates/mage-infra/migrations/` (storage.md "Migrations"). Forward-only; an applied file is
//! never edited (sqlx checksums each version in `_sqlx_migrations`).

use sqlx::PgPool;
use sqlx::migrate::{MigrateError, Migrator};

/// All migrations, embedded at compile time.
pub static MIGRATOR: Migrator = sqlx::migrate!("./migrations");

/// Applies every pending migration. sqlx holds a Postgres advisory lock for the run, so concurrent
/// callers serialize; already-applied versions are skipped after their checksum is verified.
pub async fn migrate(pool: &PgPool) -> Result<(), MigrateError> {
    MIGRATOR.run(pool).await
}
