//! `RunnerState` on SQLite (`<data_dir>/state.db`, WAL, `PRAGMA foreign_keys = ON`), executor.md
//! "`RunnerState`". `rusqlite` behind a mutex, called on tokio's blocking pool.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use mage_core::domain::{OrgId, RunId, RunKind};
use mage_core::ports::Clock;
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::ports::executor::{BaseId, BaseKey, BaseKind, EnvId, ForkMode, ForkPurpose, PortName, ProcessRole};
use crate::ports::runner_state::{
    ActiveRun, BaseClaim, BaseMeta, BaseRecord, BaseStatus, EnvResource, ForkRecord, ForkStatus, PortLease, PortOwner,
    PortRange, ProcessRecord, RecoverySnapshot, RunPhase, RunnerState, RunnerStateError,
};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS runs      (run_id TEXT PRIMARY KEY, org_id TEXT, kind TEXT, attempt INT, phase TEXT, env_id TEXT, started_at TEXT);
CREATE TABLE IF NOT EXISTS processes (pgid INT PRIMARY KEY, pid INT, start_time INT, boot_id TEXT, run_id TEXT, env_id TEXT, role TEXT, cgroup TEXT);
CREATE TABLE IF NOT EXISTS bases     (base_id TEXT PRIMARY KEY, key_json TEXT UNIQUE, kind TEXT, status TEXT, dir TEXT, bytes INT,
                                      fork_mode TEXT, template_db TEXT, created_at TEXT, last_used_at TEXT);
CREATE TABLE IF NOT EXISTS forks     (env_id TEXT PRIMARY KEY, base_id TEXT REFERENCES bases(base_id), run_id TEXT, purpose TEXT,
                                      dir TEXT, status TEXT, created_at TEXT);
CREATE TABLE IF NOT EXISTS resources (env_id TEXT REFERENCES forks(env_id) ON DELETE CASCADE, kind TEXT, value TEXT, PRIMARY KEY (env_id, kind, value));
CREATE TABLE IF NOT EXISTS ports     (port INT PRIMARY KEY, owner_kind TEXT, owner_id TEXT, name TEXT, at TEXT);
CREATE TABLE IF NOT EXISTS port_quarantine (port INT PRIMARY KEY, until TEXT);
";

/// SQLite-backed `RunnerState`.
pub struct SqliteRunnerState {
    conn: Arc<Mutex<Connection>>,
    clock: Arc<dyn Clock>,
}

impl SqliteRunnerState {
    /// Opens (creating if needed) the database at `path` with mode 0600, WAL, foreign keys on, and
    /// the executor.md schema.
    pub fn open(path: &Path, clock: Arc<dyn Clock>) -> Result<Self, RunnerStateError> {
        let existed = path.exists();
        let conn = Connection::open(path).map_err(unavailable)?;
        if !existed {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
                .map_err(|e| RunnerStateError::Unavailable(format!("chmod state.db: {e}")))?;
        }
        conn.busy_timeout(std::time::Duration::from_secs(10)).map_err(unavailable)?;
        conn.pragma_update(None, "journal_mode", "WAL").map_err(unavailable)?;
        conn.pragma_update(None, "foreign_keys", "ON").map_err(unavailable)?;
        conn.pragma_update(None, "synchronous", "NORMAL").map_err(unavailable)?;
        conn.execute_batch(SCHEMA).map_err(unavailable)?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)), clock })
    }

    async fn with<T, F>(&self, f: F) -> Result<T, RunnerStateError>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection, OffsetDateTime) -> Result<T, RunnerStateError> + Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        let now = self.clock.now();
        tokio::task::spawn_blocking(move || {
            let mut guard = conn.lock();
            f(&mut guard, now)
        })
        .await
        .map_err(|e| RunnerStateError::Unavailable(format!("blocking task: {e}")))?
    }
}

fn unavailable(err: rusqlite::Error) -> RunnerStateError {
    RunnerStateError::Unavailable(err.to_string())
}

fn invalid(what: &str, err: impl std::fmt::Display) -> RunnerStateError {
    RunnerStateError::Invalid(format!("{what}: {err}"))
}

