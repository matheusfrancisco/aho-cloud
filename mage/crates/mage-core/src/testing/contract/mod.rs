//! Generic port contract suites (README "Port contract tests"): one suite per core port, run
//! against the in-memory fake here and against every real adapter in `mage_infra`.
//!
//! Every suite takes an async factory `make(clock)` that must return a fresh, empty store wired to
//! the given [`FakeClock`]; the suite calls it once per scenario and moves the clock itself.
//!
//! The suites are test code: they `expect` / `unwrap` and panic on the first violation.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use crate::ports::jobs::JobQueue;
use crate::ports::knowledge::KnowledgeStore;
use crate::ports::repos::RepoStore;

mod blob;
mod jobs;
mod knowledge;
mod repos;
mod source_host;

pub use blob::contract_artifact_store;
pub use jobs::contract_job_queue;
pub use knowledge::contract_knowledge_store;
pub use repos::contract_repo_store;
pub use source_host::{SourceHostWorld, WorldInstallation, WorldPullRequest, WorldRepo, contract_source_host};

#[cfg(doc)]
use crate::testing::FakeClock;

/// A `RepoStore` and the `JobQueue` its outbox writes to (one database for real adapters).
#[derive(Clone)]
pub struct RepoStoreFixture {
    pub repos: Arc<dyn RepoStore>,
    pub jobs: Arc<dyn JobQueue>,
}

/// A `KnowledgeStore` and the `RepoStore` holding the versions' revs (one database for real
/// adapters).
#[derive(Clone)]
pub struct KnowledgeFixture {
    pub knowledge: Arc<dyn KnowledgeStore>,
    pub repos: Arc<dyn RepoStore>,
}
