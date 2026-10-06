//! `PgRepoStore` (storage.md "`RepoStore`"; tables `repos`, `repo_versions`,
//! `repo_version_reasons`, `layer_status`, `layer_batches`, `recipes`, `deployments`).
//!
//! Every method except `active_repos_system` runs in a tenant transaction (`begin_org`, RLS) and
//! filters `org_id`; writes that take `jobs` insert them with the outbox in the same transaction.
//! Layer transitions lock the `layer_status` row and apply the domain `LayerState::apply`; batch
//! fan-in (`finish_layer_batch`) holds that lock too, so two last batches finishing concurrently
//! complete the layer exactly once.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use async_trait::async_trait;
use mage_core::domain::customer::{Deployment, DeploymentSource};
use mage_core::domain::recipe::{Recipe, RecipeData, RecipeStatus};
use mage_core::domain::repo::{IndexRevs, Layer, LayerEvent, LayerState, Repo, RepoAccess, RepoVersion, SourceBundleRef, VersionReason};
use mage_core::domain::{CommitSha, InstallationId, OrgId, RecipeHash, RepoId, RepoVersionKey, TicketId, TreeOid};
use mage_core::ports::clock::Clock;
use mage_core::ports::common::{Cursor, Page, PageRequest};
use mage_core::ports::ids::IdGen;
use mage_core::ports::jobs::NewJob;
use mage_core::ports::repos::{RepoStore, RepoStoreError, VersionReasonRecord};
use sqlx::postgres::PgRow;
use sqlx::types::Json;
use sqlx::{PgConnection, Row};
use time::OffsetDateTime;
use uuid::Uuid;

use super::error::DbError;
use super::jobs::enqueue_all_in;
use super::pool::PgPools;
use super::tx::{TxError, begin_org, retry_tx};

type Result<T, E = RepoStoreError> = std::result::Result<T, E>;
type TxResult<T> = std::result::Result<T, TxError<RepoStoreError>>;

/// Postgres `RepoStore`.
pub struct PgRepoStore {
    pools: PgPools,
    clock: Arc<dyn Clock>,
    ids: Arc<dyn IdGen>,
}

impl PgRepoStore {
    pub fn new(pools: PgPools, clock: Arc<dyn Clock>, ids: Arc<dyn IdGen>) -> Self {
        Self { pools, clock, ids }
    }

    fn now(&self) -> OffsetDateTime {
        let now = self.clock.now();
        now.replace_nanosecond(now.nanosecond() / 1_000 * 1_000).unwrap_or(now)
    }
}

// ---- column codecs ----------------------------------------------------------------------------

fn access_str(access: RepoAccess) -> &'static str {
    match access {
        RepoAccess::Granted => "granted",
        RepoAccess::Lost => "lost",
    }
}

fn parse_access(s: &str) -> Result<RepoAccess, DbError> {
    match s {
        "granted" => Ok(RepoAccess::Granted),
        "lost" => Ok(RepoAccess::Lost),
        other => Err(DbError::Invalid(format!("unknown repo access {other:?}"))),
    }
}

fn layer_str(layer: Layer) -> &'static str {
    match layer {
        Layer::Files => "files",
        Layer::Symbols => "symbols",
        Layer::Refs => "refs",
        Layer::ErrorSites => "error_sites",
        Layer::AppMap => "app_map",
        Layer::Semantic => "semantic",
        Layer::History => "history",
    }
}

fn parse_layer(s: &str) -> Result<Layer, DbError> {
    Layer::ALL.into_iter().find(|l| layer_str(*l) == s).ok_or_else(|| DbError::Invalid(format!("unknown layer {s:?}")))
}

/// `(state, reason, ready_at)` columns of a `LayerState`.
fn layer_columns(state: &LayerState) -> (&'static str, Option<&str>, Option<OffsetDateTime>) {
    match state {
        LayerState::Pending => ("pending", None, None),
        LayerState::Running => ("running", None, None),
        LayerState::Ready { at } => ("ready", None, Some(*at)),
        LayerState::Failed { reason } => ("failed", Some(reason), None),
        LayerState::Skipped { reason } => ("skipped", Some(reason), None),
    }
}

fn layer_state(state: &str, reason: Option<String>, ready_at: Option<OffsetDateTime>) -> Result<LayerState, DbError> {
    let missing = |what: &str| DbError::Invalid(format!("layer state {state} without {what}"));
    Ok(match state {
        "pending" => LayerState::Pending,
        "running" => LayerState::Running,
        "ready" => LayerState::Ready { at: ready_at.ok_or_else(|| missing("ready_at"))? },
        "failed" => LayerState::Failed { reason: reason.ok_or_else(|| missing("reason"))? },
        "skipped" => LayerState::Skipped { reason: reason.ok_or_else(|| missing("reason"))? },
        other => return Err(DbError::Invalid(format!("unknown layer state {other:?}"))),
    })
}

/// `(reason, detail)` columns of a `VersionReason`.
fn reason_columns(reason: &VersionReason) -> (&'static str, String) {
    match reason {
        VersionReason::DefaultHead => ("default_head", String::new()),
        VersionReason::Release(tag) => ("release", tag.clone()),
        VersionReason::Deployed(env) => ("deployed", env.clone()),
        VersionReason::OnDemand(ticket) => ("on_demand", ticket.as_uuid().to_string()),
    }
}

fn version_reason(reason: &str, detail: String) -> Result<VersionReason, DbError> {
    Ok(match reason {
        "default_head" => VersionReason::DefaultHead,
        "release" => VersionReason::Release(detail),
        "deployed" => VersionReason::Deployed(detail),
        "on_demand" => {
            let uuid = Uuid::try_parse(&detail).map_err(|e| DbError::Invalid(format!("on_demand reason detail {detail:?}: {e}")))?;
            VersionReason::OnDemand(TicketId::from_uuid(uuid))
        }
        other => return Err(DbError::Invalid(format!("unknown version reason {other:?}"))),
    })
}

fn deployment_source_str(source: DeploymentSource) -> &'static str {
    match source {
        DeploymentSource::GitHubDeployments => "github_deployments",
        DeploymentSource::Manual => "manual",
        DeploymentSource::Tag => "tag",
    }
}

