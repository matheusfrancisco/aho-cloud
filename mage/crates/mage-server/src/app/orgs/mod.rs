//! Orgs and people (server.md "Org selection", "RBAC", UC2, UC4, audit and usage reads):
//!
//! - [`directory`]: session view, org selection (`OrgCtx` resolution), short-link resolution.
//! - [`members`]: members and invites (UC4), role changes, removal.
//! - [`create`]: org creation (operator CLI in v1; `POST /api/v1/orgs` on the public product).
//! - [`activity`]: audit log and usage reports.

pub mod activity;
pub mod create;
pub mod directory;
pub mod members;

pub use activity::Activity;
pub use create::{CreateOrg, OrgOwner};
pub use directory::{Directory, OrgAccess};
pub use members::Members;
