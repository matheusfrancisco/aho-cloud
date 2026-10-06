//! Intake (intake.md): webhooks → inbox → jobs → tickets, repo versions and deployments; the
//! one outbound path to Linear and Slack (`notify.ticket`); the tickets API use cases.
//!
//! | Module | Use case |
//! |---|---|
//! | [`accept`] | 1 AcceptDelivery (HTTP `/hooks/*`) |
//! | [`deliver`] | 2 ProcessDelivery (`intake.github`, `intake.github_fix`, `intake.linear`, `intake.slack`) |
//! | [`linear_issue`] | 3 IngestLinearIssue (`intake.linear_issue`) with [`normalize`], [`extract`], [`target`] |
//! | [`github`] | 4 IngestGithubPush, 5 IngestDeployment |
//! | [`lifecycle`] | 6 BindIntegration and installation / revocation lifecycle |
//! | [`reconcile`] | 7 resync, redeliver, inbox purge |
//! | [`notify`] | 8 Notify (`notify.ticket`) with [`templates`] |
//! | [`tickets`] | tickets API (list, detail, target override, resolve) |

pub mod accept;
pub mod config;
pub mod deliver;
pub mod error;
pub mod extract;
pub mod github;
pub mod lifecycle;
pub mod linear_issue;
pub mod normalize;
pub mod notify;
pub mod reconcile;
pub mod target;
pub mod templates;
pub mod tickets;
#[cfg(test)]
pub(crate) mod testkit;

pub use accept::{AcceptDelivery, AcceptOutcome, InboundDelivery};
pub use config::{GithubInstallSeed, IntakeMode, IntakeSection};
pub use error::IntakeError;
pub use target::DeploymentChain;
