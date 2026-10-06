//! Generic contract suites for the server store ports (storage.md / server.md / intake.md /
//! triage.md "Testing"): one suite per port, run against the in-memory fake (in the suite's own
//! tests) and against the pg adapter (in `adapters::driven::pg::<port>` tests, opt-in via
//! `DATABASE_URL`).
//!
//! Every suite is `contract_<port>(make)` where `make(clock)` returns a fixture over a fresh,
//! empty store wired to the given `FakeClock`; the suite calls it once per scenario, moves the
//! clock itself, and panics on the first violated expectation. Fixtures carry a
//! [`crate::testing::Seed`] for rows the port does not write (pg foreign keys).
//! Append `pub mod` / `pub use` lines, never remove others'.

#![allow(clippy::unwrap_used, clippy::expect_used)]

pub mod audit;
pub mod feed;
pub use audit::{AuditFixture, contract_audit_log};
pub use feed::{FeedFixture, contract_change_feed};
pub mod inbox;
pub mod outbound;
pub mod triage_eval;
pub use inbox::{InboxFixture, contract_webhook_inbox};
pub use outbound::{OutboundFixture, contract_outbound_log};
pub use triage_eval::{EvalFixture, contract_triage_eval_store};
pub mod org;
pub use org::{OrgFixture, contract_org_store};
pub mod secrets;
pub mod tokens;
pub use secrets::{SecretFixture, contract_secret_store};
pub use tokens::{TokenFixture, contract_token_store};
pub mod ticket;
pub use ticket::{TicketFixture, contract_ticket_store};
