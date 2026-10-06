//! UC `index.parse` (layer `Symbols`; knowledge.md "UC `index.parse`"): parse one batch of blobs,
//! write facts plus `Parse(rev)` markers, and mark the batch done; the store completes `Symbols`
//! and enqueues `index.error_sites` + `index.app_map` when the last batch finishes.
//!
//! Per-blob failures (unsupported, too large, timeout, grammar panic) become `Failed` markers and
//! never fail the job; a whole-batch failure (store or mirror down) fails it.

use std::collections::HashMap;
use std::time::Duration;

use mage_core::domain::{BlobOid, Layer, RepoPath};
use mage_core::ports::knowledge::{BlobInput, ParseLimits, ParsedBlob};

use super::layers::switch_rev;
use super::manifest::{LANGUAGE_HEAD_BYTES, PARSE_MAX_BLOB_BYTES};
use super::{IndexPipeline, IndexTarget, JobDone};
use crate::error::AppError;
use crate::ports::git::TreeEntryKind;

/// knowledge.md step 1.
pub const PARSE_LIMITS: ParseLimits =
    ParseLimits { max_bytes: PARSE_MAX_BLOB_BYTES, timeout: Duration::from_secs(2), max_symbols: 20_000, max_refs: 100_000, max_literals: 5_000 };

impl IndexPipeline {
    pub async fn parse(&self, t: IndexTarget, batch: u32, oids: &[BlobOid]) -> Result<JobDone, AppError> {
        let (org, key, m) = (t.org, t.key(), t.mirror());
        let parser = self.deps.parser.as_ref();
        let rev = parser.rev();

        // The payload carries oids only: paths (and so languages) come from the tree at the sha.
        let listing = self.deps.git.list_tree(&m, t.sha).await?;
        let mut paths: HashMap<BlobOid, &RepoPath> = HashMap::with_capacity(oids.len());
        for entry in &listing.entries {
            if let TreeEntryKind::File { oid, .. } = entry.kind {
                paths.entry(oid).or_insert(&entry.path);
            }
        }
        let reads = self.deps.git.read_blobs(&m, oids, PARSE_LIMITS.max_bytes).await?;

        let mut results: Vec<(BlobOid, Result<ParsedBlob, String>)> = Vec::with_capacity(reads.len());
        let mut inputs: Vec<BlobInput> = Vec::with_capacity(reads.len());
        for read in reads {
            let prepared = match (paths.get(&read.oid), read.bytes) {
                (None, _) => Err(format!("blob {} is not a file at {}", read.oid, t.sha)),
                (Some(_), None) => Err(format!("blob larger than {} bytes", PARSE_LIMITS.max_bytes)),
                (Some(path), Some(bytes)) => match parser.detect_language(path, &bytes[..bytes.len().min(LANGUAGE_HEAD_BYTES)]) {
                    Some(lang) if parser.languages().contains(&lang) => Ok(BlobInput { oid: read.oid, path: (*path).clone(), lang, bytes }),
                    Some(lang) => Err(format!("no extraction for {lang:?}")),
                    None => Err("unknown language".to_owned()),
                },
            };
            match prepared {
                Ok(input) => inputs.push(input),
                Err(reason) => results.push((read.oid, Err(reason))),
            }
        }
        let input_oids: Vec<BlobOid> = inputs.iter().map(|input| input.oid).collect();
        let parsed = parser.parse(inputs, PARSE_LIMITS).await;
        if parsed.len() != input_oids.len() {
            return Err(AppError::Internal(format!("parser returned {} results for {} blobs", parsed.len(), input_oids.len())));
        }
        results.extend(input_oids.into_iter().zip(parsed).map(|(oid, result)| (oid, result.map_err(|e| e.to_string()))));
        let failed = results.iter().filter(|(_, r)| r.is_err()).count();
        self.deps.knowledge.put_parsed(org, rev, &results).await?;

        let remaining = self.deps.repos.finish_layer_batch(org, key, Layer::Symbols, batch, &t.after_symbols()).await?;
        tracing::debug!(org = %org, %key, batch, blobs = results.len(), failed, remaining, "parse batch done");
        if remaining == 0 {
            switch_rev(self.deps.repos.as_ref(), org, key, self.current_revs(), |revs| revs.parser = rev).await?;
        }
        Ok(JobDone::done())
    }
}
