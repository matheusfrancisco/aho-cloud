//! Postgres [`Store`] on sqlx's runtime query API, with embedded migrations.

use async_trait::async_trait;
use cloud_contract::ledger::{Approver, Stage};
use cloud_contract::policy::ReleasePolicy;
use sqlx::Row;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions, PgRow};

use super::*;

/// A [`Store`] backed by a Postgres pool.
#[derive(Clone)]
pub struct PgStore {
    pool: PgPool,
}

fn backend(e: impl std::fmt::Display) -> StoreError {
    StoreError::Backend(e.to_string())
}

/// Maps a unique violation to [`StoreError::Conflict`] naming `what`.
fn write_err(what: impl Into<String>) -> impl FnOnce(sqlx::Error) -> StoreError {
    let what = what.into();
    move |e| match &e {
        sqlx::Error::Database(db) if db.is_unique_violation() => StoreError::Conflict(what),
        _ => backend(e),
    }
}

fn i64_of(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

fn u64_of(v: i64) -> u64 {
    u64::try_from(v).unwrap_or(0)
}

fn stage_of(s: &str) -> Result<Stage, StoreError> {
    match s {
        "preview" => Ok(Stage::Preview),
        "staging" => Ok(Stage::Staging),
        "live" => Ok(Stage::Live),
        other => Err(StoreError::Backend(format!("unknown stage `{other}` in row"))),
    }
}

fn get<'r, T: sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>>(row: &'r PgRow, col: &str) -> Result<T, StoreError> {
    row.try_get(col).map_err(backend)
}

fn user_of(row: &PgRow) -> Result<UserRec, StoreError> {
    Ok(UserRec { id: get(row, "id")?, email: get(row, "email")?, name: get(row, "name")? })
}

fn device_code_of(row: &PgRow) -> Result<DeviceCodeRec, StoreError> {
    Ok(DeviceCodeRec {
        code_hash: get(row, "code_hash")?,
        user_code: get(row, "user_code")?,
        device_name: get(row, "device_name")?,
        public_key: get(row, "public_key")?,
        expires_at: u64_of(get(row, "expires_at")?),
        approved_by: get(row, "approved_by")?,
    })
}

fn device_key_of(row: &PgRow) -> Result<DeviceKeyRec, StoreError> {
    Ok(DeviceKeyRec {
        key_id: get(row, "key_id")?,
        user_id: get(row, "user_id")?,
        name: get(row, "name")?,
        public_key: get(row, "public_key")?,
        created_at: u64_of(get(row, "created_at")?),
    })
}

fn passkey_of(row: &PgRow) -> Result<PasskeyRec, StoreError> {
    Ok(PasskeyRec {
        credential_id: get(row, "credential_id")?,
        user_id: get(row, "user_id")?,
        public_key: get(row, "public_key")?,
        sign_count: u32::try_from(get::<i64>(row, "sign_count")?).unwrap_or(u32::MAX),
        created_at: u64_of(get(row, "created_at")?),
    })
}

fn ceremony_of(row: &PgRow) -> Result<CeremonyRec, StoreError> {
    let kind: String = get(row, "kind")?;
    Ok(CeremonyRec {
        id: get(row, "id")?,
        kind: CeremonyKind::parse(&kind).ok_or_else(|| backend(format!("unknown ceremony kind `{kind}`")))?,
        challenge: get(row, "challenge")?,
        user_id: get(row, "user_id")?,
        email: get(row, "email")?,
        name: get(row, "name")?,
        new_user: get(row, "new_user")?,
        expires_at: u64_of(get(row, "expires_at")?),
    })
}

fn org_of(row: &PgRow) -> Result<OrgRec, StoreError> {
    let policy: String = get(row, "policy")?;
    let approver: Option<String> = get(row, "policy_approver")?;
    Ok(OrgRec {
        slug: get(row, "slug")?,
        name: get(row, "name")?,
        policy: serde_json::from_str(&policy).map_err(backend)?,
        policy_approver: approver.map(|a| serde_json::from_str(&a)).transpose().map_err(backend)?,
    })
}

fn member_of(row: &PgRow) -> Result<MemberRec, StoreError> {
    Ok(MemberRec { org: get(row, "org")?, user_id: get(row, "user_id")?, owner: get(row, "owner")? })
}

