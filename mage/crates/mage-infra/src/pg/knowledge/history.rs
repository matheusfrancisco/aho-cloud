//! Layer 6 (History): commits with their file rows, pull requests, ticket links and the per-repo
//! history cursor.

use std::collections::{HashMap, HashSet};

use mage_core::domain::ids::{CommitSha, OrgId, RepoId, TreeOid};
use mage_core::domain::path::RepoPath;
use mage_core::ports::knowledge::{CommitRecord, HistoryCursor, LinkLookup, LinkSource, PullRequestRecord, TicketLink};
use mage_core::wire::indexer::{ChangeKind, CommitInfo, FileChange};
use sqlx::PgConnection;
use time::OffsetDateTime;

use super::codec::{CHUNK, DbEnum, to_i32, to_path, to_u32};
use crate::pg::error::DbError;

fn shas(shas: &[CommitSha]) -> Vec<[u8; 20]> {
    shas.iter().map(|sha| sha.0).collect()
}

/// First occurrence of each item, in input order.
fn dedup_in_order<T: Eq + std::hash::Hash + Copy>(items: &[T]) -> Vec<T> {
    let mut seen = HashSet::new();
    items.iter().filter(|item| seen.insert(**item)).copied().collect()
}

/// Upsert by sha: the last record of a sha sets the commit; its file rows are upserted by path.
pub(super) async fn put_commits(conn: &mut PgConnection, org: OrgId, repo: RepoId, commits: &[CommitRecord]) -> Result<(), DbError> {
    let mut infos: Vec<&CommitInfo> = Vec::new();
    let mut at: HashMap<CommitSha, usize> = HashMap::new();
    let mut files: Vec<(CommitSha, &FileChange)> = Vec::new();
    let mut file_at: HashMap<(CommitSha, &RepoPath), usize> = HashMap::new();
    for record in commits {
        let info = &record.info;
        match at.get(&info.sha) {
            Some(&i) => infos[i] = info,
            None => {
                at.insert(info.sha, infos.len());
                infos.push(info);
            }
        }
        for file in info.files.iter().flatten() {
            match file_at.get(&(info.sha, &file.path)) {
                Some(&i) => files[i] = (info.sha, file),
                None => {
                    file_at.insert((info.sha, &file.path), files.len());
                    files.push((info.sha, file));
                }
            }
        }
    }
    for chunk in infos.chunks(CHUNK) {
        // Per-commit `bytea[]` through UNNEST: one flat array, 1-based inclusive bounds per row.
        let bound = |n: usize| i32::try_from(n).map_err(|_| DbError::Invalid("too many parents in one batch".into()));
        let mut parents: Vec<[u8; 20]> = Vec::new();
        let (mut lo, mut hi) = (Vec::with_capacity(chunk.len()), Vec::with_capacity(chunk.len()));
        for info in chunk {
            lo.push(bound(parents.len() + 1)?);
            parents.extend(info.parents.iter().map(|sha| sha.0));
            hi.push(bound(parents.len())?);
        }
        let shas: Vec<[u8; 20]> = chunk.iter().map(|info| info.sha.0).collect();
        let trees: Vec<[u8; 20]> = chunk.iter().map(|info| info.tree.0).collect();
        let authors: Vec<&str> = chunk.iter().map(|info| info.author_name.as_str()).collect();
        let author_times: Vec<OffsetDateTime> = chunk.iter().map(|info| info.author_time).collect();
        let committer_times: Vec<OffsetDateTime> = chunk.iter().map(|info| info.committer_time).collect();
        let subjects: Vec<&str> = chunk.iter().map(|info| info.subject.as_str()).collect();
        let bodies: Vec<&str> = chunk.iter().map(|info| info.body.as_str()).collect();
        sqlx::query(
            "INSERT INTO commits (org_id, repo_id, sha, tree_oid, parents, author_name, author_time, committer_time, subject, body) \
             SELECT $1, $2, u.sha, u.tree, ($3::bytea[])[u.lo:u.hi], u.author, u.author_time, u.committer_time, u.subject, u.body \
             FROM UNNEST($4::bytea[], $5::bytea[], $6::int4[], $7::int4[], $8::text[], $9::timestamptz[], $10::timestamptz[], \
                         $11::text[], $12::text[]) AS u(sha, tree, lo, hi, author, author_time, committer_time, subject, body) \
             ON CONFLICT (org_id, repo_id, sha) DO UPDATE SET tree_oid = EXCLUDED.tree_oid, parents = EXCLUDED.parents, \
               author_name = EXCLUDED.author_name, author_time = EXCLUDED.author_time, committer_time = EXCLUDED.committer_time, \
               subject = EXCLUDED.subject, body = EXCLUDED.body",
        )
        .bind(org.as_uuid())
        .bind(repo.as_uuid())
        .bind(&parents)
        .bind(&shas)
        .bind(&trees)
        .bind(&lo)
        .bind(&hi)
        .bind(&authors)
        .bind(&author_times)
        .bind(&committer_times)
        .bind(&subjects)
        .bind(&bodies)
        .execute(&mut *conn)
        .await?;
    }
    for chunk in files.chunks(CHUNK) {
        let shas: Vec<[u8; 20]> = chunk.iter().map(|(sha, _)| sha.0).collect();
        let paths: Vec<&str> = chunk.iter().map(|(_, f)| f.path.as_str()).collect();
        let changes: Vec<&str> = chunk.iter().map(|(_, f)| f.change.as_db()).collect();
        let old_paths: Vec<Option<&str>> = chunk.iter().map(|(_, f)| f.old_path.as_ref().map(RepoPath::as_str)).collect();
        let adds = chunk.iter().map(|(_, f)| f.adds.map(|n| to_i32(n, "added lines")).transpose()).collect::<Result<Vec<_>, _>>()?;
        let dels = chunk.iter().map(|(_, f)| f.dels.map(|n| to_i32(n, "deleted lines")).transpose()).collect::<Result<Vec<_>, _>>()?;
        sqlx::query(
            "INSERT INTO commit_files (org_id, repo_id, sha, path, change, old_path, adds, dels) \
             SELECT $1, $2, u.* FROM UNNEST($3::bytea[], $4::text[], $5::text[], $6::text[], $7::int4[], $8::int4[]) AS u \
             ON CONFLICT (org_id, repo_id, sha, path) DO UPDATE SET change = EXCLUDED.change, old_path = EXCLUDED.old_path, \
               adds = EXCLUDED.adds, dels = EXCLUDED.dels",
        )
        .bind(org.as_uuid())
        .bind(repo.as_uuid())
        .bind(&shas)
        .bind(&paths)
        .bind(&changes)
        .bind(&old_paths)
        .bind(&adds)
        .bind(&dels)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Upsert by number (the last record of a number wins).
pub(super) async fn put_pull_requests(conn: &mut PgConnection, org: OrgId, repo: RepoId, prs: &[PullRequestRecord]) -> Result<(), DbError> {
    let mut latest: Vec<&PullRequestRecord> = Vec::new();
    let mut at: HashMap<u32, usize> = HashMap::new();
    for pr in prs {
        match at.get(&pr.number) {
            Some(&i) => latest[i] = pr,
            None => {
                at.insert(pr.number, latest.len());
                latest.push(pr);
            }
        }
    }
    for chunk in latest.chunks(CHUNK) {
        let numbers = chunk.iter().map(|pr| to_i32(pr.number, "pull request number")).collect::<Result<Vec<_>, _>>()?;
        let titles: Vec<&str> = chunk.iter().map(|pr| pr.title.as_str()).collect();
        let bodies: Vec<&str> = chunk.iter().map(|pr| pr.body_head.as_str()).collect();
        let heads: Vec<&str> = chunk.iter().map(|pr| pr.head_ref.as_str()).collect();
        let bases: Vec<&str> = chunk.iter().map(|pr| pr.base_ref.as_str()).collect();
        let merges: Vec<Option<[u8; 20]>> = chunk.iter().map(|pr| pr.merge_commit.map(|sha| sha.0)).collect();
        let merged_at: Vec<Option<OffsetDateTime>> = chunk.iter().map(|pr| pr.merged_at).collect();
        let updated_at: Vec<OffsetDateTime> = chunk.iter().map(|pr| pr.updated_at).collect();
        let urls: Vec<&str> = chunk.iter().map(|pr| pr.url.as_str()).collect();
        sqlx::query(
            "INSERT INTO pull_requests (org_id, repo_id, number, title, body_head, head_ref, base_ref, merge_commit, merged_at, updated_at, url) \
             SELECT $1, $2, u.* FROM UNNEST($3::int4[], $4::text[], $5::text[], $6::text[], $7::text[], $8::bytea[], $9::timestamptz[], \
                                            $10::timestamptz[], $11::text[]) AS u \
             ON CONFLICT (org_id, repo_id, number) DO UPDATE SET title = EXCLUDED.title, body_head = EXCLUDED.body_head, \
               head_ref = EXCLUDED.head_ref, base_ref = EXCLUDED.base_ref, merge_commit = EXCLUDED.merge_commit, \
               merged_at = EXCLUDED.merged_at, updated_at = EXCLUDED.updated_at, url = EXCLUDED.url",
        )
        .bind(org.as_uuid())
        .bind(repo.as_uuid())
        .bind(&numbers)
        .bind(&titles)
        .bind(&bodies)
        .bind(&heads)
        .bind(&bases)
        .bind(&merges)
        .bind(&merged_at)
        .bind(&updated_at)
        .bind(&urls)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// Insert-if-absent by the whole link.
pub(super) async fn put_ticket_links(conn: &mut PgConnection, org: OrgId, repo: RepoId, links: &[TicketLink]) -> Result<(), DbError> {
    let mut seen = HashSet::new();
    let links: Vec<&TicketLink> = links.iter().filter(|link| seen.insert(*link)).collect();
    for chunk in links.chunks(CHUNK) {
        let identifiers: Vec<&str> = chunk.iter().map(|l| l.identifier.as_str()).collect();
        let sources: Vec<&str> = chunk.iter().map(|l| l.source.as_db()).collect();
        let commits: Vec<Option<[u8; 20]>> = chunk.iter().map(|l| l.commit.map(|sha| sha.0)).collect();
        let prs = chunk.iter().map(|l| l.pr.map(|n| to_i32(n, "pull request number")).transpose()).collect::<Result<Vec<_>, _>>()?;
        sqlx::query(
            "INSERT INTO ticket_links (org_id, repo_id, identifier, source, commit_sha, pr_number) \
             SELECT $1, $2, u.* FROM UNNEST($3::text[], $4::text[], $5::bytea[], $6::int4[]) AS u \
             ON CONFLICT DO NOTHING",
        )
        .bind(org.as_uuid())
        .bind(repo.as_uuid())
        .bind(&identifiers)
        .bind(&sources)
        .bind(&commits)
        .bind(&prs)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

pub(super) async fn history_cursor(conn: &mut PgConnection, org: OrgId, repo: RepoId) -> Result<HistoryCursor, DbError> {
    let row: Option<(Vec<[u8; 20]>, Option<OffsetDateTime>)> =
        sqlx::query_as("SELECT walked_heads, prs_updated_after FROM history_cursors WHERE org_id = $1 AND repo_id = $2")
            .bind(org.as_uuid())
            .bind(repo.as_uuid())
            .fetch_optional(&mut *conn)
            .await?;
    Ok(row
        .map(|(heads, prs_updated_after)| HistoryCursor { walked_heads: heads.into_iter().map(CommitSha::from_bytes).collect(), prs_updated_after })
        .unwrap_or_default())
}

/// [`HistoryCursor::merged`] under the cursor row's lock, so concurrent merges lose nothing.
pub(super) async fn merge_history_cursor(
    conn: &mut PgConnection,
    org: OrgId,
    repo: RepoId,
    cursor: &HistoryCursor,
    now: OffsetDateTime,
) -> Result<(), DbError> {
    sqlx::query(
        "INSERT INTO history_cursors (org_id, repo_id, walked_heads, prs_updated_after, updated_at) VALUES ($1, $2, '{}', NULL, $3) \
         ON CONFLICT DO NOTHING",
    )
    .bind(org.as_uuid())
    .bind(repo.as_uuid())
    .bind(now)
    .execute(&mut *conn)
    .await?;
    let (heads, prs_updated_after): (Vec<[u8; 20]>, Option<OffsetDateTime>) =
        sqlx::query_as("SELECT walked_heads, prs_updated_after FROM history_cursors WHERE org_id = $1 AND repo_id = $2 FOR UPDATE")
            .bind(org.as_uuid())
            .bind(repo.as_uuid())
            .fetch_one(&mut *conn)
            .await?;
    let stored = HistoryCursor { walked_heads: heads.into_iter().map(CommitSha::from_bytes).collect(), prs_updated_after };
    let merged = stored.merged(cursor);
    sqlx::query("UPDATE history_cursors SET walked_heads = $3, prs_updated_after = $4, updated_at = $5 WHERE org_id = $1 AND repo_id = $2")
        .bind(org.as_uuid())
        .bind(repo.as_uuid())
        .bind(shas(&merged.walked_heads))
        .bind(merged.prs_updated_after)
        .bind(now)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

pub(super) async fn has_commit(conn: &mut PgConnection, org: OrgId, repo: RepoId, sha: CommitSha) -> Result<bool, DbError> {
    Ok(sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM commits WHERE org_id = $1 AND repo_id = $2 AND sha = $3)")
        .bind(org.as_uuid())
        .bind(repo.as_uuid())
        .bind(sha.as_bytes().as_slice())
        .fetch_one(&mut *conn)
        .await?)
}

#[derive(sqlx::FromRow)]
struct CommitRow {
    sha: [u8; 20],
    tree_oid: [u8; 20],
    parents: Vec<[u8; 20]>,
    author_name: String,
    author_time: OffsetDateTime,
    committer_time: OffsetDateTime,
    subject: String,
    body: String,
}

/// Stored commits of `shas` in input order (first occurrence); files by path, `None` without rows.
pub(super) async fn commits(conn: &mut PgConnection, org: OrgId, repo: RepoId, shas_in: &[CommitSha]) -> Result<Vec<CommitRecord>, DbError> {
    let wanted = dedup_in_order(shas_in);
    let keys = shas(&wanted);
    let rows: Vec<CommitRow> = sqlx::query_as(
        "SELECT sha, tree_oid, parents, author_name, author_time, committer_time, subject, body FROM commits \
         WHERE org_id = $1 AND repo_id = $2 AND sha = ANY($3)",
    )
    .bind(org.as_uuid())
    .bind(repo.as_uuid())
    .bind(&keys)
    .fetch_all(&mut *conn)
    .await?;
    let files: Vec<([u8; 20], String, String, Option<String>, Option<i32>, Option<i32>)> = sqlx::query_as(
        "SELECT sha, path, change, old_path, adds, dels FROM commit_files \
         WHERE org_id = $1 AND repo_id = $2 AND sha = ANY($3) ORDER BY sha, path COLLATE \"C\"",
    )
    .bind(org.as_uuid())
    .bind(repo.as_uuid())
    .bind(&keys)
    .fetch_all(&mut *conn)
    .await?;
    let mut files_of: HashMap<[u8; 20], Vec<FileChange>> = HashMap::new();
    for (sha, path, change, old_path, adds, dels) in files {
        files_of.entry(sha).or_default().push(FileChange {
            path: to_path(path)?,
            old_path: old_path.map(to_path).transpose()?,
            change: ChangeKind::from_db(&change)?,
            adds: adds.map(|n| to_u32(n, "added lines")).transpose()?,
            dels: dels.map(|n| to_u32(n, "deleted lines")).transpose()?,
        });
    }
    let mut by_sha: HashMap<[u8; 20], CommitRow> = rows.into_iter().map(|row| (row.sha, row)).collect();
    Ok(wanted
        .into_iter()
        .filter_map(|sha| by_sha.remove(&sha.0))
        .map(|row| CommitRecord {
            info: CommitInfo {
                sha: CommitSha::from_bytes(row.sha),
                tree: TreeOid::from_bytes(row.tree_oid),
                parents: row.parents.into_iter().map(CommitSha::from_bytes).collect(),
                author_name: row.author_name,
                author_time: row.author_time,
                committer_time: row.committer_time,
                subject: row.subject,
                body: row.body,
                files: files_of.remove(&row.sha),
            },
        })
        .collect())
}

#[derive(sqlx::FromRow)]
struct PullRequestRow {
    number: i32,
    title: String,
    body_head: String,
    head_ref: String,
    base_ref: String,
    merge_commit: Option<[u8; 20]>,
    merged_at: Option<OffsetDateTime>,
    updated_at: OffsetDateTime,
    url: String,
}

/// PRs merged as one of `shas`: input sha order (first occurrence), then PR number.
pub(super) async fn pull_requests_for_commits(
    conn: &mut PgConnection,
    org: OrgId,
    repo: RepoId,
    shas_in: &[CommitSha],
) -> Result<Vec<(CommitSha, PullRequestRecord)>, DbError> {
    let wanted = dedup_in_order(shas_in);
    let rows: Vec<PullRequestRow> = sqlx::query_as(
        "SELECT number, title, body_head, head_ref, base_ref, merge_commit, merged_at, updated_at, url FROM pull_requests \
         WHERE org_id = $1 AND repo_id = $2 AND merge_commit = ANY($3) ORDER BY number",
    )
    .bind(org.as_uuid())
    .bind(repo.as_uuid())
    .bind(shas(&wanted))
    .fetch_all(&mut *conn)
    .await?;
    let prs = rows
        .into_iter()
        .map(|row| {
            Ok(PullRequestRecord {
                number: to_u32(row.number, "pull request number")?,
                title: row.title,
                body_head: row.body_head,
                head_ref: row.head_ref,
                base_ref: row.base_ref,
                merge_commit: row.merge_commit.map(CommitSha::from_bytes),
                merged_at: row.merged_at,
                updated_at: row.updated_at,
                url: row.url,
            })
        })
        .collect::<Result<Vec<_>, DbError>>()?;
    Ok(wanted
        .into_iter()
        .flat_map(|sha| prs.iter().filter(move |pr| pr.merge_commit == Some(sha)).map(move |pr| (sha, pr.clone())))
        .collect())
}

/// Links by identifier (ASCII case-insensitive), commit or PR; ordered by (identifier, source,
/// commit, pr).
pub(super) async fn ticket_links(conn: &mut PgConnection, org: OrgId, repo: RepoId, by: &LinkLookup) -> Result<Vec<TicketLink>, DbError> {
    let (identifier, commits, prs): (Option<&str>, Option<Vec<[u8; 20]>>, Option<Vec<i32>>) = match by {
        LinkLookup::Identifier(identifier) => (Some(identifier.as_str()), None, None),
        LinkLookup::Commits(list) => (None, Some(shas(list)), None),
        // A number past the stored range names no link.
        LinkLookup::Prs(numbers) => (None, None, Some(numbers.iter().filter_map(|n| i32::try_from(*n).ok()).collect())),
    };
    let rows: Vec<(String, String, Option<[u8; 20]>, Option<i32>)> = sqlx::query_as(
        "SELECT identifier, source, commit_sha, pr_number FROM ticket_links \
         WHERE org_id = $1 AND repo_id = $2 \
           AND ($3::text IS NULL OR lower(identifier) = lower($3)) \
           AND ($4::bytea[] IS NULL OR commit_sha = ANY($4)) \
           AND ($5::int4[] IS NULL OR pr_number = ANY($5))",
    )
    .bind(org.as_uuid())
    .bind(repo.as_uuid())
    .bind(identifier)
    .bind(commits)
    .bind(prs)
    .fetch_all(&mut *conn)
    .await?;
    let mut links = rows
        .into_iter()
        .map(|(identifier, source, commit, pr)| {
            Ok(TicketLink {
                identifier,
                source: LinkSource::from_db(&source)?,
                commit: commit.map(CommitSha::from_bytes),
                pr: pr.map(|n| to_u32(n, "pull request number")).transpose()?,
            })
        })
        .collect::<Result<Vec<_>, DbError>>()?;
    // `lower` folds more than ASCII; the port's rule is ASCII case-insensitive.
    if let Some(identifier) = identifier {
        links.retain(|link| link.identifier.eq_ignore_ascii_case(identifier));
    }
    links.sort();
    Ok(links)
}