fn ts(at: OffsetDateTime) -> String {
    at.format(&Rfc3339).unwrap_or_default()
}

fn parse_ts(s: &str) -> Result<OffsetDateTime, RunnerStateError> {
    OffsetDateTime::parse(s, &Rfc3339).map_err(|e| invalid("timestamp", e))
}

fn kind_str(kind: RunKind) -> &'static str {
    match kind {
        RunKind::Learn => "learn",
        RunKind::Triage => "triage",
        RunKind::Diagnose => "diagnose",
        RunKind::Verify => "verify",
        RunKind::Fix => "fix",
    }
}

fn parse_kind(s: &str) -> Result<RunKind, RunnerStateError> {
    Ok(match s {
        "learn" => RunKind::Learn,
        "triage" => RunKind::Triage,
        "diagnose" => RunKind::Diagnose,
        "verify" => RunKind::Verify,
        "fix" => RunKind::Fix,
        other => return Err(invalid("run kind", other)),
    })
}

fn key_json(key: &BaseKey) -> Result<String, RunnerStateError> {
    serde_json::to_string(key).map_err(|e| invalid("base key", e))
}

fn parse_key(s: &str) -> Result<BaseKey, RunnerStateError> {
    serde_json::from_str(s).map_err(|e| invalid("base key", e))
}

fn get<T: rusqlite::types::FromSql>(row: &Row<'_>, idx: usize) -> Result<T, RunnerStateError> {
    row.get(idx).map_err(unavailable)
}

fn run_from_row(row: &Row<'_>) -> Result<ActiveRun, RunnerStateError> {
    let run_id: String = get(row, 0)?;
    let org_id: String = get(row, 1)?;
    let kind: String = get(row, 2)?;
    let attempt: i64 = get(row, 3)?;
    let phase: String = get(row, 4)?;
    let env: Option<String> = get(row, 5)?;
    let started_at: String = get(row, 6)?;
    Ok(ActiveRun {
        run_id: run_id.parse::<RunId>().map_err(|e| invalid("run id", e))?,
        org_id: org_id.parse::<OrgId>().map_err(|e| invalid("org id", e))?,
        kind: parse_kind(&kind)?,
        attempt: u16::try_from(attempt).map_err(|e| invalid("attempt", e))?,
        phase: RunPhase::parse(&phase)?,
        env: env.map(|e| e.parse::<EnvId>().map_err(|err| invalid("env id", err))).transpose()?,
        started_at: parse_ts(&started_at)?,
    })
}

fn process_from_row(row: &Row<'_>) -> Result<ProcessRecord, RunnerStateError> {
    let run_id: Option<String> = get(row, 4)?;
    let env: Option<String> = get(row, 5)?;
    let role: String = get(row, 6)?;
    let start_time: i64 = get(row, 2)?;
    Ok(ProcessRecord {
        pgid: get(row, 0)?,
        pid: get(row, 1)?,
        start_time: start_time as u64,
        boot_id: get(row, 3)?,
        run_id: run_id.map(|r| r.parse::<RunId>().map_err(|e| invalid("run id", e))).transpose()?,
        env: env.map(|e| e.parse::<EnvId>().map_err(|err| invalid("env id", err))).transpose()?,
        role: role.parse::<ProcessRole>().map_err(|e| invalid("role", e))?,
        cgroup: get(row, 7)?,
    })
}

const BASE_COLS: &str = "b.base_id, b.key_json, b.kind, b.status, b.dir, b.bytes, b.fork_mode, b.template_db, b.created_at, b.last_used_at, \
     (SELECT count(*) FROM forks f WHERE f.base_id = b.base_id)";

