//! Postgres adapters (storage.md "Adapter internals"): migrations, pools, transaction and error
//! helpers shared with the server's pg adapters, the transactional outbox, and the core stores
//! `PgJobQueue`, `PgRepoStore`, `PgKnowledgeStore`.
//!
//! | Module | Contents |
//! |---|---|
//! | [`schema`] | embedded migrations (`MIGRATOR`, `migrate`) |
//! | [`pool`] | `PgPools { tenant, platform }`, `PgConfig`, `connect`, `check_schema` |
//! | [`tx`] | `begin_org` / `begin_platform`, `retry_tx`, `notify` |
//! | [`error`] | `DbError` (sqlx → port errors) |
//! | [`jobs`] | `PgJobQueue`, outbox `enqueue_in` / `enqueue_all_in` |
//! | [`repo`] | `PgRepoStore` |
//! | [`knowledge`] | `PgKnowledgeStore` |
//!
//! Rules every pg adapter follows: time from the `Clock` (SQL never calls `now()`), ids from
//! `IdGen`, tenant statements inside `begin_org` and filtered by `org_id`, runtime-checked
//! `sqlx::query` with bind parameters only.

pub mod error;
pub mod jobs;
pub mod knowledge;
pub mod pool;
pub mod repo;
pub mod schema;
pub mod tx;

pub use error::DbError;
pub use jobs::PgJobQueue;
pub use knowledge::PgKnowledgeStore;
pub use pool::{PgConfig, PgPools};
pub use repo::PgRepoStore;
pub use schema::MIGRATOR;
pub use tx::{Tx, TxError, begin_org, begin_platform, notify, retry_tx};
