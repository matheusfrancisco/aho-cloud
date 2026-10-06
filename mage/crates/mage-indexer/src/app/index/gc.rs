//! UC `index.gc` (knowledge.md "Reconcile and GC", retention table in "Which shas get indexed"),
//! daily: delete versions past retention (facts, pin, version row, bundle object when no other
//! version shares its tree), sweep unreferenced blob contents per org, run mirror maintenance
//! weekly per repo, and remove mirrors of deleted repos.

use std::collections::{BTreeSet, HashSet};
use std::time::Duration;

use mage_core::domain::{CommitSha, JobPayload, OrgId, RepoId, RepoVersion, VersionReason};
use mage_core::ports::blob::BlobKey;
use mage_core::ports::jobs::NewJob;
use mage_core::ports::repos::{RepoStoreError, VersionReasonRecord};
use time::OffsetDateTime;

use super::{IndexPipeline, JobDone};
use crate::error::AppError;
use crate::ports::git::{GitError, MirrorRef};

/// Period of the self-rescheduling chain.
pub const INTERVAL: time::Duration = time::Duration::days(1);
pub const ON_DEMAND_RETENTION: time::Duration = time::Duration::days(90);
pub const DEPLOYED_RETENTION: time::Duration = time::Duration::days(90);
pub const OLD_HEAD_RETENTION: time::Duration = time::Duration::days(7);
pub const RELEASES_KEPT: usize = 10;
/// Unreferenced blobs younger than this survive the sweep (a manifest may be in flight).
pub const BLOB_GRACE: Duration = Duration::from_secs(24 * 60 * 60);
const SWEEP_BATCH: u32 = 1000;
const SWEEP_ROUNDS: usize = 100;
/// Each repo's mirror is maintained on one day of the week.
const MAINTAIN_EVERY_DAYS: i64 = 7;

/// Shas of a repo that its reasons retain at `now`:
/// - `OnDemand`: 90 days after the reason was added;
/// - `Deployed(env)`: while current, then 90 days after it ended;
/// - `DefaultHead`: the current head, the previous head, older heads 7 days after they ended;
/// - `Release(tag)`: the 10 most recently added releases.
pub fn retained(records: &[VersionReasonRecord], now: OffsetDateTime) -> BTreeSet<CommitSha> {
    let mut keep = BTreeSet::new();
    let mut previous_head: Option<&VersionReasonRecord> = None;
    let mut releases: Vec<&VersionReasonRecord> = Vec::new();
    for record in records {
        match (&record.reason, record.ended_at) {
            (VersionReason::OnDemand(_), _) => {
                if record.added_at + ON_DEMAND_RETENTION > now {
                    keep.insert(record.sha);
                }
            }
            (VersionReason::Deployed(_), None) | (VersionReason::DefaultHead, None) => {
                keep.insert(record.sha);
            }
            (VersionReason::Deployed(_), Some(ended)) => {
                if ended + DEPLOYED_RETENTION > now {
                    keep.insert(record.sha);
                }
            }
            (VersionReason::DefaultHead, Some(ended)) => {
                if ended + OLD_HEAD_RETENTION > now {
                    keep.insert(record.sha);
                }
                if previous_head.is_none_or(|p| p.ended_at < Some(ended)) {
                    previous_head = Some(record);
                }
            }
            (VersionReason::Release(_), _) => releases.push(record),
        }
    }
    if let Some(previous) = previous_head {
        keep.insert(previous.sha);
    }
    releases.sort_by(|a, b| b.added_at.cmp(&a.added_at).then(b.sha.cmp(&a.sha)));
    let mut kept_releases = HashSet::new();
    for release in releases {
        if kept_releases.len() == RELEASES_KEPT {
            break;
        }
        kept_releases.insert(release.sha);
    }
    keep.extend(kept_releases);
    keep
}

impl IndexPipeline {
    pub async fn gc(&self) -> Result<JobDone, AppError> {
        let now = self.deps.clock.now();
        let active = self.deps.repos.active_repos_system().await?;
        let mut orgs = BTreeSet::new();
        for (org, repo) in &active {
            orgs.insert(*org);
            if let Err(err) = self.gc_repo(*org, *repo, now).await {
                tracing::warn!(org = %org, repo = %repo, error = %err, "gc skipped repo");
            }
        }
        for org in orgs {
            self.sweep_blobs(org).await?;
        }
        self.remove_orphan_mirrors(&active).await?;
        Ok(JobDone::Complete(vec![NewJob::new(JobPayload::IndexGc).after(now + INTERVAL)]))
    }

    async fn gc_repo(&self, org: OrgId, repo: RepoId, now: OffsetDateTime) -> Result<(), AppError> {
        let repos = self.deps.repos.as_ref();
        let m = MirrorRef::new(org, repo);
        let keep = retained(&repos.version_reasons(org, repo).await?, now);
        let versions = repos.versions(org, repo).await?;
        let (kept, expired): (Vec<RepoVersion>, Vec<RepoVersion>) = versions.into_iter().partition(|v| keep.contains(&v.sha));
        let mut bundle_trees: HashSet<_> = kept.iter().filter_map(|v| v.bundle.as_ref().map(|b| b.tree)).collect();
        for version in &expired {
            let key = version.key();
            self.deps.knowledge.delete_version_facts(org, &key).await?;
            ignore_missing(self.deps.git.unpin(&m, version.sha).await)?;
            repos.delete_version(org, key).await?;
            tracing::info!(org = %org, %key, "version past retention deleted");
        }
        // Bundles of deleted versions whose tree no retained version shares.
        for version in &expired {
            let Some(bundle) = &version.bundle else { continue };
            if bundle_trees.insert(bundle.tree) {
                self.deps.artifacts.delete(&BlobKey::parse(&bundle.key)?).await?;
            }
        }
        let day = now.unix_timestamp().div_euclid(86_400);
        let slot = i64::from(repo.as_uuid().as_u128().to_le_bytes()[0]);
        if (day + slot).rem_euclid(MAINTAIN_EVERY_DAYS) == 0 {
            ignore_missing(self.deps.git.maintain(&m).await)?;
        }
        Ok(())
    }