fn base_from_row(row: &Row<'_>) -> Result<BaseRecord, RunnerStateError> {
    let id: String = get(row, 0)?;
    let key: String = get(row, 1)?;
    let kind: String = get(row, 2)?;
    let status: String = get(row, 3)?;
    let bytes: i64 = get(row, 5)?;
    let fork_mode: Option<String> = get(row, 6)?;
    let created: String = get(row, 8)?;
    let used: String = get(row, 9)?;
    let live: i64 = get(row, 10)?;
    Ok(BaseRecord {
        key: parse_key(&key)?,
        id: BaseId::parse(&id).map_err(|e| invalid("base id", e))?,
        kind: BaseKind::parse(&kind).map_err(|e| invalid("base kind", e))?,
        status: match status.as_str() {
            "preparing" => BaseStatus::Preparing,
            "ready" => BaseStatus::Ready,
            other => return Err(invalid("base status", other)),
        },
        dir: get(row, 4)?,
        bytes: bytes as u64,
        created_at: parse_ts(&created)?,
        last_used_at: parse_ts(&used)?,
        live_forks: u32::try_from(live).unwrap_or(u32::MAX),
        fork_mode: fork_mode.map(|m| ForkMode::parse(&m).map_err(|e| invalid("fork mode", e))).transpose()?,
        template_db: get(row, 7)?,
    })
}

fn select_base(conn: &Connection, id: &BaseId) -> Result<Option<BaseRecord>, RunnerStateError> {
    let sql = format!("SELECT {BASE_COLS} FROM bases b WHERE b.base_id = ?1");
    let mut stmt = conn.prepare(&sql).map_err(unavailable)?;
    let mut rows = stmt.query(params![id.as_str()]).map_err(unavailable)?;
    match rows.next().map_err(unavailable)? {
        Some(row) => Ok(Some(base_from_row(row)?)),
        None => Ok(None),
    }
}

fn select_bases(conn: &Connection, filter: &str) -> Result<Vec<BaseRecord>, RunnerStateError> {
    let sql = format!("SELECT {BASE_COLS} FROM bases b {filter} ORDER BY b.created_at, b.base_id");
    let mut stmt = conn.prepare(&sql).map_err(unavailable)?;
    let mut rows = stmt.query([]).map_err(unavailable)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(unavailable)? {
        out.push(base_from_row(row)?);
    }
    Ok(out)
}

fn select_runs(conn: &Connection) -> Result<Vec<ActiveRun>, RunnerStateError> {
    let mut stmt = conn
        .prepare("SELECT run_id, org_id, kind, attempt, phase, env_id, started_at FROM runs ORDER BY started_at, run_id")
        .map_err(unavailable)?;
    let mut rows = stmt.query([]).map_err(unavailable)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(unavailable)? {
        out.push(run_from_row(row)?);
    }
    Ok(out)
}

fn select_processes(conn: &Connection) -> Result<Vec<ProcessRecord>, RunnerStateError> {
    let mut stmt = conn
        .prepare("SELECT pgid, pid, start_time, boot_id, run_id, env_id, role, cgroup FROM processes ORDER BY pgid")
        .map_err(unavailable)?;
    let mut rows = stmt.query([]).map_err(unavailable)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(unavailable)? {
        out.push(process_from_row(row)?);
    }
    Ok(out)
}

fn select_forks(conn: &Connection) -> Result<Vec<ForkRecord>, RunnerStateError> {
    let mut stmt = conn
        .prepare(
            "SELECT f.env_id, b.key_json, f.run_id, f.purpose, f.dir, f.status, f.created_at \
             FROM forks f LEFT JOIN bases b ON b.base_id = f.base_id ORDER BY f.created_at, f.env_id",
        )
        .map_err(unavailable)?;
    let mut rows = stmt.query([]).map_err(unavailable)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(unavailable)? {
        let env: String = get(row, 0)?;
        let key: Option<String> = get(row, 1)?;
        let run: String = get(row, 2)?;
        let purpose: String = get(row, 3)?;
        let status: String = get(row, 5)?;
        let created: String = get(row, 6)?;
        out.push(ForkRecord {
            env: env.parse::<EnvId>().map_err(|e| invalid("env id", e))?,
            base: key.map(|k| parse_key(&k)).transpose()?,
            run_id: run.parse::<RunId>().map_err(|e| invalid("run id", e))?,
            purpose: ForkPurpose::parse(&purpose).map_err(|e| invalid("purpose", e))?,
            dir: get(row, 4)?,
            status: ForkStatus::parse(&status)?,
            created_at: parse_ts(&created)?,
            resources: Vec::new(),
        });
    }
    drop(rows);
    let mut res = conn.prepare("SELECT kind, value FROM resources WHERE env_id = ?1 ORDER BY kind, value").map_err(unavailable)?;
    for fork in &mut out {
        let mut rows = res.query(params![fork.env.to_string()]).map_err(unavailable)?;
        while let Some(row) = rows.next().map_err(unavailable)? {
            fork.resources.push(EnvResource { kind: get(row, 0)?, value: get(row, 1)? });
        }
    }
    Ok(out)
}

