//! Shared driven adapters implementing `mage-core` ports, used by the server and the indexer
//! (never by the runner):
//!
//! | Module | Contents |
//! |---|---|
//! | [`pg`] | migrations, pools, tenant transactions, error mapping, outbox, `PgJobQueue`, `PgRepoStore`, `PgKnowledgeStore` |
//! | [`blob`] | `ObjectBlobs` (`ArtifactStore` over GCS / S3 / local FS / memory), signed URLs, dev blob routes |
//! | [`github`] | GitHub App client (JWT, installation-token cache, REST) and `GitHubSourceHost` |
//! | [`config`] | `SecretSource`, TOML sections with `MAGE_*` overrides, inline-secret refusal |
//! | [`telemetry`] | JSON / text log subscriber with span fields, trace correlation and redaction |

pub mod blob;
pub mod config;
pub mod github;
pub mod pg;
pub mod telemetry;
