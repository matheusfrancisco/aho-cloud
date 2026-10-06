//! Start-time schema check (storage.md "Migrations" → deploy order): a database behind the binary
//! or with an edited migration refuses start; a database ahead of the binary starts.
//! `DATABASE_URL`-gated; see `support` for the harness.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use mage_infra::pg::DbError;
use mage_infra::pg::pool::{SchemaStatus, check_schema};
use support::with_test_db;

#[tokio::test]
async fn schema_check_refuses_a_database_behind_the_binary() {
    with_test_db("schema_check_refuses_a_database_behind_the_binary", |db| async move {
        assert_eq!(check_schema(&db.owner).await, Ok(SchemaStatus::Current));

        sqlx::query("INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) VALUES (99990101000000, 'future', true, '\\x00', 1)")
            .execute(&db.owner)
            .await
            .unwrap();
        assert_eq!(check_schema(&db.owner).await, Ok(SchemaStatus::Ahead { unknown: vec![99990101000000] }), "expand/contract: newer schema starts");

        let last: i64 = sqlx::query_scalar("SELECT max(version) FROM _sqlx_migrations WHERE version < 99990101000000").fetch_one(&db.owner).await.unwrap();
        sqlx::query("UPDATE _sqlx_migrations SET checksum = '\\x00' WHERE version = $1").bind(last).execute(&db.owner).await.unwrap();
        assert!(matches!(check_schema(&db.owner).await, Err(DbError::Invalid(msg)) if msg.contains("edited")), "checksum mismatch");

        sqlx::query("DELETE FROM _sqlx_migrations WHERE version = $1").bind(last).execute(&db.owner).await.unwrap();
        assert!(matches!(check_schema(&db.owner).await, Err(DbError::Invalid(msg)) if msg.contains("behind")), "missing migration");

        sqlx::raw_sql("DROP TABLE _sqlx_migrations").execute(&db.owner).await.unwrap();
        assert!(matches!(check_schema(&db.owner).await, Err(DbError::Invalid(msg)) if msg.contains("no migrations")), "never migrated");
    })
    .await;
}