fn parse_deployment_source(s: &str) -> Result<DeploymentSource, DbError> {
    match s {
        "github_deployments" => Ok(DeploymentSource::GitHubDeployments),
        "manual" => Ok(DeploymentSource::Manual),
        "tag" => Ok(DeploymentSource::Tag),
        other => Err(DbError::Invalid(format!("unknown deployment source {other:?}"))),
    }
}

fn fixed<const N: usize>(bytes: Vec<u8>, what: &str) -> Result<[u8; N], DbError> {
    <[u8; N]>::try_from(bytes).map_err(|b| DbError::Invalid(format!("{what}: expected {N} bytes, got {}", b.len())))
}

fn as_i64(n: u64, what: &str) -> Result<i64, DbError> {
    i64::try_from(n).map_err(|_| DbError::Invalid(format!("{what} {n} exceeds bigint")))
}

const REPO_COLUMNS: &str = "org_id, id, external_id, owner, name, default_branch, installation_id, access, created_at";

fn repo_from_row(row: &PgRow) -> Result<Repo, DbError> {
    let external: i64 = row.try_get("external_id")?;
    let installation: i64 = row.try_get("installation_id")?;
    let access: String = row.try_get("access")?;
    Ok(Repo {
        id: RepoId::from_uuid(row.try_get("id")?),
        org_id: OrgId::from_uuid(row.try_get("org_id")?),
        external_id: u64::try_from(external).map_err(|_| DbError::Invalid("negative external_id".into()))?,
        owner: row.try_get("owner")?,
        name: row.try_get("name")?,
        default_branch: row.try_get("default_branch")?,
        installation_id: InstallationId(u64::try_from(installation).map_err(|_| DbError::Invalid("negative installation_id".into()))?),
        access: parse_access(&access)?,
        created_at: row.try_get("created_at")?,
    })
}

const RECIPE_COLUMNS: &str = "org_id, repo_id, hash, body, status, validated_at_sha, validated_at, broken_reason, broken_at, created_at";

fn recipe_from_row(row: &PgRow) -> Result<Recipe, DbError> {
    let status: String = row.try_get("status")?;
    let status = match status.as_str() {
        "draft" => RecipeStatus::Draft,
        "validated" => RecipeStatus::Validated {
            at: row.try_get::<Option<OffsetDateTime>, _>("validated_at")?.ok_or_else(|| DbError::Invalid("validated recipe without validated_at".into()))?,
        },
        "broken" => RecipeStatus::Broken {
            reason: row.try_get::<Option<String>, _>("broken_reason")?.unwrap_or_default(),
            at: row.try_get::<Option<OffsetDateTime>, _>("broken_at")?.ok_or_else(|| DbError::Invalid("broken recipe without broken_at".into()))?,
        },
        other => return Err(DbError::Invalid(format!("unknown recipe status {other:?}"))),
    };
    let validated_at_sha = row.try_get::<Option<Vec<u8>>, _>("validated_at_sha")?.map(|b| fixed::<20>(b, "validated_at_sha")).transpose()?;
    Ok(Recipe::rehydrate(RecipeData {
        org_id: OrgId::from_uuid(row.try_get("org_id")?),
        repo_id: RepoId::from_uuid(row.try_get("repo_id")?),
        hash: RecipeHash::from_bytes(fixed::<32>(row.try_get("hash")?, "recipe hash")?),
        body: row.try_get("body")?,
        validated_at_sha: validated_at_sha.map(CommitSha::from_bytes),
        status,
    }))
}

fn deployment_from_row(row: &PgRow) -> Result<Deployment, DbError> {
    let source: String = row.try_get("source")?;
    Ok(Deployment {
        org_id: OrgId::from_uuid(row.try_get("org_id")?),
        repo_id: RepoId::from_uuid(row.try_get("repo_id")?),
        environment: row.try_get("environment")?,
        sha: CommitSha::from_bytes(fixed::<20>(row.try_get("sha")?, "deployment sha")?),
        observed_at: row.try_get("observed_at")?,
        source: parse_deployment_source(&source)?,
    })
}

/// Keyset cursor `{unix micros}/{text}`.
fn encode_cursor(at: OffsetDateTime, tail: &str) -> Cursor {
    Cursor(format!("{}/{tail}", at.unix_timestamp_nanos() / 1_000))
}

fn decode_cursor(cursor: &Cursor) -> Result<(OffsetDateTime, String)> {
    let invalid = || RepoStoreError::Invalid(format!("bad cursor {:?}", cursor.0));
    let (us, tail) = cursor.0.split_once('/').ok_or_else(invalid)?;
    let us: i128 = us.parse().map_err(|_| invalid())?;
    Ok((OffsetDateTime::from_unix_timestamp_nanos(us * 1_000).map_err(|_| invalid())?, tail.to_owned()))
}

fn limit_plus_one(page: &PageRequest) -> i64 {
    i64::try_from(page.effective_limit() + 1).unwrap_or(i64::MAX)
}

fn cross_tenant() -> RepoStoreError {
    RepoStoreError::Invalid("record belongs to another org".into())
}

// ---- version helpers (inside a tenant transaction) ---------------------------------------------

/// Locks one layer row; `NotFound` when the version (or its layer row) is absent.
async fn lock_layer(conn: &mut PgConnection, org: OrgId, key: RepoVersionKey, layer: Layer) -> TxResult<LayerState> {
    let row: Option<(String, Option<String>, Option<OffsetDateTime>)> = sqlx::query_as(
        "SELECT state, reason, ready_at FROM layer_status \
         WHERE org_id = $1 AND repo_id = $2 AND sha = $3 AND layer = $4 FOR UPDATE",
    )
    .bind(org.as_uuid())
    .bind(key.repo_id.as_uuid())
    .bind(key.sha.as_bytes().as_slice())
    .bind(layer_str(layer))
    .fetch_optional(&mut *conn)
    .await?;
    let (state, reason, ready_at) = row.ok_or(TxError::Port(RepoStoreError::NotFound))?;
    Ok(layer_state(&state, reason, ready_at)?)
}