fn project_of(row: &PgRow) -> Result<ProjectRec, StoreError> {
    Ok(ProjectRec {
        slug: get(row, "slug")?,
        org: get(row, "org")?,
        created_at: u64_of(get(row, "created_at")?),
        deleted_at: get::<Option<i64>>(row, "deleted_at")?.map(u64_of),
    })
}

fn secret_of(row: &PgRow) -> Result<SecretRec, StoreError> {
    Ok(SecretRec {
        project: get(row, "project")?,
        name: get(row, "name")?,
        stage: stage_of(&get::<String>(row, "stage")?)?,
        ciphertext: get(row, "ciphertext")?,
        fingerprint: get(row, "fingerprint")?,
        edge_phc: get(row, "edge_phc")?,
        updated_at: u64_of(get(row, "updated_at")?),
    })
}

fn blob_of(row: &PgRow) -> Result<BlobRec, StoreError> {
    let kind: String = get(row, "kind")?;
    Ok(BlobRec {
        project: get(row, "project")?,
        hash: get(row, "hash")?,
        kind: BlobKind::parse(&kind).ok_or_else(|| backend(format!("unknown blob kind `{kind}`")))?,
        size: u64_of(get(row, "size")?),
        created_at: u64_of(get(row, "created_at")?),
    })
}

fn session_of(row: &PgRow) -> Result<SessionRec, StoreError> {
    Ok(SessionRec {
        project: get(row, "project")?,
        session_id: get(row, "session_id")?,
        head_hash: get(row, "head_hash")?,
        blob_hash: get(row, "blob_hash")?,
        entries: u64_of(get(row, "entries")?),
        created_at: u64_of(get(row, "created_at")?),
    })
}

fn release_meta_of(row: &PgRow) -> Result<ReleaseMeta, StoreError> {
    let build: String = get(row, "build")?;
    Ok(ReleaseMeta {
        project: get(row, "project")?,
        release: get(row, "release")?,
        edge_toml: get(row, "edge_toml")?,
        build: serde_json::from_str(&build).map_err(backend)?,
        build_log: get(row, "build_log")?,
        created_at: u64_of(get(row, "created_at")?),
    })
}

fn all<T>(rows: Vec<PgRow>, f: fn(&PgRow) -> Result<T, StoreError>) -> Result<Vec<T>, StoreError> {
    rows.iter().map(f).collect()
}

impl PgStore {
    /// Connects and applies the embedded migrations.
    pub async fn connect(url: &str) -> Result<Self, StoreError> {
        let options: PgConnectOptions = url.parse().map_err(backend)?;
        Self::connect_with(options).await
    }

    /// Connects with `search_path` set to `schema` (created if missing) and migrates it; lets
    /// tests share one database without seeing each other.
    pub async fn connect_schema(url: &str, schema: &str) -> Result<Self, StoreError> {
        if schema.is_empty() || !schema.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') {
            return Err(StoreError::Backend(format!("`{schema}` is not a plain schema name")));
        }
        let options: PgConnectOptions = url.parse().map_err(backend)?;
        let admin = PgPoolOptions::new().max_connections(1).connect_with(options.clone()).await.map_err(backend)?;
        sqlx::raw_sql(sqlx::AssertSqlSafe(format!("CREATE SCHEMA IF NOT EXISTS {schema}"))).execute(&admin).await.map_err(backend)?;
        admin.close().await;
        Self::connect_with(options.options([("search_path", schema)])).await
    }

    async fn connect_with(options: PgConnectOptions) -> Result<Self, StoreError> {
        let pool = PgPoolOptions::new().max_connections(16).connect_with(options).await.map_err(backend)?;
        sqlx::migrate!("./migrations").run(&pool).await.map_err(backend)?;
        Ok(Self { pool })
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }
}

#[async_trait]
impl Store for PgStore {
    async fn create_user(&self, user: &UserRec) -> Result<(), StoreError> {
        sqlx::query("INSERT INTO users (id, email, name) VALUES ($1, $2, $3)")
            .bind(&user.id)
            .bind(&user.email)
            .bind(&user.name)
            .execute(&self.pool)
            .await
            .map_err(write_err(format!("user {}", user.email)))?;
        Ok(())
    }

    async fn user(&self, id: &str) -> Result<Option<UserRec>, StoreError> {
        let row =
            sqlx::query("SELECT id, email, name FROM users WHERE id = $1").bind(id).fetch_optional(&self.pool).await.map_err(backend)?;
        row.as_ref().map(user_of).transpose()
    }

