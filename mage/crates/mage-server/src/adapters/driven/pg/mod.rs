//! Server-only Postgres adapters (storage.md "`mage_server::adapters::driven::pg`"). They reuse
//! `mage_infra::pg` (pools, `begin_org` / `begin_platform`, `retry_tx`, `DbError`, outbox
//! `jobs::enqueue_all_in`) and never run migrations. Every store adapter is built with
//! `new(pools, clock, ids)`; tenant methods run on `pools.tenant` inside `begin_org`, the
//! platform methods listed in storage.md "Tenancy enforcement" on `pools.platform`.
//! Append `pub mod` lines, never remove others'.

pub(crate) mod codec;

pub mod deployments_manual;
pub mod signals;
pub mod feed;
pub use feed::PgChangeFeed;
pub mod audit;
pub use audit::PgAuditLog;
pub mod inbox;
pub mod outbound;
pub mod triage_eval;
pub use inbox::PgWebhookInbox;
pub use outbound::PgOutboundLog;
pub use triage_eval::PgTriageEvalStore;
pub mod org;
pub use org::PgOrgStore;
pub mod runs;
pub use runs::PgRunStore;
pub use signals::{PgRunSignals, SignalHub};
pub mod secrets;
pub mod tokens;
pub use secrets::PgSecretStore;
pub use tokens::PgTokenStore;
pub mod ticket;
pub use ticket::PgTicketStore;