async fn write_layer(conn: &mut PgConnection, org: OrgId, key: RepoVersionKey, layer: Layer, state: &LayerState, now: OffsetDateTime) -> TxResult<()> {
    let (name, reason, ready_at) = layer_columns(state);
    sqlx::query(
        "UPDATE layer_status SET state = $5, reason = $6, ready_at = $7, updated_at = $8 \
         WHERE org_id = $1 AND repo_id = $2 AND sha = $3 AND layer = $4",
    )
    .bind(org.as_uuid())
    .bind(key.repo_id.as_uuid())
    .bind(key.sha.as_bytes().as_slice())
    .bind(layer_str(layer))
    .bind(name)
    .bind(reason)
    .bind(ready_at)
    .bind(now)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

fn apply(current: LayerState, event: LayerEvent) -> TxResult<LayerState> {
    current.apply(event).map_err(|e| TxError::Port(RepoStoreError::Invalid(e.to_string())))
}

/// Assemble `RepoVersion`s of `repo` (all, or the given shas) from the three tables.
async fn load_versions(conn: &mut PgConnection, org: OrgId, repo: RepoId, sha: Option<CommitSha>) -> Result<Vec<RepoVersion>, DbError> {
    let sha_bytes = sha.map(|s| s.as_bytes().to_vec());
    let rows = sqlx::query(
        "SELECT sha, tree_oid, revs, bundle_key, bundle_bytes, bundle_sha256, created_at FROM repo_versions \
         WHERE org_id = $1 AND repo_id = $2 AND ($3::bytea IS NULL OR sha = $3) ORDER BY created_at, sha",
    )
    .bind(org.as_uuid())
    .bind(repo.as_uuid())
    .bind(sha_bytes.as_deref())
    .fetch_all(&mut *conn)
    .await?;
    if rows.is_empty() {
        return Ok(Vec::new());
    }
    let reasons: Vec<(Vec<u8>, String, String)> = sqlx::query_as(
        "SELECT sha, reason, detail FROM repo_version_reasons \
         WHERE org_id = $1 AND repo_id = $2 AND ($3::bytea IS NULL OR sha = $3) AND ended_at IS NULL",
    )
    .bind(org.as_uuid())
    .bind(repo.as_uuid())
    .bind(sha_bytes.as_deref())
    .fetch_all(&mut *conn)
    .await?;
    let layers: Vec<(Vec<u8>, String, String, Option<String>, Option<OffsetDateTime>)> = sqlx::query_as(
        "SELECT sha, layer, state, reason, ready_at FROM layer_status \
         WHERE org_id = $1 AND repo_id = $2 AND ($3::bytea IS NULL OR sha = $3)",
    )
    .bind(org.as_uuid())
    .bind(repo.as_uuid())
    .bind(sha_bytes.as_deref())
    .fetch_all(&mut *conn)
    .await?;

    let mut reason_map: BTreeMap<Vec<u8>, BTreeSet<VersionReason>> = BTreeMap::new();
    for (sha, reason, detail) in reasons {
        reason_map.entry(sha).or_default().insert(version_reason(&reason, detail)?);
    }
    let mut layer_map: BTreeMap<Vec<u8>, BTreeMap<Layer, LayerState>> = BTreeMap::new();
    for (sha, layer, state, reason, ready_at) in layers {
        layer_map.entry(sha).or_default().insert(parse_layer(&layer)?, layer_state(&state, reason, ready_at)?);
    }
    rows.iter()
        .map(|row| {
            let sha_raw: Vec<u8> = row.try_get("sha")?;
            let tree: Option<Vec<u8>> = row.try_get("tree_oid")?;
            let bundle_key: Option<String> = row.try_get("bundle_key")?;
            let bundle = match (bundle_key, tree) {
                (Some(key), Some(tree)) => {
                    let bytes: Option<i64> = row.try_get("bundle_bytes")?;
                    let sha256: Option<Vec<u8>> = row.try_get("bundle_sha256")?;
                    Some(SourceBundleRef {
                        key,
                        tree: TreeOid::from_bytes(fixed::<20>(tree, "tree_oid")?),
                        bytes: u64::try_from(bytes.unwrap_or(0)).unwrap_or(0),
                        sha256: fixed::<32>(sha256.unwrap_or_default(), "bundle_sha256")?,
                    })
                }
                _ => None,
            };
            let revs: Option<Json<IndexRevs>> = row.try_get("revs")?;
            Ok(RepoVersion {
                org_id: org,
                repo_id: repo,
                sha: CommitSha::from_bytes(fixed::<20>(sha_raw.clone(), "sha")?),
                reasons: reason_map.remove(&sha_raw).unwrap_or_default(),
                layers: layer_map.remove(&sha_raw).unwrap_or_default(),
                bundle,
                revs: revs.map(|Json(r)| r),
                created_at: row.try_get("created_at")?,
            })
        })
        .collect()
}

// ---- the port ---------------------------------------------------------------------------------

#[async_trait]
impl RepoStore for PgRepoStore {
    async fn upsert_repo(&self, repo: &Repo, jobs: &[NewJob]) -> Result<Repo> {
        jobs.iter().try_for_each(NewJob::validate)?;
        let now = self.now();
        let org = repo.org_id;
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let row = sqlx::query(&format!(
                "INSERT INTO repos ({REPO_COLUMNS}) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
                 ON CONFLICT (org_id, external_id) DO UPDATE SET owner = EXCLUDED.owner, name = EXCLUDED.name, \
                   default_branch = EXCLUDED.default_branch, installation_id = EXCLUDED.installation_id, access = EXCLUDED.access \
                 RETURNING {REPO_COLUMNS}"
            ))
            .bind(org.as_uuid())
            .bind(repo.id.as_uuid())
            .bind(as_i64(repo.external_id, "external_id")?)
            .bind(&repo.owner)
            .bind(&repo.name)
            .bind(&repo.default_branch)
            .bind(as_i64(repo.installation_id.0, "installation_id")?)
            .bind(access_str(repo.access))
            .bind(repo.created_at)
            .fetch_one(&mut *tx)
            .await?;
            let stored = repo_from_row(&row)?;
            enqueue_all_in(&mut tx, jobs, now, &*self.ids).await?;
            tx.commit().await?;
            Ok(stored)
        })
        .await
    }

    async fn repo(&self, org: OrgId, id: RepoId) -> Result<Repo> {
        let mut tx = begin_org(&self.pools.tenant, org).await?;
        let row = sqlx::query(&format!("SELECT {REPO_COLUMNS} FROM repos WHERE org_id = $1 AND id = $2"))
            .bind(org.as_uuid())
            .bind(id.as_uuid())
            .fetch_optional(&mut *tx)
            .await
            .map_err(DbError::from)?
            .ok_or(RepoStoreError::NotFound)?;
        tx.commit().await.map_err(DbError::from)?;
        Ok(repo_from_row(&row)?)
    }

    async fn get_by_external_id(&self, org: OrgId, external_id: u64) -> Result<Option<Repo>> {
        let mut tx = begin_org(&self.pools.tenant, org).await?;
        let row = sqlx::query(&format!("SELECT {REPO_COLUMNS} FROM repos WHERE org_id = $1 AND external_id = $2"))
            .bind(org.as_uuid())
            .bind(as_i64(external_id, "external_id")?)
            .fetch_optional(&mut *tx)
            .await
            .map_err(DbError::from)?;
        tx.commit().await.map_err(DbError::from)?;
        Ok(row.as_ref().map(repo_from_row).transpose()?)
    }

    async fn repo_by_name(&self, org: OrgId, owner: &str, name: &str) -> Result<Option<Repo>> {
        let mut tx = begin_org(&self.pools.tenant, org).await?;
        let row = sqlx::query(&format!(
            "SELECT {REPO_COLUMNS} FROM repos WHERE org_id = $1 AND lower(owner) = lower($2) AND lower(name) = lower($3) \
             ORDER BY id LIMIT 1"
        ))
        .bind(org.as_uuid())
        .bind(owner)
        .bind(name)
        .fetch_optional(&mut *tx)
        .await
        .map_err(DbError::from)?;
        tx.commit().await.map_err(DbError::from)?;
        Ok(row.as_ref().map(repo_from_row).transpose()?)
    }

    async fn list_repos(&self, org: OrgId) -> Result<Vec<Repo>> {
        let mut tx = begin_org(&self.pools.tenant, org).await?;
        let rows = sqlx::query(&format!(
            "SELECT {REPO_COLUMNS} FROM repos WHERE org_id = $1 \
             ORDER BY lower(owner) COLLATE \"C\", lower(name) COLLATE \"C\", id"
        ))
        .bind(org.as_uuid())
        .fetch_all(&mut *tx)
        .await
        .map_err(DbError::from)?;
        tx.commit().await.map_err(DbError::from)?;
        Ok(rows.iter().map(repo_from_row).collect::<Result<Vec<_>, _>>()?)
    }

    async fn set_access(&self, org: OrgId, repo: RepoId, access: RepoAccess) -> Result<()> {
        let mut tx = begin_org(&self.pools.tenant, org).await?;
        let updated = sqlx::query("UPDATE repos SET access = $3 WHERE org_id = $1 AND id = $2")
            .bind(org.as_uuid())
            .bind(repo.as_uuid())
            .bind(access_str(access))
            .execute(&mut *tx)
            .await
            .map_err(DbError::from)?;
        if updated.rows_affected() == 0 {
            return Err(RepoStoreError::NotFound);
        }
        tx.commit().await.map_err(DbError::from)?;
        Ok(())
    }

    async fn active_repos_system(&self) -> Result<Vec<(OrgId, RepoId)>> {
        let rows: Vec<(Uuid, Uuid)> = sqlx::query_as("SELECT org_id, id FROM repos WHERE access = 'granted' ORDER BY org_id, id")
            .fetch_all(&self.pools.platform)
            .await
            .map_err(DbError::from)?;
        Ok(rows.into_iter().map(|(o, r)| (OrgId::from_uuid(o), RepoId::from_uuid(r))).collect())
    }

    async fn add_version_reason(&self, org: OrgId, repo: RepoId, sha: CommitSha, reason: VersionReason) -> Result<bool> {
        let now = self.now();
        let (kind, detail) = reason_columns(&reason);
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let created: Option<i32> = sqlx::query_scalar(
                "INSERT INTO repo_versions (org_id, repo_id, sha, created_at) VALUES ($1, $2, $3, $4) \
                 ON CONFLICT DO NOTHING RETURNING 1",
            )
            .bind(org.as_uuid())
            .bind(repo.as_uuid())
            .bind(sha.as_bytes().as_slice())
            .bind(now)
            .fetch_optional(&mut *tx)
            .await?;
            let created = created.is_some();
            if created {
                let layers: Vec<&str> = Layer::ALL.iter().map(|l| layer_str(*l)).collect();
                sqlx::query(
                    "INSERT INTO layer_status (org_id, repo_id, sha, layer, state, updated_at) \
                     SELECT $1, $2, $3, l, 'pending', $5 FROM UNNEST($4::text[]) AS l",
                )
                .bind(org.as_uuid())
                .bind(repo.as_uuid())
                .bind(sha.as_bytes().as_slice())
                .bind(&layers)
                .bind(now)
                .execute(&mut *tx)
                .await?;
            }
            let held: Option<i32> = sqlx::query_scalar(
                "SELECT 1 FROM repo_version_reasons \
                 WHERE org_id = $1 AND repo_id = $2 AND sha = $3 AND reason = $4 AND detail = $5 AND ended_at IS NULL",
            )
            .bind(org.as_uuid())
            .bind(repo.as_uuid())
            .bind(sha.as_bytes().as_slice())
            .bind(kind)
            .bind(&detail)
            .fetch_optional(&mut *tx)
            .await?;
            if held.is_none() {
                // Singletons move: the previous holder's reason ends now.
                let singleton = matches!(reason, VersionReason::DefaultHead | VersionReason::Deployed(_));
                if singleton {
                    sqlx::query(
                        "UPDATE repo_version_reasons SET ended_at = $6 \
                         WHERE org_id = $1 AND repo_id = $2 AND reason = $3 AND detail = $4 AND sha <> $5 AND ended_at IS NULL",
                    )
                    .bind(org.as_uuid())
                    .bind(repo.as_uuid())
                    .bind(kind)
                    .bind(&detail)
                    .bind(sha.as_bytes().as_slice())
                    .bind(now)
                    .execute(&mut *tx)
                    .await?;
                }
                sqlx::query(
                    "INSERT INTO repo_version_reasons (org_id, repo_id, sha, reason, detail, added_at) VALUES ($1, $2, $3, $4, $5, $6) \
                     ON CONFLICT (org_id, repo_id, sha, reason, detail, added_at) DO UPDATE SET ended_at = NULL",
                )
                .bind(org.as_uuid())
                .bind(repo.as_uuid())
                .bind(sha.as_bytes().as_slice())
                .bind(kind)
                .bind(&detail)
                .bind(now)
                .execute(&mut *tx)
                .await?;
            }
            tx.commit().await?;
            Ok::<_, TxError<RepoStoreError>>(created)
        })
        .await
    }

    async fn default_head(&self, org: OrgId, repo: RepoId) -> Result<Option<CommitSha>> {
        let mut tx = begin_org(&self.pools.tenant, org).await?;
        let sha: Option<Vec<u8>> = sqlx::query_scalar(
            "SELECT sha FROM repo_version_reasons \
             WHERE org_id = $1 AND repo_id = $2 AND reason = 'default_head' AND ended_at IS NULL",
        )
        .bind(org.as_uuid())
        .bind(repo.as_uuid())
        .fetch_optional(&mut *tx)
        .await
        .map_err(DbError::from)?;
        tx.commit().await.map_err(DbError::from)?;
        Ok(sha.map(|s| fixed::<20>(s, "sha").map(CommitSha::from_bytes)).transpose()?)
    }

    async fn set_default_head(&self, org: OrgId, repo: RepoId, sha: CommitSha) -> Result<bool> {
        self.add_version_reason(org, repo, sha, VersionReason::DefaultHead).await
    }

    async fn version(&self, org: OrgId, key: RepoVersionKey) -> Result<RepoVersion> {
        let mut tx = begin_org(&self.pools.tenant, org).await?;
        let mut versions = load_versions(&mut tx, org, key.repo_id, Some(key.sha)).await?;
        tx.commit().await.map_err(DbError::from)?;
        versions.pop().ok_or(RepoStoreError::NotFound)
    }

    async fn versions(&self, org: OrgId, repo: RepoId) -> Result<Vec<RepoVersion>> {
        let mut tx = begin_org(&self.pools.tenant, org).await?;
        let versions = load_versions(&mut tx, org, repo, None).await?;
        tx.commit().await.map_err(DbError::from)?;
        Ok(versions)
    }

    async fn version_reasons(&self, org: OrgId, repo: RepoId) -> Result<Vec<VersionReasonRecord>> {
        let mut tx = begin_org(&self.pools.tenant, org).await?;
        let rows: Vec<(Vec<u8>, String, String, OffsetDateTime, Option<OffsetDateTime>)> = sqlx::query_as(
            "SELECT sha, reason, detail, added_at, ended_at FROM repo_version_reasons WHERE org_id = $1 AND repo_id = $2",
        )
        .bind(org.as_uuid())
        .bind(repo.as_uuid())
        .fetch_all(&mut *tx)
        .await
        .map_err(DbError::from)?;
        tx.commit().await.map_err(DbError::from)?;
        let mut out = rows
            .into_iter()
            .map(|(sha, reason, detail, added_at, ended_at)| {
                Ok(VersionReasonRecord { sha: CommitSha::from_bytes(fixed::<20>(sha, "sha")?), reason: version_reason(&reason, detail)?, added_at, ended_at })
            })
            .collect::<Result<Vec<_>, DbError>>()?;
        out.sort_by(|a, b| (a.added_at, a.sha, &a.reason).cmp(&(b.added_at, b.sha, &b.reason)));
        Ok(out)
    }

    async fn delete_version(&self, org: OrgId, key: RepoVersionKey) -> Result<()> {
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            for table in ["layer_batches", "layer_status", "repo_version_reasons", "repo_versions"] {
                sqlx::query(&format!("DELETE FROM {table} WHERE org_id = $1 AND repo_id = $2 AND sha = $3"))
                    .bind(org.as_uuid())
                    .bind(key.repo_id.as_uuid())
                    .bind(key.sha.as_bytes().as_slice())
                    .execute(&mut *tx)
                    .await?;
            }
            tx.commit().await?;
            Ok::<_, TxError<RepoStoreError>>(())
        })
        .await
    }

    async fn apply_layer_event(&self, org: OrgId, key: RepoVersionKey, layer: Layer, event: LayerEvent, jobs: &[NewJob]) -> Result<LayerState> {
        jobs.iter().try_for_each(NewJob::validate)?;
        let now = self.now();
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let current = lock_layer(&mut tx, org, key, layer).await?;
            let next = apply(current, event.clone())?;
            write_layer(&mut tx, org, key, layer, &next, now).await?;
            enqueue_all_in(&mut tx, jobs, now, &*self.ids).await?;
            tx.commit().await?;
            Ok(next)
        })
        .await
    }

    async fn begin_layer_batches(&self, org: OrgId, key: RepoVersionKey, layer: Layer, n: u32, jobs: &[NewJob]) -> Result<()> {
        if n == 0 {
            return Err(RepoStoreError::Invalid("a batched layer needs at least one batch".into()));
        }
        let batches = i32::try_from(n).map_err(|_| RepoStoreError::Invalid(format!("{n} batches")))?;
        jobs.iter().try_for_each(NewJob::validate)?;
        let now = self.now();
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let current = lock_layer(&mut tx, org, key, layer).await?;
            // A Ready layer keeps serving its current facts during a shadow rebuild.
            if !current.is_ready() {
                let next = apply(current, LayerEvent::Start)?;
                write_layer(&mut tx, org, key, layer, &next, now).await?;
            }
            sqlx::query("DELETE FROM layer_batches WHERE org_id = $1 AND repo_id = $2 AND sha = $3 AND layer = $4")
                .bind(org.as_uuid())
                .bind(key.repo_id.as_uuid())
                .bind(key.sha.as_bytes().as_slice())
                .bind(layer_str(layer))
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                "INSERT INTO layer_batches (org_id, repo_id, sha, layer, batch) \
                 SELECT $1, $2, $3, $4, b FROM generate_series(1, $5::int) AS b",
            )
            .bind(org.as_uuid())
            .bind(key.repo_id.as_uuid())
            .bind(key.sha.as_bytes().as_slice())
            .bind(layer_str(layer))
            .bind(batches)
            .execute(&mut *tx)
            .await?;
            enqueue_all_in(&mut tx, jobs, now, &*self.ids).await?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    async fn finish_layer_batch(&self, org: OrgId, key: RepoVersionKey, layer: Layer, batch: u32, on_last: &[NewJob]) -> Result<u32> {
        on_last.iter().try_for_each(NewJob::validate)?;
        let batch = i32::try_from(batch).map_err(|_| RepoStoreError::NotFound)?;
        let now = self.now();
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            // The layer row lock serializes the fan-in of one (version, layer).
            let current = lock_layer(&mut tx, org, key, layer).await?;
            let done_at: Option<Option<OffsetDateTime>> = sqlx::query_scalar(
                "SELECT done_at FROM layer_batches WHERE org_id = $1 AND repo_id = $2 AND sha = $3 AND layer = $4 AND batch = $5",
            )
            .bind(org.as_uuid())
            .bind(key.repo_id.as_uuid())
            .bind(key.sha.as_bytes().as_slice())
            .bind(layer_str(layer))
            .bind(batch)
            .fetch_optional(&mut *tx)
            .await?;
            let Some(done_at) = done_at else {
                return Err(TxError::Port(RepoStoreError::NotFound));
            };
            if done_at.is_none() {
                sqlx::query(
                    "UPDATE layer_batches SET done_at = $6 \
                     WHERE org_id = $1 AND repo_id = $2 AND sha = $3 AND layer = $4 AND batch = $5",
                )
                .bind(org.as_uuid())
                .bind(key.repo_id.as_uuid())
                .bind(key.sha.as_bytes().as_slice())
                .bind(layer_str(layer))
                .bind(batch)
                .bind(now)
                .execute(&mut *tx)
                .await?;
            }
            let open: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM layer_batches \
                 WHERE org_id = $1 AND repo_id = $2 AND sha = $3 AND layer = $4 AND done_at IS NULL",
            )
            .bind(org.as_uuid())
            .bind(key.repo_id.as_uuid())
            .bind(key.sha.as_bytes().as_slice())
            .bind(layer_str(layer))
            .fetch_one(&mut *tx)
            .await?;
            if done_at.is_none() && open == 0 {
                let next = apply(current, LayerEvent::Complete { at: now })?;
                write_layer(&mut tx, org, key, layer, &next, now).await?;
                enqueue_all_in(&mut tx, on_last, now, &*self.ids).await?;
            }
            tx.commit().await?;
            Ok(u32::try_from(open).unwrap_or(u32::MAX))
        })
        .await
    }

    async fn set_index_revs(&self, org: OrgId, key: RepoVersionKey, revs: &IndexRevs) -> Result<()> {
        let mut tx = begin_org(&self.pools.tenant, org).await?;
        let updated = sqlx::query("UPDATE repo_versions SET revs = $4 WHERE org_id = $1 AND repo_id = $2 AND sha = $3")
            .bind(org.as_uuid())
            .bind(key.repo_id.as_uuid())
            .bind(key.sha.as_bytes().as_slice())
            .bind(Json(revs))
            .execute(&mut *tx)
            .await
            .map_err(DbError::from)?;
        if updated.rows_affected() == 0 {
            return Err(RepoStoreError::NotFound);
        }
        tx.commit().await.map_err(DbError::from)?;
        Ok(())
    }

    async fn set_source_bundle(&self, org: OrgId, key: RepoVersionKey, bundle: &SourceBundleRef, jobs: &[NewJob]) -> Result<()> {
        jobs.iter().try_for_each(NewJob::validate)?;
        let now = self.now();
        let bytes = as_i64(bundle.bytes, "bundle bytes")?;
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let updated = sqlx::query(
                "UPDATE repo_versions SET tree_oid = $4, bundle_key = $5, bundle_bytes = $6, bundle_sha256 = $7 \
                 WHERE org_id = $1 AND repo_id = $2 AND sha = $3",
            )
            .bind(org.as_uuid())
            .bind(key.repo_id.as_uuid())
            .bind(key.sha.as_bytes().as_slice())
            .bind(bundle.tree.as_bytes().as_slice())
            .bind(&bundle.key)
            .bind(bytes)
            .bind(bundle.sha256.as_slice())
            .execute(&mut *tx)
            .await?;
            if updated.rows_affected() == 0 {
                return Err(TxError::Port(RepoStoreError::NotFound));
            }
            enqueue_all_in(&mut tx, jobs, now, &*self.ids).await?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    async fn bundle_for_tree(&self, org: OrgId, repo: RepoId, tree: TreeOid) -> Result<Option<SourceBundleRef>> {
        let mut tx = begin_org(&self.pools.tenant, org).await?;
        let row: Option<(String, Option<i64>, Option<Vec<u8>>)> = sqlx::query_as(
            "SELECT bundle_key, bundle_bytes, bundle_sha256 FROM repo_versions \
             WHERE org_id = $1 AND repo_id = $2 AND tree_oid = $3 AND bundle_key IS NOT NULL \
             ORDER BY created_at, sha LIMIT 1",
        )
        .bind(org.as_uuid())
        .bind(repo.as_uuid())
        .bind(tree.as_bytes().as_slice())
        .fetch_optional(&mut *tx)
        .await
        .map_err(DbError::from)?;
        tx.commit().await.map_err(DbError::from)?;
        row.map(|(key, bytes, sha256)| {
            Ok(SourceBundleRef {
                key,
                tree,
                bytes: u64::try_from(bytes.unwrap_or(0)).unwrap_or(0),
                sha256: fixed::<32>(sha256.unwrap_or_default(), "bundle_sha256")?,
            })
        })
        .transpose()
    }

    async fn put_recipe(&self, org: OrgId, recipe: &Recipe) -> Result<bool> {
        if recipe.org_id != org {
            return Err(cross_tenant());
        }
        let now = self.now();
        let (status, validated_at, broken_reason, broken_at) = recipe_status_columns(&recipe.status);
        let mut tx = begin_org(&self.pools.tenant, org).await?;
        let inserted: Option<i32> = sqlx::query_scalar(&format!(
            "INSERT INTO recipes ({RECIPE_COLUMNS}) VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
             ON CONFLICT (org_id, repo_id, hash) DO NOTHING RETURNING 1"
        ))
        .bind(org.as_uuid())
        .bind(recipe.repo_id.as_uuid())
        .bind(recipe.hash.as_bytes().as_slice())
        .bind(&recipe.body)
        .bind(status)
        .bind(recipe.validated_at_sha.map(|s| s.as_bytes().to_vec()))
        .bind(validated_at)
        .bind(broken_reason)
        .bind(broken_at)
        .bind(now)
        .fetch_optional(&mut *tx)
        .await
        .map_err(DbError::from)?;
        tx.commit().await.map_err(DbError::from)?;
        Ok(inserted.is_some())
    }

    async fn get_recipe(&self, org: OrgId, repo: RepoId, hash: RecipeHash) -> Result<Recipe> {
        let mut tx = begin_org(&self.pools.tenant, org).await?;
        let row = sqlx::query(&format!("SELECT {RECIPE_COLUMNS} FROM recipes WHERE org_id = $1 AND repo_id = $2 AND hash = $3"))
            .bind(org.as_uuid())
            .bind(repo.as_uuid())
            .bind(hash.as_bytes().as_slice())
            .fetch_optional(&mut *tx)
            .await
            .map_err(DbError::from)?
            .ok_or(RepoStoreError::NotFound)?;
        tx.commit().await.map_err(DbError::from)?;
        Ok(recipe_from_row(&row)?)
    }

    async fn latest_recipe(&self, org: OrgId, repo: RepoId) -> Result<Option<Recipe>> {
        let mut tx = begin_org(&self.pools.tenant, org).await?;
        let row = sqlx::query(&format!(
            "(SELECT {RECIPE_COLUMNS}, 0 AS pick FROM recipes WHERE org_id = $1 AND repo_id = $2 AND status = 'validated' \
              ORDER BY validated_at DESC, hash LIMIT 1) \
             UNION ALL \
             (SELECT {RECIPE_COLUMNS}, 1 AS pick FROM recipes WHERE org_id = $1 AND repo_id = $2 AND status = 'draft' \
              ORDER BY created_at DESC, hash LIMIT 1) \
             ORDER BY pick LIMIT 1"
        ))
        .bind(org.as_uuid())
        .bind(repo.as_uuid())
        .fetch_optional(&mut *tx)
        .await
        .map_err(DbError::from)?;
        tx.commit().await.map_err(DbError::from)?;
        Ok(row.as_ref().map(recipe_from_row).transpose()?)
    }

    async fn list_recipes(&self, org: OrgId, repo: RepoId, page: PageRequest) -> Result<Page<Recipe>> {
        let after = page
            .after
            .as_ref()
            .map(|c| {
                let (at, hash) = decode_cursor(c)?;
                let hash = hex::decode(&hash).map_err(|_| RepoStoreError::Invalid(format!("bad cursor {:?}", c.0)))?;
                Ok::<_, RepoStoreError>((at, hash))
            })
            .transpose()?;
        let limit = page.effective_limit();
        let mut tx = begin_org(&self.pools.tenant, org).await?;
        let rows = sqlx::query(&format!(
            "SELECT {RECIPE_COLUMNS} FROM recipes WHERE org_id = $1 AND repo_id = $2 \
               AND ($3::timestamptz IS NULL OR created_at < $3 OR (created_at = $3 AND hash > $4)) \
             ORDER BY created_at DESC, hash LIMIT $5"
        ))
        .bind(org.as_uuid())
        .bind(repo.as_uuid())
        .bind(after.as_ref().map(|a| a.0))
        .bind(after.as_ref().map(|a| a.1.clone()))
        .bind(limit_plus_one(&page))
        .fetch_all(&mut *tx)
        .await
        .map_err(DbError::from)?;
        tx.commit().await.map_err(DbError::from)?;
        let more = rows.len() > limit;
        let mut items = Vec::with_capacity(limit);
        let mut last = None;
        for row in rows.iter().take(limit) {
            items.push(recipe_from_row(row)?);
            last = Some(row.try_get::<OffsetDateTime, _>("created_at").map_err(DbError::from)?);
        }
        let next = match (more, last, items.last()) {
            (true, Some(at), Some(recipe)) => Some(encode_cursor(at, &recipe.hash.to_hex())),
            _ => None,
        };
        Ok(Page { items, next })
    }

    async fn update_recipe(&self, org: OrgId, recipe: &Recipe, expect: &RecipeStatus, jobs: &[NewJob]) -> Result<()> {
        if recipe.org_id != org {
            return Err(cross_tenant());
        }
        jobs.iter().try_for_each(NewJob::validate)?;
        let now = self.now();
        let (status, validated_at, broken_reason, broken_at) = recipe_status_columns(&recipe.status);
        retry_tx(|| async {
            let mut tx = begin_org(&self.pools.tenant, org).await?;
            let row = sqlx::query(&format!(
                "SELECT {RECIPE_COLUMNS} FROM recipes WHERE org_id = $1 AND repo_id = $2 AND hash = $3 FOR UPDATE"
            ))
            .bind(org.as_uuid())
            .bind(recipe.repo_id.as_uuid())
            .bind(recipe.hash.as_bytes().as_slice())
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(TxError::Port(RepoStoreError::NotFound))?;
            let stored = recipe_from_row(&row)?;
            if stored.status != *expect {
                return Err(TxError::Port(RepoStoreError::Conflict));
            }
            sqlx::query(
                "UPDATE recipes SET status = $4, validated_at_sha = $5, validated_at = COALESCE($6, validated_at), \
                        broken_reason = $7, broken_at = $8 \
                 WHERE org_id = $1 AND repo_id = $2 AND hash = $3",
            )
            .bind(org.as_uuid())
            .bind(recipe.repo_id.as_uuid())
            .bind(recipe.hash.as_bytes().as_slice())
            .bind(status)
            .bind(recipe.validated_at_sha.map(|s| s.as_bytes().to_vec()))
            .bind(validated_at)
            .bind(broken_reason)
            .bind(broken_at)
            .execute(&mut *tx)
            .await?;
            enqueue_all_in(&mut tx, jobs, now, &*self.ids).await?;
            tx.commit().await?;
            Ok(())
        })
        .await
    }

    async fn record_deployment(&self, org: OrgId, deployment: &Deployment) -> Result<bool> {
        if deployment.org_id != org {
            return Err(cross_tenant());
        }
        let mut tx = begin_org(&self.pools.tenant, org).await?;
        let inserted: Option<i32> = sqlx::query_scalar(
            "INSERT INTO deployments (org_id, repo_id, environment, observed_at, sha, source) VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT (org_id, repo_id, environment, observed_at) DO NOTHING RETURNING 1",
        )
        .bind(org.as_uuid())
        .bind(deployment.repo_id.as_uuid())
        .bind(&deployment.environment)
        .bind(deployment.observed_at)
        .bind(deployment.sha.as_bytes().as_slice())
        .bind(deployment_source_str(deployment.source))
        .fetch_optional(&mut *tx)
        .await
        .map_err(DbError::from)?;
        tx.commit().await.map_err(DbError::from)?;
        Ok(inserted.is_some())
    }

    async fn latest_deployment(&self, org: OrgId, repo: RepoId, environment: &str, as_of: OffsetDateTime) -> Result<Option<Deployment>> {
        let mut tx = begin_org(&self.pools.tenant, org).await?;
        let row = sqlx::query(
            "SELECT org_id, repo_id, environment, observed_at, sha, source FROM deployments \
             WHERE org_id = $1 AND repo_id = $2 AND environment = $3 AND observed_at <= $4 \
             ORDER BY observed_at DESC LIMIT 1",
        )
        .bind(org.as_uuid())
        .bind(repo.as_uuid())
        .bind(environment)
        .bind(as_of)
        .fetch_optional(&mut *tx)
        .await
        .map_err(DbError::from)?;
        tx.commit().await.map_err(DbError::from)?;
        Ok(row.as_ref().map(deployment_from_row).transpose()?)
    }

    async fn list_deployments(&self, org: OrgId, repo: RepoId, page: PageRequest) -> Result<Page<Deployment>> {
        let after = page.after.as_ref().map(decode_cursor).transpose()?;
        let limit = page.effective_limit();
        let mut tx = begin_org(&self.pools.tenant, org).await?;
        let rows = sqlx::query(
            "SELECT org_id, repo_id, environment, observed_at, sha, source FROM deployments \
             WHERE org_id = $1 AND repo_id = $2 \
               AND ($3::timestamptz IS NULL OR observed_at < $3 OR (observed_at = $3 AND environment COLLATE \"C\" > $4)) \
             ORDER BY observed_at DESC, environment COLLATE \"C\" LIMIT $5",
        )
        .bind(org.as_uuid())
        .bind(repo.as_uuid())
        .bind(after.as_ref().map(|a| a.0))
        .bind(after.as_ref().map(|a| a.1.clone()))
        .bind(limit_plus_one(&page))
        .fetch_all(&mut *tx)
        .await
        .map_err(DbError::from)?;
        tx.commit().await.map_err(DbError::from)?;
        let more = rows.len() > limit;
        let items = rows.iter().take(limit).map(deployment_from_row).collect::<Result<Vec<_>, _>>()?;
        let next = match (more, items.last()) {
            (true, Some(d)) => Some(encode_cursor(d.observed_at, &d.environment)),
            _ => None,
        };
        Ok(Page { items, next })
    }
}