    /// `sweep_unreferenced_blobs` rounds; each returned oid's content object is deleted.
    async fn sweep_blobs(&self, org: OrgId) -> Result<(), AppError> {
        for _ in 0..SWEEP_ROUNDS {
            let oids = self.deps.knowledge.sweep_unreferenced_blobs(org, BLOB_GRACE, SWEEP_BATCH).await?;
            for oid in &oids {
                self.deps.artifacts.delete(&BlobKey::blob_content(org, *oid)).await?;
            }
            if oids.len() < SWEEP_BATCH as usize {
                break;
            }
        }
        Ok(())
    }

    /// Mirrors whose repo row is gone (repos that only lost access keep theirs: a mirror is a
    /// cache and access may come back).
    async fn remove_orphan_mirrors(&self, active: &[(OrgId, RepoId)]) -> Result<(), AppError> {
        let active: HashSet<(OrgId, RepoId)> = active.iter().copied().collect();
        for m in self.deps.git.list_mirrors().await? {
            if active.contains(&(m.org, m.repo)) {
                continue;
            }
            match self.deps.repos.repo(m.org, m.repo).await {
                Err(RepoStoreError::NotFound) => {
                    self.deps.git.remove_mirror(&m).await?;
                    tracing::info!(org = %m.org, repo = %m.repo, "mirror of deleted repo removed");
                }
                Ok(_) => {}
                Err(err) => return Err(err.into()),
            }
        }
        Ok(())
    }
}

fn ignore_missing(result: Result<(), GitError>) -> Result<(), AppError> {
    match result {
        Ok(()) | Err(GitError::NotFound) => Ok(()),
        Err(err) => Err(err.into()),
    }
}

#[cfg(test)]
mod tests {
    use mage_core::domain::TicketId;
    use time::macros::datetime;
    use uuid::Uuid;

    use super::*;

    fn sha(n: u8) -> CommitSha {
        CommitSha::from_bytes([n; 20])
    }

    fn rec(n: u8, reason: VersionReason, added: OffsetDateTime, ended: Option<OffsetDateTime>) -> VersionReasonRecord {
        VersionReasonRecord { sha: sha(n), reason, added_at: added, ended_at: ended }
    }

    #[test]
    fn retention_per_reason() {
        let now = datetime!(2026-10-05 12:00 UTC);
        let days = |d: i64| now - time::Duration::days(d);
        let ticket = TicketId::from_uuid(Uuid::from_u128(9));
        let mut records = vec![
            rec(1, VersionReason::OnDemand(ticket), days(89), None),
            rec(2, VersionReason::OnDemand(ticket), days(91), None),
            rec(3, VersionReason::Deployed("prod".into()), days(400), None),
            rec(4, VersionReason::Deployed("prod".into()), days(200), Some(days(89))),
            rec(5, VersionReason::Deployed("staging".into()), days(200), Some(days(91))),
            rec(6, VersionReason::DefaultHead, days(1), None),
            rec(7, VersionReason::DefaultHead, days(30), Some(days(20))),
            rec(8, VersionReason::DefaultHead, days(40), Some(days(30))),
            rec(9, VersionReason::DefaultHead, days(10), Some(days(6))),
        ];
        // 12 releases, one added per day: the 10 newest survive.
        for i in 0..12u8 {
            records.push(rec(20 + i, VersionReason::Release(format!("v{i}")), days(i64::from(40 - i)), None));
        }
        let keep = retained(&records, now);
        let expected: BTreeSet<CommitSha> = [1, 3, 4, 6, 9].into_iter().chain(22..32).map(sha).collect();
        assert_eq!(keep, expected);

        // Without a recently ended head, the most recent ended head is still kept as "previous".
        let records = vec![
            rec(6, VersionReason::DefaultHead, days(1), None),
            rec(7, VersionReason::DefaultHead, days(30), Some(days(20))),
            rec(8, VersionReason::DefaultHead, days(40), Some(days(30))),
        ];
        assert_eq!(retained(&records, now), [6, 7].into_iter().map(sha).collect());
    }

    #[test]
    fn a_sha_kept_by_any_reason_survives() {
        let now = datetime!(2026-10-05 12:00 UTC);
        let old = now - time::Duration::days(365);
        let records = vec![
            rec(1, VersionReason::DefaultHead, old, Some(old)),
            rec(2, VersionReason::DefaultHead, old, Some(old + time::Duration::days(1))),
            rec(1, VersionReason::Deployed("prod".into()), old, None),
        ];
        assert_eq!(retained(&records, now), [1, 2].into_iter().map(sha).collect());
    }
}