    async fn user_by_email(&self, email: &str) -> Result<Option<UserRec>, StoreError> {
        let row = sqlx::query("SELECT id, email, name FROM users WHERE email = $1")
            .bind(email)
            .fetch_optional(&self.pool)
            .await
            .map_err(backend)?;
        row.as_ref().map(user_of).transpose()
    }

    async fn put_device_code(&self, code: &DeviceCodeRec) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO device_codes (code_hash, user_code, device_name, public_key, expires_at, approved_by) VALUES ($1, $2, $3, $4, $5, $6)",
        )
        .bind(&code.code_hash)
        .bind(&code.user_code)
        .bind(&code.device_name)
        .bind(&code.public_key)
        .bind(i64_of(code.expires_at))
        .bind(&code.approved_by)
        .execute(&self.pool)
        .await
        .map_err(write_err(format!("device code {}", code.user_code)))?;
        Ok(())
    }

    async fn device_code(&self, code_hash: &str) -> Result<Option<DeviceCodeRec>, StoreError> {
        let row = sqlx::query("SELECT * FROM device_codes WHERE code_hash = $1")
            .bind(code_hash)
            .fetch_optional(&self.pool)
            .await
            .map_err(backend)?;
        row.as_ref().map(device_code_of).transpose()
    }

    async fn device_code_by_user_code(&self, user_code: &str) -> Result<Option<DeviceCodeRec>, StoreError> {
        let row = sqlx::query("SELECT * FROM device_codes WHERE user_code = $1")
            .bind(user_code)
            .fetch_optional(&self.pool)
            .await
            .map_err(backend)?;
        row.as_ref().map(device_code_of).transpose()
    }

    async fn approve_device_code(&self, code_hash: &str, user_id: &str) -> Result<bool, StoreError> {
        let done = sqlx::query("UPDATE device_codes SET approved_by = $2 WHERE code_hash = $1 AND approved_by IS NULL")
            .bind(code_hash)
            .bind(user_id)
            .execute(&self.pool)
            .await
            .map_err(backend)?;
        Ok(done.rows_affected() == 1)
    }

    async fn take_device_code(&self, code_hash: &str) -> Result<Option<DeviceCodeRec>, StoreError> {
        let row = sqlx::query("DELETE FROM device_codes WHERE code_hash = $1 RETURNING *")
            .bind(code_hash)
            .fetch_optional(&self.pool)
            .await
            .map_err(backend)?;
        row.as_ref().map(device_code_of).transpose()
    }

    async fn put_token(&self, token: &TokenRec) -> Result<(), StoreError> {
        sqlx::query("INSERT INTO tokens (hash, user_id, expires_at) VALUES ($1, $2, $3)")
            .bind(&token.hash)
            .bind(&token.user_id)
            .bind(i64_of(token.expires_at))
            .execute(&self.pool)
            .await
            .map_err(write_err("token"))?;
        Ok(())
    }

    async fn token(&self, hash: &str) -> Result<Option<TokenRec>, StoreError> {
        let row = sqlx::query("SELECT hash, user_id, expires_at FROM tokens WHERE hash = $1")
            .bind(hash)
            .fetch_optional(&self.pool)
            .await
            .map_err(backend)?;
        row.map(|r| Ok(TokenRec { hash: get(&r, "hash")?, user_id: get(&r, "user_id")?, expires_at: u64_of(get(&r, "expires_at")?) }))
            .transpose()
    }

    async fn put_device_key(&self, key: &DeviceKeyRec) -> Result<(), StoreError> {
        sqlx::query("INSERT INTO device_keys (key_id, user_id, name, public_key, created_at) VALUES ($1, $2, $3, $4, $5)")
            .bind(&key.key_id)
            .bind(&key.user_id)
            .bind(&key.name)
            .bind(&key.public_key)
            .bind(i64_of(key.created_at))
            .execute(&self.pool)
            .await
            .map_err(write_err(format!("device key {}", key.key_id)))?;
        Ok(())
    }

    async fn device_key(&self, key_id: &str) -> Result<Option<DeviceKeyRec>, StoreError> {
        let row =
            sqlx::query("SELECT * FROM device_keys WHERE key_id = $1").bind(key_id).fetch_optional(&self.pool).await.map_err(backend)?;
        row.as_ref().map(device_key_of).transpose()
    }

    async fn device_keys_for_user(&self, user_id: &str) -> Result<Vec<DeviceKeyRec>, StoreError> {
        let rows = sqlx::query("SELECT * FROM device_keys WHERE user_id = $1 ORDER BY created_at, key_id")
            .bind(user_id)
            .fetch_all(&self.pool)
            .await
            .map_err(backend)?;
        all(rows, device_key_of)
    }

    async fn put_passkey(&self, passkey: &PasskeyRec) -> Result<(), StoreError> {
        sqlx::query("INSERT INTO passkeys (credential_id, user_id, public_key, sign_count, created_at) VALUES ($1, $2, $3, $4, $5)")
            .bind(&passkey.credential_id)
            .bind(&passkey.user_id)
            .bind(&passkey.public_key)
            .bind(i64::from(passkey.sign_count))
            .bind(i64_of(passkey.created_at))
            .execute(&self.pool)
            .await
            .map_err(write_err("passkey credential"))?;
        Ok(())
    }

    async fn passkey(&self, credential_id: &str) -> Result<Option<PasskeyRec>, StoreError> {
        let row = sqlx::query("SELECT * FROM passkeys WHERE credential_id = $1")
            .bind(credential_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(backend)?;
        row.as_ref().map(passkey_of).transpose()
    }

    async fn passkeys_for_user(&self, user_id: &str) -> Result<Vec<PasskeyRec>, StoreError> {
        let rows = sqlx::query("SELECT * FROM passkeys WHERE user_id = $1 ORDER BY created_at, credential_id")
            .bind(user_id)
            .fetch_all(&self.pool)
            .await
            .map_err(backend)?;
        all(rows, passkey_of)
    }

    async fn update_sign_count(&self, credential_id: &str, old: u32, new: u32) -> Result<bool, StoreError> {
        let done = sqlx::query("UPDATE passkeys SET sign_count = $3 WHERE credential_id = $1 AND sign_count = $2")
            .bind(credential_id)
            .bind(i64::from(old))
            .bind(i64::from(new))
            .execute(&self.pool)
            .await
            .map_err(backend)?;
        Ok(done.rows_affected() == 1)
    }

    async fn put_ceremony(&self, c: &CeremonyRec) -> Result<(), StoreError> {
        sqlx::query("INSERT INTO ceremonies (id, kind, challenge, user_id, email, name, new_user, expires_at) VALUES ($1, $2, $3, $4, $5, $6, $7, $8)")
            .bind(&c.id)
            .bind(c.kind.as_str())
            .bind(&c.challenge)
            .bind(&c.user_id)
            .bind(&c.email)
            .bind(&c.name)
            .bind(c.new_user)
            .bind(i64_of(c.expires_at))
            .execute(&self.pool)
            .await
            .map_err(write_err("ceremony"))?;
        Ok(())
    }

    async fn take_ceremony(&self, id: &str) -> Result<Option<CeremonyRec>, StoreError> {
        let row =
            sqlx::query("DELETE FROM ceremonies WHERE id = $1 RETURNING *").bind(id).fetch_optional(&self.pool).await.map_err(backend)?;
        row.as_ref().map(ceremony_of).transpose()
    }

    async fn create_org(&self, org: &OrgRec, owner: &str) -> Result<(), StoreError> {
        let policy = serde_json::to_string(&org.policy).map_err(backend)?;
        let approver = org.policy_approver.as_ref().map(serde_json::to_string).transpose().map_err(backend)?;
        let mut tx = self.pool.begin().await.map_err(backend)?;
        sqlx::query("INSERT INTO orgs (slug, name, policy, policy_approver) VALUES ($1, $2, $3, $4)")
            .bind(&org.slug)
            .bind(&org.name)
            .bind(policy)
            .bind(approver)
            .execute(&mut *tx)
            .await
            .map_err(write_err(format!("org {}", org.slug)))?;
        sqlx::query("INSERT INTO members (org, user_id, owner) VALUES ($1, $2, TRUE)")
            .bind(&org.slug)
            .bind(owner)
            .execute(&mut *tx)
            .await
            .map_err(backend)?;
        tx.commit().await.map_err(backend)
    }

    async fn org(&self, slug: &str) -> Result<Option<OrgRec>, StoreError> {
        let row = sqlx::query("SELECT * FROM orgs WHERE slug = $1").bind(slug).fetch_optional(&self.pool).await.map_err(backend)?;
        row.as_ref().map(org_of).transpose()
    }

    async fn set_org_policy(&self, slug: &str, policy: &ReleasePolicy, approver: &Approver) -> Result<(), StoreError> {
        let done = sqlx::query("UPDATE orgs SET policy = $2, policy_approver = $3 WHERE slug = $1")
            .bind(slug)
            .bind(serde_json::to_string(policy).map_err(backend)?)
            .bind(serde_json::to_string(approver).map_err(backend)?)
            .execute(&self.pool)
            .await
            .map_err(backend)?;
        if done.rows_affected() == 1 { Ok(()) } else { Err(StoreError::Backend(format!("unknown org {slug}"))) }
    }

    async fn put_member(&self, m: &MemberRec) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO members (org, user_id, owner) VALUES ($1, $2, $3) ON CONFLICT (org, user_id) DO UPDATE SET owner = EXCLUDED.owner",
        )
        .bind(&m.org)
        .bind(&m.user_id)
        .bind(m.owner)
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(())
    }

    async fn member(&self, org: &str, user_id: &str) -> Result<Option<MemberRec>, StoreError> {
        let row = sqlx::query("SELECT * FROM members WHERE org = $1 AND user_id = $2")
            .bind(org)
            .bind(user_id)
            .fetch_optional(&self.pool)
            .await
            .map_err(backend)?;
        row.as_ref().map(member_of).transpose()
    }

    async fn members(&self, org: &str) -> Result<Vec<MemberRec>, StoreError> {
        let rows =
            sqlx::query("SELECT * FROM members WHERE org = $1 ORDER BY user_id").bind(org).fetch_all(&self.pool).await.map_err(backend)?;
        all(rows, member_of)
    }

    async fn memberships(&self, user_id: &str) -> Result<Vec<MemberRec>, StoreError> {
        let rows = sqlx::query("SELECT * FROM members WHERE user_id = $1 ORDER BY org")
            .bind(user_id)
            .fetch_all(&self.pool)
            .await
            .map_err(backend)?;
        all(rows, member_of)
    }

    async fn create_project(&self, p: &ProjectRec) -> Result<(), StoreError> {
        sqlx::query("INSERT INTO projects (slug, org, created_at, deleted_at) VALUES ($1, $2, $3, $4)")
            .bind(&p.slug)
            .bind(&p.org)
            .bind(i64_of(p.created_at))
            .bind(p.deleted_at.map(i64_of))
            .execute(&self.pool)
            .await
            .map_err(write_err(format!("project {}", p.slug)))?;
        Ok(())
    }

    async fn project(&self, slug: &str) -> Result<Option<ProjectRec>, StoreError> {
        let row = sqlx::query("SELECT * FROM projects WHERE slug = $1").bind(slug).fetch_optional(&self.pool).await.map_err(backend)?;
        row.as_ref().map(project_of).transpose()
    }

    async fn projects(&self) -> Result<Vec<ProjectRec>, StoreError> {
        let rows = sqlx::query("SELECT * FROM projects ORDER BY slug").fetch_all(&self.pool).await.map_err(backend)?;
        all(rows, project_of)
    }

    async fn delete_project(&self, slug: &str, at: u64) -> Result<(), StoreError> {
        sqlx::query("UPDATE projects SET deleted_at = COALESCE(deleted_at, $2) WHERE slug = $1")
            .bind(slug)
            .bind(i64_of(at))
            .execute(&self.pool)
            .await
            .map_err(backend)?;
        Ok(())
    }

    async fn put_secret(&self, s: &SecretRec) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO secrets (project, name, stage, ciphertext, fingerprint, edge_phc, updated_at) VALUES ($1, $2, $3, $4, $5, $6, $7) \
             ON CONFLICT (project, name, stage) DO UPDATE SET ciphertext = EXCLUDED.ciphertext, fingerprint = EXCLUDED.fingerprint, \
             edge_phc = EXCLUDED.edge_phc, updated_at = EXCLUDED.updated_at",
        )
        .bind(&s.project)
        .bind(&s.name)
        .bind(s.stage.as_str())
        .bind(&s.ciphertext)
        .bind(&s.fingerprint)
        .bind(&s.edge_phc)
        .bind(i64_of(s.updated_at))
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(())
    }

    async fn secrets(&self, project: &str) -> Result<Vec<SecretRec>, StoreError> {
        let rows = sqlx::query("SELECT * FROM secrets WHERE project = $1 ORDER BY name, stage")
            .bind(project)
            .fetch_all(&self.pool)
            .await
            .map_err(backend)?;
        all(rows, secret_of)
    }

    async fn put_blob(&self, b: &BlobRec) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO blobs (project, hash, kind, size, created_at) VALUES ($1, $2, $3, $4, $5) ON CONFLICT (project, hash) DO NOTHING",
        )
        .bind(&b.project)
        .bind(&b.hash)
        .bind(b.kind.as_str())
        .bind(i64_of(b.size))
        .bind(i64_of(b.created_at))
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(())
    }

    async fn blob(&self, project: &str, hash: &str) -> Result<Option<BlobRec>, StoreError> {
        let row = sqlx::query("SELECT * FROM blobs WHERE project = $1 AND hash = $2")
            .bind(project)
            .bind(hash)
            .fetch_optional(&self.pool)
            .await
            .map_err(backend)?;
        row.as_ref().map(blob_of).transpose()
    }

    async fn archive_exists(&self, hash: &str) -> Result<bool, StoreError> {
        let row = sqlx::query("SELECT 1 AS one FROM blobs WHERE hash = $1 AND kind = 'archive' LIMIT 1")
            .bind(hash)
            .fetch_optional(&self.pool)
            .await
            .map_err(backend)?;
        Ok(row.is_some())
    }

    async fn put_session(&self, s: &SessionRec) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO sessions (project, session_id, head_hash, blob_hash, entries, created_at) VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT (project, session_id, head_hash) DO NOTHING",
        )
        .bind(&s.project)
        .bind(&s.session_id)
        .bind(&s.head_hash)
        .bind(&s.blob_hash)
        .bind(i64_of(s.entries))
        .bind(i64_of(s.created_at))
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(())
    }

    async fn session(&self, project: &str, session_id: &str, head_hash: &str) -> Result<Option<SessionRec>, StoreError> {
        let row = sqlx::query("SELECT * FROM sessions WHERE project = $1 AND session_id = $2 AND head_hash = $3")
            .bind(project)
            .bind(session_id)
            .bind(head_hash)
            .fetch_optional(&self.pool)
            .await
            .map_err(backend)?;
        row.as_ref().map(session_of).transpose()
    }

    async fn sessions(&self, project: &str) -> Result<Vec<SessionRec>, StoreError> {
        let rows = sqlx::query("SELECT * FROM sessions WHERE project = $1 ORDER BY created_at, session_id")
            .bind(project)
            .fetch_all(&self.pool)
            .await
            .map_err(backend)?;
        all(rows, session_of)
    }

    async fn put_release_meta(&self, m: &ReleaseMeta) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO release_meta (project, release, edge_toml, build, build_log, created_at) VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT (project, release) DO UPDATE SET edge_toml = EXCLUDED.edge_toml, build = EXCLUDED.build, build_log = EXCLUDED.build_log",
        )
        .bind(&m.project)
        .bind(&m.release)
        .bind(&m.edge_toml)
        .bind(serde_json::to_string(&m.build).map_err(backend)?)
        .bind(&m.build_log)
        .bind(i64_of(m.created_at))
        .execute(&self.pool)
        .await
        .map_err(backend)?;
        Ok(())
    }

    async fn release_meta(&self, project: &str, release: &str) -> Result<Option<ReleaseMeta>, StoreError> {
        let row = sqlx::query("SELECT * FROM release_meta WHERE project = $1 AND release = $2")
            .bind(project)
            .bind(release)
            .fetch_optional(&self.pool)
            .await
            .map_err(backend)?;
        row.as_ref().map(release_meta_of).transpose()
    }

    async fn kv_get(&self, key: &str) -> Result<Option<String>, StoreError> {
        let row = sqlx::query("SELECT value FROM kv WHERE key = $1").bind(key).fetch_optional(&self.pool).await.map_err(backend)?;
        row.map(|r| get(&r, "value")).transpose()
    }

    async fn kv_put(&self, key: &str, value: &str) -> Result<(), StoreError> {
        sqlx::query("INSERT INTO kv (key, value) VALUES ($1, $2) ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value")
            .bind(key)
            .bind(value)
            .execute(&self.pool)
            .await
            .map_err(backend)?;
        Ok(())
    }
}
