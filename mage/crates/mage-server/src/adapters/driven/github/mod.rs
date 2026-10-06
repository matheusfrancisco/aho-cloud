//! Server-side GitHub adapters on the shared `mage_infra::github::GitHubClient` (intake.md
//! "`mage_infra::github` … and `mage_server::adapters::driven::github`"): `SourceHostAdmin`,
//! the live `DeploymentResolver`, the GitHub `WebhookDecoder`, and fix.md's `PullRequests`.
//! Append `pub mod` lines, never remove others'.

pub mod admin;
pub mod deployments;
pub mod webhook;
pub mod pull_requests;
