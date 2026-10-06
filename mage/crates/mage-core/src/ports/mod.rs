//! `mage_core::ports`: ports used by more than one role (docs/internals/README.md "Port catalog").
//!
//! Conventions (README "Port conventions"): async ports are `#[async_trait]`, `Send + Sync`, held
//! as `Arc<dyn Port>`; every store method takes `OrgId` explicitly; each port has its own
//! `thiserror` error enum with at least `NotFound`, `Conflict`, `Invalid(String)`,
//! `Unavailable(String)`. Time comes from [`Clock`], ids from [`IdGen`]. In-memory fakes and the
//! generic contract suites live in `mage_core::testing` (feature `testing`).
//!
//! | Module | Port | Owner doc |
//! |---|---|---|
//! | [`clock`] | `Clock` (+ `SystemClock`) | README |
//! | [`ids`] | `IdGen` (+ `UuidV7Gen`) | README |
//! | [`common`] | `PageRequest`, `Page`, `Cursor`, `Versioned` | storage.md |
//! | [`jobs`] | `JobQueue` | storage.md |
//! | [`repos`] | `RepoStore` | storage.md |
//! | [`blob`] | `ArtifactStore` | storage.md |
//! | [`knowledge`] | `KnowledgeStore` | knowledge.md |
//! | [`source_host`] | `SourceHost` | intake.md |

pub mod blob;
pub mod clock;
pub mod common;
pub mod ids;
pub mod jobs;
pub mod knowledge;
pub mod repos;
pub mod source_host;

pub use blob::{ArtifactStore, BlobError, BlobKey};
pub use clock::{Clock, SystemClock};
pub use common::{Cursor, Page, PageRequest, Versioned};
pub use ids::{IdGen, UuidV7Gen};
pub use jobs::{JobError, JobQueue, NewJob};
pub use knowledge::{KnowledgeError, KnowledgeStore};
pub use repos::{RepoStore, RepoStoreError};
pub use source_host::{SourceHost, SourceHostError};
