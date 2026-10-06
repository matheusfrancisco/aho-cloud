//! `mage_core::testing`: in-memory fakes for every core port and the generic port contract suites
//! (README "Testing", storage.md and knowledge.md "Testing"). Feature `testing`.
//!
//! Fakes:
//!
//! | Port | Fake |
//! |---|---|
//! | `Clock` | [`FakeClock`] (settable) |
//! | `IdGen` | [`SeqIdGen`] (deterministic) |
//! | `JobQueue` | [`MemJobQueue`] |
//! | `RepoStore` | [`MemRepoStore`] (outbox into a `MemJobQueue`) |
//! | `KnowledgeStore` | [`MemKnowledgeStore`] (reads version revs from a `MemRepoStore`) |
//! | `ArtifactStore` | [`MemArtifactStore`] (signed URLs redeemable in tests) |
//! | `SourceHost` | [`FakeSourceHost`] (scripted from a [`SourceHostWorld`]) |
//!
//! Contract suites (`contract_*`) take an async factory so every scenario starts from an empty
//! store driven by the suite's [`FakeClock`]; real adapters (`mage_infra`) run the same suites.
//! A suite panics on the first violated expectation.
//!
//! The artifact fake's `put_file` / `get_to_file` touch the local filesystem, like the real
//! adapter; everything else is in memory.

mod blob;
mod clock;
mod contract;
mod jobs;
mod knowledge;
mod page;
mod repos;
mod source_host;
mod sync;

pub use blob::{MemArtifactStore, SignedUseError};
pub use clock::{FakeClock, SeqIdGen};
pub use contract::{
    KnowledgeFixture, RepoStoreFixture, SourceHostWorld, WorldInstallation, WorldPullRequest, WorldRepo, contract_artifact_store,
    contract_job_queue, contract_knowledge_store, contract_repo_store, contract_source_host,
};
pub use jobs::MemJobQueue;
pub use knowledge::MemKnowledgeStore;
pub use repos::MemRepoStore;
pub use source_host::{FakeSourceHost, SourceHostCall, SourceHostMethod};
pub use sync::join_all;
