//! UC `index.bundle_source` (knowledge.md "SourceBundle"): the deterministic tar.zst of the tree
//! at a sha, shared by identical trees, recorded on the version with `runs.source_ready` in the
//! same transaction. A bundle over `indexer.bundle_max_bytes` fails permanently; the dead-letter
//! path still enqueues `runs.source_ready` so waiting runs error out.

use std::path::{Path, PathBuf};

use mage_core::domain::SourceBundleRef;
use mage_core::ports::blob::{BlobKey, PutOptions};

use super::{IndexPipeline, IndexTarget, JobDone};
use crate::error::AppError;

pub const BUNDLE_CONTENT_TYPE: &str = "application/zstd";

/// Removes the staged file however the job ends.
struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        match std::fs::remove_file(&self.0) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => tracing::warn!(path = %self.0.display(), error = %err, "bundle staging file not removed"),
        }
    }
}

impl IndexPipeline {
    pub async fn bundle_source(&self, t: IndexTarget) -> Result<JobDone, AppError> {
        let (org, key, m) = (t.org, t.key(), t.mirror());
        let commit = self.deps.git.commit(&m, t.sha).await?;
        let notify = [t.source_ready_job()];

        if let Some(bundle) = self.deps.repos.bundle_for_tree(org, t.repo, commit.tree).await? {
            self.deps.repos.set_source_bundle(org, key, &bundle, &notify).await?;
            return Ok(JobDone::done());
        }

        let staging = self.config.data_dir.join("tmp");
        tokio::fs::create_dir_all(&staging).await.map_err(|e| io_error(&staging, &e))?;
        let tmp = TempFile(staging.join(format!("{}.tar.zst", self.deps.ids.uuid_v4())));
        let info = self.deps.git.archive(&m, t.sha, &tmp.0).await?;
        if info.bytes > self.config.bundle_max_bytes {
            return Err(AppError::Invalid(format!(
                "source bundle of {key} is {} bytes (cap indexer.bundle_max_bytes = {})",
                info.bytes, self.config.bundle_max_bytes
            )));
        }
        let object = BlobKey::source_bundle(org, t.repo, info.tree);
        let opts = PutOptions { content_type: BUNDLE_CONTENT_TYPE.to_owned(), if_absent: true };
        self.deps.artifacts.put_file(&object, &tmp.0, &opts).await?;
        let bundle = SourceBundleRef { key: object.as_str().to_owned(), tree: info.tree, bytes: info.bytes, sha256: info.sha256 };
        self.deps.repos.set_source_bundle(org, key, &bundle, &notify).await?;
        tracing::info!(org = %org, %key, tree = %info.tree, bytes = info.bytes, entries = info.entries, "source bundle stored");
        Ok(JobDone::done())
    }
}

fn io_error(path: &Path, err: &std::io::Error) -> AppError {
    AppError::Unavailable(format!("{}: {err}", path.display()))
}