fn select_ports(conn: &Connection) -> Result<Vec<PortLease>, RunnerStateError> {
    let mut stmt = conn.prepare("SELECT port, owner_kind, owner_id, name, at FROM ports ORDER BY port").map_err(unavailable)?;
    let mut rows = stmt.query([]).map_err(unavailable)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(unavailable)? {
        let port: i64 = get(row, 0)?;
        let kind: String = get(row, 1)?;
        let id: String = get(row, 2)?;
        let name: Option<String> = get(row, 3)?;
        let at: String = get(row, 4)?;
        out.push(PortLease {
            port: u16::try_from(port).map_err(|e| invalid("port", e))?,
            owner: PortOwner::from_parts(&kind, &id)?,
            name: name.map(|n| PortName::new(&n).map_err(|e| invalid("port name", e))).transpose()?,
            at: parse_ts(&at)?,
        });
    }
    Ok(out)
}

#[async_trait]
impl RunnerState for SqliteRunnerState {
    async fn begin_run(&self, run: ActiveRun) -> Result<(), RunnerStateError> {
        self.with(move |conn, _| {
            let n = conn
                .execute(
                    "INSERT OR IGNORE INTO runs (run_id, org_id, kind, attempt, phase, env_id, started_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        run.run_id.to_string(),
                        run.org_id.to_string(),
                        kind_str(run.kind),
                        i64::from(run.attempt),
                        run.phase.as_str(),
                        run.env.map(|e| e.to_string()),
                        ts(run.started_at)
                    ],
                )
                .map_err(unavailable)?;
            if n == 0 {
                return Err(RunnerStateError::Conflict(format!("run {} already active", run.run_id)));
            }
            Ok(())
        })
        .await
    }

    async fn set_phase(&self, run: RunId, phase: RunPhase) -> Result<(), RunnerStateError> {
        self.with(move |conn, _| {
            let n = conn
                .execute("UPDATE runs SET phase = ?2 WHERE run_id = ?1", params![run.to_string(), phase.as_str()])
                .map_err(unavailable)?;
            if n == 0 { Err(RunnerStateError::NotFound) } else { Ok(()) }
        })
        .await
    }

    async fn attach_env(&self, run: RunId, env: EnvId) -> Result<(), RunnerStateError> {
        self.with(move |conn, _| {
            let n = conn
                .execute("UPDATE runs SET env_id = ?2 WHERE run_id = ?1", params![run.to_string(), env.to_string()])
                .map_err(unavailable)?;
            if n == 0 { Err(RunnerStateError::NotFound) } else { Ok(()) }
        })
        .await
    }

    async fn end_run(&self, run: RunId) -> Result<(), RunnerStateError> {
        self.with(move |conn, _| {
            conn.execute("DELETE FROM runs WHERE run_id = ?1", params![run.to_string()]).map_err(unavailable)?;
            Ok(())
        })
        .await
    }

    async fn active_runs(&self) -> Result<Vec<ActiveRun>, RunnerStateError> {
        self.with(|conn, _| select_runs(conn)).await
    }

    async fn record_process(&self, p: ProcessRecord) -> Result<(), RunnerStateError> {
        self.with(move |conn, _| {
            conn.execute(
                "INSERT OR REPLACE INTO processes (pgid, pid, start_time, boot_id, run_id, env_id, role, cgroup) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    p.pgid,
                    p.pid,
                    p.start_time as i64,
                    p.boot_id,
                    p.run_id.map(|r| r.to_string()),
                    p.env.map(|e| e.to_string()),
                    p.role.to_string(),
                    p.cgroup
                ],
            )
            .map_err(unavailable)?;
            Ok(())
        })
        .await
    }

    async fn clear_process(&self, pgid: i32) -> Result<(), RunnerStateError> {
        self.with(move |conn, _| {
            conn.execute("DELETE FROM processes WHERE pgid = ?1", params![pgid]).map_err(unavailable)?;
            Ok(())
        })
        .await
    }

    async fn processes(&self) -> Result<Vec<ProcessRecord>, RunnerStateError> {
        self.with(|conn, _| select_processes(conn)).await
    }

    async fn claim_base(&self, key: &BaseKey, staging_dir: &str) -> Result<BaseClaim, RunnerStateError> {
        let key = *key;
        let staging = staging_dir.to_owned();
        self.with(move |conn, now| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(unavailable)?;
            let id = key.id();
            let claim = match select_base(&tx, &id)? {
                Some(rec) if rec.status == BaseStatus::Ready => BaseClaim::Ready(rec),
                Some(_) => BaseClaim::InProgress,
                None => {
                    let kind = if key.recipe_hash.is_some() { BaseKind::Prepared } else { BaseKind::SourceOnly };
                    tx.execute(
                        "INSERT INTO bases (base_id, key_json, kind, status, dir, bytes, fork_mode, template_db, created_at, last_used_at) \
                         VALUES (?1, ?2, ?3, 'preparing', ?4, 0, NULL, NULL, ?5, ?5)",
                        params![id.as_str(), key_json(&key)?, kind.as_str(), staging, ts(now)],
                    )
                    .map_err(unavailable)?;
                    BaseClaim::Owned
                }
            };
            tx.commit().map_err(unavailable)?;
            Ok(claim)
        })
        .await
    }

    async fn base_ready(&self, key: &BaseKey, meta: BaseMeta) -> Result<(), RunnerStateError> {
        let id = key.id();
        self.with(move |conn, now| {
            let n = conn
                .execute(
                    "UPDATE bases SET kind = ?2, status = 'ready', dir = ?3, bytes = ?4, fork_mode = ?5, template_db = ?6, last_used_at = ?7 \
                     WHERE base_id = ?1",
                    params![
                        id.as_str(),
                        meta.kind.as_str(),
                        meta.dir,
                        meta.bytes as i64,
                        meta.fork_mode.as_str(),
                        meta.template_db,
                        ts(now)
                    ],
                )
                .map_err(unavailable)?;
            if n == 0 { Err(RunnerStateError::NotFound) } else { Ok(()) }
        })
        .await
    }

    async fn base_abandon(&self, key: &BaseKey) -> Result<(), RunnerStateError> {
        let id = key.id();
        self.with(move |conn, _| {
            conn.execute("DELETE FROM bases WHERE base_id = ?1 AND status = 'preparing'", params![id.as_str()])
                .map_err(unavailable)?;
            Ok(())
        })
        .await
    }

    async fn base(&self, key: &BaseKey) -> Result<Option<BaseRecord>, RunnerStateError> {
        let id = key.id();
        self.with(move |conn, _| select_base(conn, &id)).await
    }

    async fn bases(&self) -> Result<Vec<BaseRecord>, RunnerStateError> {
        self.with(|conn, _| select_bases(conn, "")).await
    }

    async fn touch_base(&self, key: &BaseKey, at: OffsetDateTime) -> Result<(), RunnerStateError> {
        let id = key.id();
        self.with(move |conn, _| {
            let n = conn
                .execute("UPDATE bases SET last_used_at = ?2 WHERE base_id = ?1", params![id.as_str(), ts(at)])
                .map_err(unavailable)?;
            if n == 0 { Err(RunnerStateError::NotFound) } else { Ok(()) }
        })
        .await
    }

    async fn remove_base(&self, key: &BaseKey) -> Result<(), RunnerStateError> {
        let id = key.id();
        self.with(move |conn, _| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(unavailable)?;
            let live: i64 = tx
                .query_row("SELECT count(*) FROM forks WHERE base_id = ?1", params![id.as_str()], |r| r.get(0))
                .map_err(unavailable)?;
            if live > 0 {
                return Err(RunnerStateError::Conflict(format!("base {id} has {live} live forks")));
            }
            tx.execute("DELETE FROM bases WHERE base_id = ?1", params![id.as_str()]).map_err(unavailable)?;
            tx.commit().map_err(unavailable)?;
            Ok(())
        })
        .await
    }

    async fn begin_fork(&self, f: ForkRecord) -> Result<(), RunnerStateError> {
        self.with(move |conn, _| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(unavailable)?;
            let base_id = match &f.base {
                Some(key) => {
                    let id = key.id();
                    let exists: Option<String> = tx
                        .query_row("SELECT base_id FROM bases WHERE base_id = ?1", params![id.as_str()], |r| r.get(0))
                        .optional()
                        .map_err(unavailable)?;
                    if exists.is_none() {
                        return Err(RunnerStateError::NotFound);
                    }
                    Some(id.as_str().to_owned())
                }
                None => None,
            };
            let n = tx
                .execute(
                    "INSERT OR IGNORE INTO forks (env_id, base_id, run_id, purpose, dir, status, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        f.env.to_string(),
                        base_id,
                        f.run_id.to_string(),
                        f.purpose.as_str(),
                        f.dir,
                        f.status.as_str(),
                        ts(f.created_at)
                    ],
                )
                .map_err(unavailable)?;
            if n == 0 {
                return Err(RunnerStateError::Conflict(format!("fork {} exists", f.env)));
            }
            for r in &f.resources {
                tx.execute(
                    "INSERT OR IGNORE INTO resources (env_id, kind, value) VALUES (?1, ?2, ?3)",
                    params![f.env.to_string(), r.kind, r.value],
                )
                .map_err(unavailable)?;
            }
            tx.commit().map_err(unavailable)?;
            Ok(())
        })
        .await
    }

    async fn set_fork_status(&self, env: EnvId, s: ForkStatus) -> Result<(), RunnerStateError> {
        self.with(move |conn, _| {
            let n = conn
                .execute("UPDATE forks SET status = ?2 WHERE env_id = ?1", params![env.to_string(), s.as_str()])
                .map_err(unavailable)?;
            if n == 0 { Err(RunnerStateError::NotFound) } else { Ok(()) }
        })
        .await
    }

    async fn add_resource(&self, env: EnvId, r: EnvResource) -> Result<(), RunnerStateError> {
        self.with(move |conn, _| {
            let exists: Option<String> = conn
                .query_row("SELECT env_id FROM forks WHERE env_id = ?1", params![env.to_string()], |r| r.get(0))
                .optional()
                .map_err(unavailable)?;
            if exists.is_none() {
                return Err(RunnerStateError::NotFound);
            }
            conn.execute(
                "INSERT OR IGNORE INTO resources (env_id, kind, value) VALUES (?1, ?2, ?3)",
                params![env.to_string(), r.kind, r.value],
            )
            .map_err(unavailable)?;
            Ok(())
        })
        .await
    }

    async fn remove_resource(&self, env: EnvId, r: &EnvResource) -> Result<(), RunnerStateError> {
        let r = r.clone();
        self.with(move |conn, _| {
            conn.execute(
                "DELETE FROM resources WHERE env_id = ?1 AND kind = ?2 AND value = ?3",
                params![env.to_string(), r.kind, r.value],
            )
            .map_err(unavailable)?;
            Ok(())
        })
        .await
    }

    async fn forks(&self) -> Result<Vec<ForkRecord>, RunnerStateError> {
        self.with(|conn, _| select_forks(conn)).await
    }

    async fn remove_fork(&self, env: EnvId) -> Result<(), RunnerStateError> {
        self.with(move |conn, _| {
            conn.execute("DELETE FROM forks WHERE env_id = ?1", params![env.to_string()]).map_err(unavailable)?;
            Ok(())
        })
        .await
    }

    async fn lease_ports(&self, owner: &PortOwner, names: &[Option<PortName>], pool: PortRange) -> Result<Vec<PortLease>, RunnerStateError> {
        if pool.start > pool.end {
            return Err(RunnerStateError::Invalid(format!("empty port range {}..={}", pool.start, pool.end)));
        }
        let owner = owner.clone();
        let names = names.to_vec();
        self.with(move |conn, now| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(unavailable)?;
            tx.execute("DELETE FROM port_quarantine WHERE until <= ?1", params![ts(now)]).map_err(unavailable)?;
            let mut taken = std::collections::BTreeSet::new();
            {
                let mut stmt = tx
                    .prepare(
                        "SELECT port FROM ports WHERE port BETWEEN ?1 AND ?2 \
                         UNION SELECT port FROM port_quarantine WHERE port BETWEEN ?1 AND ?2",
                    )
                    .map_err(unavailable)?;
                let mut rows = stmt.query(params![i64::from(pool.start), i64::from(pool.end)]).map_err(unavailable)?;
                while let Some(row) = rows.next().map_err(unavailable)? {
                    let p: i64 = get(row, 0)?;
                    taken.insert(p as u16);
                }
            }
            let mut free = (pool.start..=pool.end).filter(|p| !taken.contains(p));
            let (kind, id) = owner.parts();
            let mut out = Vec::with_capacity(names.len());
            for name in names {
                let Some(port) = free.next() else {
                    return Err(RunnerStateError::Unavailable(format!("port pool {}..={} exhausted", pool.start, pool.end)));
                };
                tx.execute(
                    "INSERT INTO ports (port, owner_kind, owner_id, name, at) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![i64::from(port), kind, id, name.as_ref().map(|n| n.as_str().to_owned()), ts(now)],
                )
                .map_err(unavailable)?;
                out.push(PortLease { port, owner: owner.clone(), name, at: now });
            }
            tx.commit().map_err(unavailable)?;
            Ok(out)
        })
        .await
    }

    async fn quarantine_port(&self, port: u16, until: OffsetDateTime) -> Result<(), RunnerStateError> {
        self.with(move |conn, _| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate).map_err(unavailable)?;
            tx.execute("DELETE FROM ports WHERE port = ?1", params![i64::from(port)]).map_err(unavailable)?;
            tx.execute(
                "INSERT OR REPLACE INTO port_quarantine (port, until) VALUES (?1, ?2)",
                params![i64::from(port), ts(until)],
            )
            .map_err(unavailable)?;
            tx.commit().map_err(unavailable)?;
            Ok(())
        })
        .await
    }

    async fn release_ports(&self, owner: &PortOwner) -> Result<(), RunnerStateError> {
        let (kind, id) = owner.parts();
        self.with(move |conn, _| {
            conn.execute("DELETE FROM ports WHERE owner_kind = ?1 AND owner_id = ?2", params![kind, id]).map_err(unavailable)?;
            Ok(())
        })
        .await
    }

    async fn port_leases(&self) -> Result<Vec<PortLease>, RunnerStateError> {
        self.with(|conn, _| select_ports(conn)).await
    }

    async fn recovery_snapshot(&self) -> Result<RecoverySnapshot, RunnerStateError> {
        self.with(|conn, _| {
            Ok(RecoverySnapshot {
                runs: select_runs(conn)?,
                processes: select_processes(conn)?,
                forks: select_forks(conn)?,
                preparing_bases: select_bases(conn, "WHERE b.status = 'preparing'")?,
                ports: select_ports(conn)?,
            })
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use mage_core::ports::SystemClock;

    use super::*;
    use crate::testing::contract::runner_state as contract;

    #[tokio::test]
    async fn sqlite_runner_state_contract() {
        let dir = tempfile::tempdir().unwrap();
        let counter = std::sync::atomic::AtomicU32::new(0);
        contract::run(|| {
            let n = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let path = dir.path().join(format!("state-{n}.db"));
            async move { Arc::new(SqliteRunnerState::open(&path, Arc::new(SystemClock)).unwrap()) as Arc<dyn RunnerState> }
        })
        .await;
    }

    #[tokio::test]
    async fn sqlite_state_survives_a_crash() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.db");
        contract::recovery_after_restart(|| {
            let path = path.clone();
            async move { Arc::new(SqliteRunnerState::open(&path, Arc::new(SystemClock)).unwrap()) as Arc<dyn RunnerState> }
        })
        .await;
        let mode = std::fs::metadata(&path).map(|m| std::os::unix::fs::PermissionsExt::mode(&m.permissions())).unwrap();
        assert_eq!(mode & 0o777, 0o600);
    }
}