/// `(status, validated_at, broken_reason, broken_at)` columns.
fn recipe_status_columns(status: &RecipeStatus) -> (&'static str, Option<OffsetDateTime>, Option<&str>, Option<OffsetDateTime>) {
    match status {
        RecipeStatus::Draft => ("draft", None, None, None),
        RecipeStatus::Validated { at } => ("validated", Some(*at), None, None),
        RecipeStatus::Broken { reason, at } => ("broken", None, Some(reason.as_str()), Some(*at)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn column_codecs_round_trip() {
        for layer in Layer::ALL {
            assert_eq!(parse_layer(layer_str(layer)), Ok(layer));
        }
        let ticket = TicketId::from_uuid(Uuid::from_u128(5));
        for reason in [VersionReason::DefaultHead, VersionReason::Release("v1".into()), VersionReason::Deployed("prod".into()), VersionReason::OnDemand(ticket)] {
            let (kind, detail) = reason_columns(&reason);
            assert_eq!(version_reason(kind, detail), Ok(reason));
        }
        let at = OffsetDateTime::UNIX_EPOCH;
        for state in [
            LayerState::Pending,
            LayerState::Running,
            LayerState::Ready { at },
            LayerState::Failed { reason: "f".into() },
            LayerState::Skipped { reason: "s".into() },
        ] {
            let (name, reason, ready_at) = layer_columns(&state);
            assert_eq!(layer_state(name, reason.map(str::to_owned), ready_at), Ok(state));
        }
        for source in [DeploymentSource::GitHubDeployments, DeploymentSource::Manual, DeploymentSource::Tag] {
            assert_eq!(parse_deployment_source(deployment_source_str(source)), Ok(source));
        }
    }

    #[test]
    fn cursor_round_trips_and_rejects_garbage() {
        let at = OffsetDateTime::from_unix_timestamp(1_767_225_600).unwrap_or(OffsetDateTime::UNIX_EPOCH);
        assert_eq!(decode_cursor(&encode_cursor(at, "prod/eu")), Ok((at, "prod/eu".to_owned())));
        assert!(matches!(decode_cursor(&Cursor("x".into())), Err(RepoStoreError::Invalid(_))));
    }
}
