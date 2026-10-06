//! Runner-local Postgres administration and `db_query` (executor.md "Database (runner-local
//! Postgres)").
//!
//! Every identifier is quoted and length-checked; role passwords are embedded as escaped literals
//! because `CREATE ROLE`/`ALTER ROLE` take no bind parameters. Statement logging is disabled on
//! every connection so those literals never reach `tracing`.

use std::fmt;
use std::time::Duration;

use base64::Engine as _;
use futures::TryStreamExt as _;
use rand::RngCore as _;
use sqlx::encode::IsNull;
use sqlx::error::BoxDynError;
use sqlx::postgres::types::Oid;
use sqlx::postgres::{
    PgArgumentBuffer, PgArguments, PgConnectOptions, PgConnection, PgPool, PgPoolOptions, PgTypeInfo, PgValueRef,
    Postgres,
};
use sqlx::{Arguments as _, Column as _, ConnectOptions as _, Connection as _, Either, Executor as _, Row as _};
use sqlx::{Statement as _, TypeInfo as _, ValueRef as _};
use tokio::sync::Mutex;

use crate::ports::executor::{BaseId, DbColumn, DbQuery, DbRows, DbValue, EnvId, ExecError};

/// Postgres identifier limit (NAMEDATALEN - 1).
const NAME_MAX: usize = 63;
/// `PgStrategy::Auto` switches to `FILE_COPY` at this template size.
const AUTO_FILE_COPY_BYTES: u64 = 1 << 30;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const POOL_MAX_CONNECTIONS: u32 = 4;
const PASSWORD_BYTES: usize = 24;

/// SQLSTATE `object_in_use`: the template still has sessions.
const SQLSTATE_OBJECT_IN_USE: &str = "55006";
/// SQLSTATE `invalid_catalog_name`: the database does not exist.
const SQLSTATE_NO_DATABASE: &str = "3D000";

/// `CREATE DATABASE ... STRATEGY` for forks (config `postgres.strategy`; default `auto`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PgStrategy {
    /// `WAL_LOG` below 1 GiB, `FILE_COPY` above.
    #[default]
    Auto,
    WalLog,
    FileCopy,
}

/// Login credentials for one Mage role and database.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct DbCreds {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
    pub database: String,
}

impl DbCreds {
    /// `postgres://user:password@host:port/database`, with user, password and database
    /// percent-encoded.
    pub(crate) fn url(&self) -> String {
        let host = if self.host.contains(':') && !self.host.starts_with('[') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        format!(
            "postgres://{}:{}@{}:{}/{}",
            percent_encode(&self.user),
            percent_encode(&self.password),
            host,
            self.port,
            percent_encode(&self.database)
        )
    }
}

impl fmt::Debug for DbCreds {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DbCreds")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("user", &self.user)
            .field("password", &"<redacted>")
            .field("database", &self.database)
            .finish()
    }
}

/// Admin handle on the runner-local Postgres.
pub(crate) struct RunnerPg {
    pool: PgPool,
    admin: PgConnectOptions,
    host: String,
    port: u16,
    strategy: PgStrategy,
    /// Serialises the `REASSIGN OWNED` step of fork creation: it rewrites the template's shared
    /// `pg_database` row, which concurrent forks of the same base would otherwise race on.
    reassign: Mutex<()>,
}

impl fmt::Debug for RunnerPg {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunnerPg")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("strategy", &self.strategy)
            .finish_non_exhaustive()
    }
}

impl RunnerPg {
    /// Lazy pool (no connection until first use); `password` overrides the URL password.
    /// Invalid URL → `ExecError::Invalid`. Must be called inside a tokio runtime.
    pub(crate) fn connect_lazy(url: &str, password: Option<&str>, strategy: PgStrategy) -> Result<Self, ExecError> {
        // The parse error is not echoed: it may quote the URL, password included.
        let mut admin: PgConnectOptions =
            url.parse().map_err(|_| ExecError::Invalid("postgres: invalid admin URL".into()))?;
        if let Some(password) = password {
            admin = admin.password(password);
        }
        let admin = admin.disable_statement_logging();
        let pool = PgPoolOptions::new()
            .max_connections(POOL_MAX_CONNECTIONS)
            .acquire_timeout(CONNECT_TIMEOUT)
            .connect_lazy_with(admin.clone());
        Ok(Self {
            host: admin.get_host().to_owned(),
            port: admin.get_port(),
            pool,
            admin,
            strategy,
            reassign: Mutex::new(()),
        })
    }

    /// Server major version (`server_version_num / 10000`).
    pub(crate) async fn server_major(&self) -> Result<u32, ExecError> {
        let num: i32 = sqlx::query_scalar("SELECT current_setting('server_version_num')::int4")
            .fetch_one(&self.pool)
            .await
            .map_err(|e| pg_err("server version", e))?;
        u32::try_from(num / 10_000)
            .map_err(|_| ExecError::Unavailable(format!("postgres: unexpected server_version_num {num}")))
    }

    pub(crate) fn base_db_name(base: &BaseId) -> String {
        format!("mb_{base}")
    }

    pub(crate) fn fork_db_name(env: &EnvId) -> String {
        format!("mf_{}", env.slug())
    }

    /// Creates role and database `mb_<id>` (owned by the role, `CONNECT` revoked from `PUBLIC`)
    /// and the extensions inside it as admin. Returns the base role's credentials.
    pub(crate) async fn create_base(&self, base: &BaseId, extensions: &[String]) -> Result<DbCreds, ExecError> {
        let name = Self::base_db_name(base);
        let ident = quote_ident(&name)?;
        let password = random_password();
        self.admin_exec(
            "create base role",
            &format!("CREATE ROLE {ident} LOGIN PASSWORD {}", quote_literal(&password)?),
        )
        .await?;
        self.admin_exec("create base database", &format!("CREATE DATABASE {ident} OWNER {ident}")).await?;
        self.admin_exec("revoke base connect", &format!("REVOKE CONNECT ON DATABASE {ident} FROM PUBLIC")).await?;
        self.create_extensions(&name, extensions).await?;
        Ok(self.creds(name.clone(), password, name))
    }

    /// Makes `mb_<id>` a template nobody can connect to, then terminates leftover sessions.
    pub(crate) async fn freeze_base(&self, base: &BaseId) -> Result<(), ExecError> {
        let name = Self::base_db_name(base);
        let ident = quote_ident(&name)?;
        // Connections are disallowed first so no new session slips in after the terminate.
        self.admin_exec(
            "freeze base",
            &format!("ALTER DATABASE {ident} WITH ALLOW_CONNECTIONS false IS_TEMPLATE true"),
        )
        .await?;
        self.terminate_backends(&name).await
    }

    /// Fork create (executor.md table). `template = None` creates an empty database with
    /// `extensions`; with a template the extensions come from the base. `password`: `Some` keeps
    /// (or sets) that password, `None` generates a fresh one.
    pub(crate) async fn create_fork(
        &self,
        env: &EnvId,
        template: Option<&BaseId>,
        extensions: &[String],
        password: Option<&str>,
    ) -> Result<DbCreds, ExecError> {
        let name = Self::fork_db_name(env);
        let ident = quote_ident(&name)?;
        let password = password.map_or_else(random_password, str::to_owned);
        let password_lit = quote_literal(&password)?;
        if self.role_exists(&name).await? {
            self.admin_exec("set fork role password", &format!("ALTER ROLE {ident} WITH LOGIN PASSWORD {password_lit}"))
                .await?;
        } else {
            self.admin_exec("create fork role", &format!("CREATE ROLE {ident} LOGIN PASSWORD {password_lit}")).await?;
        }

        match template {
            Some(base) => {
                let base_name = Self::base_db_name(base);
                let base_ident = quote_ident(&base_name)?;
                let strategy = match self.strategy {
                    PgStrategy::WalLog => "WAL_LOG",
                    PgStrategy::FileCopy => "FILE_COPY",
                    PgStrategy::Auto if self.database_size(&base_name).await? < AUTO_FILE_COPY_BYTES => "WAL_LOG",
                    PgStrategy::Auto => "FILE_COPY",
                };
                let create =
                    format!("CREATE DATABASE {ident} TEMPLATE {base_ident} STRATEGY {strategy} OWNER {ident}");
                match sqlx::raw_sql(&create).execute(&self.pool).await {
                    Ok(_) => {}
                    Err(e) if sqlstate(&e).as_deref() == Some(SQLSTATE_OBJECT_IN_USE) => {
                        self.terminate_backends(&base_name).await?;
                        self.admin_exec("create fork database", &create).await?;
                    }
                    Err(e) => return Err(pg_err("create fork database", e)),
                }
                self.grant_fork_connect(&ident).await?;
                // REASSIGN OWNED also moves shared objects, i.e. the template database itself;
                // its owner is put back in the same transaction.
                let _guard = self.reassign.lock().await;
                let mut conn = self.admin_connect(&name).await?;
                let reassign = format!(
                    "BEGIN; REASSIGN OWNED BY {base_ident} TO {ident}; \
                     ALTER DATABASE {base_ident} OWNER TO {base_ident}; COMMIT"
                );
                let result = exec_raw(&mut conn, &reassign).await.map_err(|e| pg_err("reassign fork ownership", e));
                close(conn).await;
                result?;
            }
            None => {
                self.admin_exec("create fork database", &format!("CREATE DATABASE {ident} OWNER {ident}")).await?;
                self.grant_fork_connect(&ident).await?;
                self.create_extensions(&name, extensions).await?;
            }
        }
        Ok(self.creds(name.clone(), password, name))
    }

    /// `DROP DATABASE IF EXISTS mf_<env> WITH (FORCE)`; the role is kept (reset_db).
    pub(crate) async fn drop_fork_db(&self, env: &EnvId) -> Result<(), ExecError> {
        self.drop_database(&Self::fork_db_name(env)).await
    }

    /// Drops the fork database and role. Idempotent. (Test cleanup; `HostEnv` drops the
    /// recorded `pg_db` / `pg_role` resources by name.)
    #[cfg(test)]
    pub(crate) async fn drop_fork(&self, env: &EnvId) -> Result<(), ExecError> {
        let name = Self::fork_db_name(env);
        self.drop_database(&name).await?;
        self.drop_role(&name).await
    }

    /// Clears `IS_TEMPLATE`, drops the base database and role. Idempotent.
    pub(crate) async fn drop_base(&self, base: &BaseId) -> Result<(), ExecError> {
        let name = Self::base_db_name(base);
        self.drop_template_database(&name).await?;
        self.drop_role(&name).await
    }

    /// Databases named `mf_*` or `mb_*`, sorted.
    pub(crate) async fn mage_databases(&self) -> Result<Vec<String>, ExecError> {
        sqlx::query_scalar(
            r"SELECT datname::text FROM pg_database WHERE datname LIKE 'mf\_%' OR datname LIKE 'mb\_%' ORDER BY 1",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| pg_err("list databases", e))
    }

    /// Roles named `mf_*` or `mb_*`, sorted.
    pub(crate) async fn mage_roles(&self) -> Result<Vec<String>, ExecError> {
        sqlx::query_scalar(
            r"SELECT rolname::text FROM pg_roles WHERE rolname LIKE 'mf\_%' OR rolname LIKE 'mb\_%' ORDER BY 1",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|e| pg_err("list roles", e))
    }

    /// Drops a Mage database by name (orphan scan), template or not. Names outside
    /// `^m[bf]_[a-z0-9]+$` → `Invalid`.
    pub(crate) async fn drop_database_named(&self, name: &str) -> Result<(), ExecError> {
        check_mage_name(name)?;
        self.drop_template_database(name).await
    }

    /// Drops a Mage role by name (orphan scan). Names outside `^m[bf]_[a-z0-9]+$` → `Invalid`.
    pub(crate) async fn drop_role_named(&self, name: &str) -> Result<(), ExecError> {
        check_mage_name(name)?;
        self.drop_role(name).await
    }

    /// `pg_database_size` in bytes. Unknown database → `NotFound`.
    pub(crate) async fn database_size(&self, name: &str) -> Result<u64, ExecError> {
        let size: i64 = sqlx::query_scalar("SELECT pg_database_size($1::text::name)")
            .bind(name)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| match sqlstate(&e).as_deref() {
                Some(SQLSTATE_NO_DATABASE) => ExecError::NotFound(format!("database {name}")),
                _ => pg_err("database size", e),
            })?;
        Ok(u64::try_from(size).unwrap_or(0))
    }

    /// `db_query`: one statement as `creds` on a dedicated connection, inside a transaction that
    /// is read-only unless `q.write`, with `SET LOCAL statement_timeout`. Rows past `q.max_rows`
    /// set `truncated`; read-only queries stop reading there, writes drain to commit. SQL errors →
    /// `Invalid(<server message>)`; connection failures → `Unavailable`.
    pub(crate) async fn query(&self, creds: &DbCreds, q: &DbQuery) -> Result<DbRows, ExecError> {
        let options = self
            .admin
            .clone()
            .host(&creds.host)
            .port(creds.port)
            .username(&creds.user)
            .password(&creds.password)
            .database(&creds.database);
        let mut conn = match tokio::time::timeout(CONNECT_TIMEOUT, PgConnection::connect_with(&options)).await {
            Ok(Ok(conn)) => conn,
            Ok(Err(e)) => return Err(ExecError::Unavailable(format!("postgres: connect: {}", error_text(&e)))),
            Err(_) => return Err(ExecError::Unavailable("postgres: connect timed out".into())),
        };
        let result = run_query_boxed(&mut conn, q).await;
        match &result {
            // A truncated read-only result left rows unread: drop the socket instead of draining.
            Ok(rows) if rows.truncated && !q.write => {
                let _ = conn.close_hard().await;
            }
            _ => close(conn).await,
        }
        result
    }

    // -----------------------------------------------------------------------------------------

    fn creds(&self, user: String, password: String, database: String) -> DbCreds {
        DbCreds { host: self.host.clone(), port: self.port, user, password, database }
    }

    async fn admin_exec(&self, ctx: &str, sql: &str) -> Result<(), ExecError> {
        sqlx::raw_sql(sql).execute(&self.pool).await.map(drop).map_err(|e| pg_err(ctx, e))
    }

    /// A dedicated admin connection to `database` (the pool only reaches the admin database).
    async fn admin_connect(&self, database: &str) -> Result<PgConnection, ExecError> {
        let options = self.admin.clone().database(database);
        match tokio::time::timeout(CONNECT_TIMEOUT, PgConnection::connect_with(&options)).await {
            Ok(Ok(conn)) => Ok(conn),
            Ok(Err(e)) => Err(ExecError::Unavailable(format!("postgres: connect to {database}: {}", error_text(&e)))),
            Err(_) => Err(ExecError::Unavailable(format!("postgres: connect to {database} timed out"))),
        }
    }

    async fn create_extensions(&self, database: &str, extensions: &[String]) -> Result<(), ExecError> {
        if extensions.is_empty() {
            return Ok(());
        }
        let statements = extensions
            .iter()
            .map(|ext| quote_ident(ext).map(|ident| format!("CREATE EXTENSION IF NOT EXISTS {ident}")))
            .collect::<Result<Vec<_>, _>>()?;
        let mut conn = self.admin_connect(database).await?;
        let mut result = Ok(());
        for (ext, sql) in extensions.iter().zip(&statements) {
            if let Err(e) = exec_raw(&mut conn, sql).await {
                result = Err(pg_err(&format!("create extension {ext:?}"), e));
                break;
            }
        }
        close(conn).await;
        result
    }

    async fn grant_fork_connect(&self, ident: &str) -> Result<(), ExecError> {
        self.admin_exec(
            "grant fork connect",
            &format!("REVOKE CONNECT ON DATABASE {ident} FROM PUBLIC; GRANT CONNECT ON DATABASE {ident} TO {ident}"),
        )
        .await
    }

    async fn role_exists(&self, name: &str) -> Result<bool, ExecError> {
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = $1)")
            .bind(name)
            .fetch_one(&self.pool)
            .await
            .map_err(|e| pg_err("look up role", e))
    }

    async fn terminate_backends(&self, database: &str) -> Result<(), ExecError> {
        sqlx::query("SELECT pg_terminate_backend(pid) FROM pg_stat_activity WHERE datname = $1 AND pid <> pg_backend_pid()")
            .bind(database)
            .execute(&self.pool)
            .await
            .map(drop)
            .map_err(|e| pg_err("terminate backends", e))
    }

    async fn drop_database(&self, name: &str) -> Result<(), ExecError> {
        let ident = quote_ident(name)?;
        self.admin_exec("drop database", &format!("DROP DATABASE IF EXISTS {ident} WITH (FORCE)")).await
    }

    /// Like `drop_database`, but clears `IS_TEMPLATE` first (Postgres refuses to drop templates).
    async fn drop_template_database(&self, name: &str) -> Result<(), ExecError> {
        let ident = quote_ident(name)?;
        match sqlx::raw_sql(&format!("ALTER DATABASE {ident} WITH IS_TEMPLATE false")).execute(&self.pool).await {
            Ok(_) => {}
            Err(e) if sqlstate(&e).as_deref() == Some(SQLSTATE_NO_DATABASE) => {}
            Err(e) => return Err(pg_err("clear template flag", e)),
        }
        self.drop_database(name).await
    }

    async fn drop_role(&self, name: &str) -> Result<(), ExecError> {
        let ident = quote_ident(name)?;
        self.admin_exec("drop role", &format!("DROP ROLE IF EXISTS {ident}")).await
    }
}

// ---------------------------------------------------------------------------------------------
// db_query
// ---------------------------------------------------------------------------------------------

// Futures borrowing a `&mut PgConnection` are boxed as `Send` at a concrete lifetime: awaited
// directly inside an `#[async_trait]` method, the compiler's higher-ranked `Send` check fails
// ("implementation of `sqlx::Executor` is not general enough", rust-lang/rust#102211).
type SendFuture<'a, T> = std::pin::Pin<Box<dyn Future<Output = T> + Send + 'a>>;

fn exec_raw<'a>(conn: &'a mut PgConnection, sql: &'a str) -> SendFuture<'a, Result<sqlx::postgres::PgQueryResult, sqlx::Error>> {
    conn.execute(sqlx::raw_sql(sql))
}

fn prepare_on<'a>(
    conn: &'a mut PgConnection,
    sql: &'a str,
    types: &'a [PgTypeInfo],
) -> SendFuture<'a, Result<sqlx::postgres::PgStatement<'a>, sqlx::Error>> {
    Box::pin(conn.prepare_with(sql, types))
}

type RowStream<'a> = std::pin::Pin<
    Box<dyn futures::Stream<Item = Result<Either<sqlx::postgres::PgQueryResult, sqlx::postgres::PgRow>, sqlx::Error>> + Send + 'a>,
>;

fn fetch_many_on<'a>(conn: &'a mut PgConnection, sql: &'a str, args: PgArguments) -> RowStream<'a> {
    Box::pin(conn.fetch_many(sqlx::query_with(sql, args)))
}

fn run_query_boxed<'a>(conn: &'a mut PgConnection, q: &'a DbQuery) -> SendFuture<'a, Result<DbRows, ExecError>> {
    Box::pin(run_query(conn, q))
}

async fn run_query(conn: &mut PgConnection, q: &DbQuery) -> Result<DbRows, ExecError> {
    let timeout_ms = q.timeout.as_millis().clamp(1, i32::MAX as u128);
    let setup = if q.write {
        format!("BEGIN; SET LOCAL statement_timeout = {timeout_ms}")
    } else {
        format!("BEGIN; SET TRANSACTION READ ONLY; SET LOCAL statement_timeout = {timeout_ms}")
    };
    exec_raw(conn, &setup).await.map_err(sql_err)?;

    let mut types = Vec::with_capacity(q.params.len());
    let mut args = PgArguments::default();
    for param in &q.params {
        let added = match param {
            DbValue::Null => {
                types.push(<UntypedNull as sqlx::Type<Postgres>>::type_info());
                args.add(UntypedNull)
            }
            DbValue::Bool(b) => {
                types.push(<bool as sqlx::Type<Postgres>>::type_info());
                args.add(*b)
            }
            DbValue::Int(i) => {
                types.push(<i64 as sqlx::Type<Postgres>>::type_info());
                args.add(*i)
            }
            DbValue::Float(f) => {
                types.push(<f64 as sqlx::Type<Postgres>>::type_info());
                args.add(*f)
            }
            DbValue::Text(s) => {
                types.push(<&str as sqlx::Type<Postgres>>::type_info());
                args.add(s.as_str())
            }
        };
        added.map_err(|e| ExecError::Invalid(format!("bind parameter: {e}")))?;
    }

    // Preparing first yields the column list even when no row comes back.
    let statement = prepare_on(conn, &q.sql, &types).await.map_err(sql_err)?;
    let (columns, kinds): (Vec<DbColumn>, Vec<ColumnKind>) = statement
        .columns()
        .iter()
        .map(|column| {
            let (type_name, kind) = classify(column.type_info());
            (DbColumn { name: column.name().to_owned(), type_name }, kind)
        })
        .unzip();

    let max_rows = usize::try_from(q.max_rows).unwrap_or(usize::MAX);
    let mut rows = Vec::new();
    let mut seen: u64 = 0;
    let mut affected: Option<u64> = None;
    let mut truncated = false;
    {
        let mut stream = fetch_many_on(conn, &q.sql, args);
        while let Some(item) = stream.try_next().await.map_err(sql_err)? {
            match item {
                Either::Left(done) => affected = Some(affected.unwrap_or(0) + done.rows_affected()),
                Either::Right(row) => {
                    seen += 1;
                    if rows.len() < max_rows {
                        rows.push((0..kinds.len()).map(|i| decode_cell(&row, i, kinds[i])).collect());
                    } else {
                        truncated = true;
                        if !q.write {
                            break;
                        }
                    }
                }
            }
        }
    }
    if truncated && !q.write {
        // The caller drops the connection; the transaction dies with it.
        return Ok(DbRows { columns, rows, row_count: seen, truncated });
    }
    let end = if q.write { "COMMIT" } else { "ROLLBACK" };
    exec_raw(conn, end).await.map_err(sql_err)?;
    Ok(DbRows { columns, rows, row_count: affected.unwrap_or(seen), truncated })
}

/// A NULL parameter with an unspecified type, so the server infers it from context.
struct UntypedNull;

impl sqlx::Type<Postgres> for UntypedNull {
    fn type_info() -> PgTypeInfo {
        PgTypeInfo::with_oid(Oid(0))
    }
}

impl sqlx::Encode<'_, Postgres> for UntypedNull {
    fn encode_by_ref(&self, _buf: &mut PgArgumentBuffer) -> Result<IsNull, BoxDynError> {
        Ok(IsNull::Yes)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ColumnKind {
    Bool,
    Int2,
    Int4,
    Int8,
    Float4,
    Float8,
    Text,
    Uuid,
    Json,
    Jsonb,
    Timestamptz,
    Timestamp,
    Date,
    Time,
    Bytea,
    Numeric,
    Unsupported,
}

/// Lowercase Postgres type name and decoding strategy for a result column.
fn classify(info: &PgTypeInfo) -> (String, ColumnKind) {
    use ColumnKind as K;
    let known = match info.oid().map(|oid| oid.0) {
        Some(16) => Some(("bool", K::Bool)),
        Some(17) => Some(("bytea", K::Bytea)),
        Some(19) => Some(("name", K::Text)),
        Some(20) => Some(("int8", K::Int8)),
        Some(21) => Some(("int2", K::Int2)),
        Some(23) => Some(("int4", K::Int4)),
        Some(25) => Some(("text", K::Text)),
        Some(114) => Some(("json", K::Json)),
        Some(700) => Some(("float4", K::Float4)),
        Some(701) => Some(("float8", K::Float8)),
        Some(705) => Some(("unknown", K::Text)),
        Some(1042) => Some(("bpchar", K::Text)),
        Some(1043) => Some(("varchar", K::Text)),
        Some(1082) => Some(("date", K::Date)),
        Some(1083) => Some(("time", K::Time)),
        Some(1114) => Some(("timestamp", K::Timestamp)),
        Some(1184) => Some(("timestamptz", K::Timestamptz)),
        Some(1700) => Some(("numeric", K::Numeric)),
        Some(2950) => Some(("uuid", K::Uuid)),
        Some(3802) => Some(("jsonb", K::Jsonb)),
        _ => None,
    };
    match known {
        Some((name, kind)) => (name.to_owned(), kind),
        None => {
            let name = info.name().to_ascii_lowercase();
            // citext is an extension type: its OID differs per database.
            let kind = if name == "citext" { K::Text } else { K::Unsupported };
            (name, kind)
        }
    }
}

fn decode_cell(row: &sqlx::postgres::PgRow, index: usize, kind: ColumnKind) -> DbValue {
    let Ok(value) = row.try_get_raw(index) else {
        return DbValue::Text(format!("<{}>", kind_label(row, index)));
    };
    if value.is_null() {
        return DbValue::Null;
    }
    decode_value(value, kind).unwrap_or_else(|| match kind {
        ColumnKind::Unsupported => DbValue::Text(format!("<unsupported type {}>", kind_label(row, index))),
        _ => DbValue::Text(format!("<{}>", kind_label(row, index))),
    })
}

fn kind_label(row: &sqlx::postgres::PgRow, index: usize) -> String {
    row.columns().get(index).map_or_else(|| "?".to_owned(), |c| classify(c.type_info()).0)
}

fn decode_value(value: PgValueRef<'_>, kind: ColumnKind) -> Option<DbValue> {
    use time::format_description::well_known::Rfc3339;
    use time::macros::format_description;

    fn dec<'r, T: sqlx::Decode<'r, Postgres>>(value: PgValueRef<'r>) -> Option<T> {
        T::decode(value).ok()
    }

    let text = |s: String| Some(DbValue::Text(s));
    match kind {
        ColumnKind::Bool => dec::<bool>(value).map(DbValue::Bool),
        ColumnKind::Int2 => dec::<i16>(value).map(|n| DbValue::Int(n.into())),
        ColumnKind::Int4 => dec::<i32>(value).map(|n| DbValue::Int(n.into())),
        ColumnKind::Int8 => dec::<i64>(value).map(DbValue::Int),
        ColumnKind::Float4 => dec::<f32>(value).map(|n| DbValue::Float(n.into())),
        ColumnKind::Float8 => dec::<f64>(value).map(DbValue::Float),
        ColumnKind::Text | ColumnKind::Json => value.as_str().ok().and_then(|s| text(s.to_owned())),
        ColumnKind::Jsonb => {
            let bytes = value.as_bytes().ok()?;
            let body = match value.format() {
                sqlx::postgres::PgValueFormat::Binary => bytes.strip_prefix(&[1u8])?,
                sqlx::postgres::PgValueFormat::Text => bytes,
            };
            std::str::from_utf8(body).ok().and_then(|s| text(s.to_owned()))
        }
        ColumnKind::Uuid => dec::<uuid::Uuid>(value).and_then(|u| text(u.to_string())),
        ColumnKind::Timestamptz => {
            dec::<time::OffsetDateTime>(value).and_then(|t| t.format(&Rfc3339).ok()).and_then(text)
        }
        ColumnKind::Timestamp => dec::<time::PrimitiveDateTime>(value)
            .and_then(|t| t.format(format_description!("[year]-[month]-[day]T[hour]:[minute]:[second].[subsecond digits:6]")).ok())
            .and_then(text),
        ColumnKind::Date => {
            dec::<time::Date>(value).and_then(|d| d.format(format_description!("[year]-[month]-[day]")).ok()).and_then(text)
        }
        ColumnKind::Time => dec::<time::Time>(value)
            .and_then(|t| t.format(format_description!("[hour]:[minute]:[second].[subsecond digits:6]")).ok())
            .and_then(text),
        ColumnKind::Bytea => dec::<Vec<u8>>(value).and_then(|b| text(format!("\\x{}", hex::encode(b)))),
        ColumnKind::Numeric => match value.format() {
            sqlx::postgres::PgValueFormat::Binary => numeric_text(value.as_bytes().ok()?).and_then(text),
            sqlx::postgres::PgValueFormat::Text => value.as_str().ok().and_then(|s| text(s.to_owned())),
        },
        ColumnKind::Unsupported => None,
    }
}

/// Renders Postgres' binary `numeric` (base-10000 digits) as its decimal text form.
fn numeric_text(bytes: &[u8]) -> Option<String> {
    let header = |i: usize| bytes.get(i..i + 2).map(|b| u16::from_be_bytes([b[0], b[1]]));
    let ndigits = usize::from(header(0)?);
    let weight = i32::from(header(2)? as i16);
    let sign = header(4)?;
    let dscale = usize::from(header(6)?);
    match sign {
        0xC000 => return Some("NaN".into()),
        0xD000 => return Some("Infinity".into()),
        0xF000 => return Some("-Infinity".into()),
        0x0000 | 0x4000 => {}
        _ => return None,
    }
    if bytes.len() != 8 + 2 * ndigits {
        return None;
    }
    let digits = (0..ndigits).map(|i| header(8 + 2 * i).filter(|d| *d < 10_000)).collect::<Option<Vec<u16>>>()?;
    // Digit `i` is worth 10000^(weight - i).
    let digit_at = |index: i32| usize::try_from(index).ok().and_then(|i| digits.get(i).copied()).unwrap_or(0);

    let mut out = String::new();
    if sign == 0x4000 {
        out.push('-');
    }
    if weight < 0 {
        out.push('0');
    } else {
        out.push_str(&digit_at(0).to_string());
        for index in 1..=weight {
            out.push_str(&format!("{:04}", digit_at(index)));
        }
    }
    if dscale > 0 {
        let mut frac = String::with_capacity(dscale + 4);
        let mut index = weight + 1;
        while frac.len() < dscale {
            frac.push_str(&format!("{:04}", digit_at(index)));
            index += 1;
        }
        frac.truncate(dscale);
        out.push('.');
        out.push_str(&frac);
    }
    Some(out)
}

// ---------------------------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------------------------

/// `"name"` with embedded quotes doubled; rejects empty, NUL-containing and over-long names.
fn quote_ident(name: &str) -> Result<String, ExecError> {
    if name.is_empty() || name.len() > NAME_MAX || name.contains('\0') {
        return Err(ExecError::Invalid(format!("postgres identifier {name:?} must be 1..={NAME_MAX} bytes without NUL")));
    }
    Ok(format!("\"{}\"", name.replace('"', "\"\"")))
}

/// `E'...'` literal with quotes and backslashes escaped, valid whatever
/// `standard_conforming_strings` is. The value is never echoed in errors.
fn quote_literal(value: &str) -> Result<String, ExecError> {
    if value.contains('\0') {
        return Err(ExecError::Invalid("postgres literal contains NUL".into()));
    }
    Ok(format!("E'{}'", value.replace('\\', "\\\\").replace('\'', "''")))
}

/// `^m[bf]_[a-z0-9]+$`, at most 63 bytes.
fn check_mage_name(name: &str) -> Result<(), ExecError> {
    let ok = name.len() <= NAME_MAX
        && (name.starts_with("mb_") || name.starts_with("mf_"))
        && name.len() > 3
        && name.bytes().skip(3).all(|b| b.is_ascii_lowercase() || b.is_ascii_digit());
    if ok { Ok(()) } else { Err(ExecError::Invalid(format!("not a mage database or role name: {name:?}"))) }
}

/// 24 random bytes, base64url without padding (32 characters).
fn random_password() -> String {
    let mut bytes = [0u8; PASSWORD_BYTES];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Percent-encodes everything but RFC 3986 unreserved characters.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

async fn close(conn: PgConnection) {
    if let Err(e) = conn.close().await {
        tracing::debug!(error = %error_text(&e), "postgres: closing connection failed");
    }
}

fn sqlstate(e: &sqlx::Error) -> Option<String> {
    match e {
        sqlx::Error::Database(db) => db.code().map(|c| c.into_owned()),
        _ => None,
    }
}

/// Server-side failures that mean "Postgres is not usable right now" rather than "bad SQL":
/// connection exceptions, insufficient resources, shutdowns, system and internal errors.
fn is_unavailable_state(code: &str) -> bool {
    ["08", "53", "58", "XX"].iter().any(|class| code.starts_with(class)) || code.starts_with("57P")
}

/// Error text that never carries credentials (sqlx errors don't quote connect options).
fn error_text(e: &sqlx::Error) -> String {
    match e {
        sqlx::Error::Database(db) => db.message().to_owned(),
        other => other.to_string(),
    }
}

/// Admin-operation errors: SQL errors → `Invalid`, everything else → `Unavailable`.
fn pg_err(ctx: &str, e: sqlx::Error) -> ExecError {
    match sqlstate(&e) {
        Some(code) if !is_unavailable_state(&code) => ExecError::Invalid(format!("postgres: {ctx}: {}", error_text(&e))),
        _ => ExecError::Unavailable(format!("postgres: {ctx}: {}", error_text(&e))),
    }
}

/// `db_query` errors: the bare server message for SQL errors.
fn sql_err(e: sqlx::Error) -> ExecError {
    match sqlstate(&e) {
        Some(code) if !is_unavailable_state(&code) => ExecError::Invalid(error_text(&e)),
        _ => ExecError::Unavailable(format!("postgres: {}", error_text(&e))),
    }
}

#[cfg(test)]
mod tests {
    use std::panic::AssertUnwindSafe;
    use std::time::Instant;

    use futures::FutureExt as _;

    use super::*;

    fn env_id(n: u128) -> EnvId {
        EnvId::from_uuid(uuid::Uuid::from_u128(n))
    }

    #[test]
    fn names_follow_the_mage_scheme() {
        let base = BaseId::parse("0123456789abcdef0123").unwrap();
        assert_eq!(RunnerPg::base_db_name(&base), "mb_0123456789abcdef0123");
        let env = env_id(0x0190_0000_0000_7000_8000_0000_0000_0001);
        let fork = RunnerPg::fork_db_name(&env);
        assert_eq!(fork, format!("mf_{}", env.slug()));
        assert!(fork.len() <= NAME_MAX);
        assert!(check_mage_name(&fork).is_ok());
        assert!(check_mage_name(&RunnerPg::base_db_name(&base)).is_ok());
    }

    #[test]
    fn creds_debug_redacts_and_url_percent_encodes() {
        let creds = DbCreds {
            host: "127.0.0.1".into(),
            port: 5432,
            user: "mf_abc".into(),
            password: "p@ss:w/rd%?#'".into(),
            database: "mf_abc".into(),
        };
        let debug = format!("{creds:?}");
        assert!(!debug.contains("p@ss"), "{debug}");
        assert!(debug.contains("<redacted>") && debug.contains("mf_abc"));
        assert_eq!(creds.url(), "postgres://mf_abc:p%40ss%3Aw%2Frd%25%3F%23%27@127.0.0.1:5432/mf_abc");
        // sqlx parses it back to the same credentials.
        let parsed: PgConnectOptions = creds.url().parse().unwrap();
        assert_eq!(parsed.get_username(), "mf_abc");
        assert_eq!(parsed.get_database(), Some("mf_abc"));
        let v6 = DbCreds { host: "::1".into(), ..creds };
        assert!(v6.url().contains("@[::1]:5432/"));
    }

    #[test]
    fn quoting_escapes_and_bounds() {
        assert_eq!(quote_ident("a\"b").unwrap(), "\"a\"\"b\"");
        assert!(quote_ident(&"a".repeat(63)).is_ok());
        assert!(matches!(quote_ident(&"a".repeat(64)), Err(ExecError::Invalid(_))));
        assert!(matches!(quote_ident(""), Err(ExecError::Invalid(_))));
        assert_eq!(quote_literal("it's \\x").unwrap(), "E'it''s \\\\x'");
        let pw = random_password();
        assert_eq!(pw.len(), 32);
        assert!(pw.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'));
        assert_ne!(pw, random_password());
    }

    #[test]
    fn numeric_binary_decoding() {
        fn enc(weight: i16, sign: u16, dscale: u16, digits: &[u16]) -> Vec<u8> {
            let mut b = Vec::new();
            b.extend((digits.len() as u16).to_be_bytes());
            b.extend(weight.to_be_bytes());
            b.extend(sign.to_be_bytes());
            b.extend(dscale.to_be_bytes());
            for d in digits {
                b.extend(d.to_be_bytes());
            }
            b
        }
        assert_eq!(numeric_text(&enc(0, 0, 3, &[12, 3400])).as_deref(), Some("12.340"));
        assert_eq!(numeric_text(&enc(1, 0x4000, 0, &[1, 0])).as_deref(), Some("-10000"));
        assert_eq!(numeric_text(&enc(2, 0, 0, &[5])).as_deref(), Some("500000000"));
        // 12 × 10000^-2.
        assert_eq!(numeric_text(&enc(-2, 0, 8, &[12])).as_deref(), Some("0.00000012"));
        assert_eq!(numeric_text(&enc(-1, 0, 4, &[12])).as_deref(), Some("0.0012"));
        assert_eq!(numeric_text(&enc(0, 0, 0, &[])).as_deref(), Some("0"));
        assert_eq!(numeric_text(&enc(0, 0xC000, 0, &[])).as_deref(), Some("NaN"));
        assert_eq!(numeric_text(&enc(0, 0, 0, &[10_000])), None);
        assert_eq!(numeric_text(&[0, 1, 0]), None);
    }

    #[tokio::test]
    async fn rejects_bad_urls_and_non_mage_names_without_connecting() {
        assert!(matches!(
            RunnerPg::connect_lazy("not a url", None, PgStrategy::Auto),
            Err(ExecError::Invalid(m)) if !m.contains("not a url")
        ));
        // Port 1: any attempt to connect would fail with Unavailable, not Invalid.
        let pg = RunnerPg::connect_lazy("postgres://u:secret@127.0.0.1:1/x", Some("other"), PgStrategy::Auto).unwrap();
        assert!(!format!("{pg:?}").contains("secret"));
        for bad in ["postgres", "mb_", "mb_ABC", "mf_a;drop", "mx_abc", "mb_a-b", &format!("mb_{}", "a".repeat(61))] {
            assert!(matches!(pg.drop_database_named(bad).await, Err(ExecError::Invalid(_))), "{bad}");
            assert!(matches!(pg.drop_role_named(bad).await, Err(ExecError::Invalid(_))), "{bad}");
        }
        assert!(matches!(pg.drop_role_named("mb_abc123").await, Err(ExecError::Unavailable(_))));
    }

    // -----------------------------------------------------------------------------------------
    // Live tests against a disposable superuser Postgres ≥ 15.

    fn test_url() -> Option<String> {
        match std::env::var("MAGE_TEST_RUNNER_PG_URL") {
            Ok(url) if !url.is_empty() => Some(url),
            _ => {
                println!("skipped: MAGE_TEST_RUNNER_PG_URL not set");
                None
            }
        }
    }

    fn random_base() -> BaseId {
        let mut bytes = [0u8; 10];
        rand::rngs::OsRng.fill_bytes(&mut bytes);
        BaseId::parse(&hex::encode(bytes)).unwrap()
    }

    fn random_env() -> EnvId {
        EnvId::from_uuid(uuid::Uuid::new_v4())
    }

    fn query(sql: &str) -> DbQuery {
        DbQuery { sql: sql.into(), params: vec![], write: false, max_rows: 200, timeout: Duration::from_secs(10) }
    }

    fn write(sql: &str) -> DbQuery {
        DbQuery { write: true, ..query(sql) }
    }

    async fn single(pg: &RunnerPg, creds: &DbCreds, sql: &str) -> DbValue {
        let rows = pg.query(creds, &query(sql)).await.unwrap();
        rows.rows[0][0].clone()
    }

    /// Runs `body`, then drops the given bases and forks even if `body` panicked.
    async fn with_cleanup<F>(pg: &RunnerPg, bases: &[BaseId], envs: &[EnvId], body: F)
    where
        F: Future<Output = ()>,
    {
        let outcome = AssertUnwindSafe(body).catch_unwind().await;
        for env in envs {
            let _ = pg.drop_fork(env).await;
        }
        for base in bases {
            let _ = pg.drop_base(base).await;
        }
        if let Err(panic) = outcome {
            std::panic::resume_unwind(panic);
        }
    }

    #[tokio::test]
    async fn base_template_forks_are_isolated_and_reset_restores_template_state() {
        let Some(url) = test_url() else { return };
        let pg = RunnerPg::connect_lazy(&url, None, PgStrategy::Auto).unwrap();
        assert!(pg.server_major().await.unwrap() >= 15);
        let base = random_base();
        let (env_a, env_b) = (random_env(), random_env());
        let base_name = RunnerPg::base_db_name(&base);

        with_cleanup(&pg, std::slice::from_ref(&base), &[env_a, env_b], async {
            let base_creds = pg.create_base(&base, &["pgcrypto".to_owned()]).await.unwrap();
            assert_eq!(base_creds.user, base_name);
            assert_eq!(base_creds.database, base_name);
            // pgcrypto is usable by the base role; the table belongs to it.
            pg.query(
                &base_creds,
                &write("CREATE TABLE t (id int PRIMARY KEY, h text DEFAULT encode(digest('x', 'sha256'), 'hex'))"),
            )
            .await
            .unwrap();
            pg.query(&base_creds, &write("INSERT INTO t (id) VALUES (1)")).await.unwrap();
            pg.freeze_base(&base).await.unwrap();
            // Frozen: even the base role cannot connect any more.
            assert!(matches!(pg.query(&base_creds, &query("SELECT 1")).await, Err(ExecError::Unavailable(_))));

            let a = pg.create_fork(&env_a, Some(&base), &[], None).await.unwrap();
            let b = pg.create_fork(&env_b, Some(&base), &[], None).await.unwrap();
            assert_eq!(a.user, RunnerPg::fork_db_name(&env_a));
            assert_ne!(a.password, b.password);

            let inserted = pg.query(&a, &write("INSERT INTO t (id) VALUES (2)")).await.unwrap();
            assert_eq!(inserted.row_count, 1);
            assert_eq!(single(&pg, &a, "SELECT count(*) FROM t").await, DbValue::Int(2));
            assert_eq!(single(&pg, &b, "SELECT count(*) FROM t").await, DbValue::Int(1));
            // The fork role owns the copied table.
            pg.query(&a, &write("ALTER TABLE t ADD COLUMN extra int")).await.unwrap();
            // Fork roles cannot reach each other's databases.
            let cross = DbCreds { database: b.database.clone(), ..a.clone() };
            assert!(matches!(pg.query(&cross, &query("SELECT 1")).await, Err(ExecError::Unavailable(_))));
            // REASSIGN OWNED did not hand the template database to a fork role.
            let owner: String = sqlx::query_scalar(
                "SELECT pg_get_userbyid(datdba)::text FROM pg_database WHERE datname = $1",
            )
            .bind(&base_name)
            .fetch_one(&pg.pool)
            .await
            .unwrap();
            assert_eq!(owner, base_name);

            // reset_db: drop the database, re-fork with the kept password.
            pg.drop_fork_db(&env_a).await.unwrap();
            assert!(!pg.mage_databases().await.unwrap().contains(&a.database));
            assert!(pg.mage_roles().await.unwrap().contains(&a.user));
            let again = pg.create_fork(&env_a, Some(&base), &[], Some(&a.password)).await.unwrap();
            assert_eq!(again, a);
            assert_eq!(single(&pg, &a, "SELECT count(*) FROM t").await, DbValue::Int(1));

            pg.drop_fork(&env_a).await.unwrap();
            pg.drop_fork(&env_a).await.unwrap();
            assert!(!pg.mage_roles().await.unwrap().contains(&a.user));
            pg.drop_fork(&env_b).await.unwrap();
            pg.drop_base(&base).await.unwrap();
            pg.drop_base(&base).await.unwrap();
            assert!(!pg.mage_databases().await.unwrap().contains(&base_name));
            assert!(!pg.mage_roles().await.unwrap().contains(&base_name));
        })
        .await;
    }

    #[tokio::test]
    async fn file_copy_fork_and_busy_template_retry() {
        let Some(url) = test_url() else { return };
        let pg = RunnerPg::connect_lazy(&url, None, PgStrategy::FileCopy).unwrap();
        let base = random_base();
        let env = random_env();
        let base_name = RunnerPg::base_db_name(&base);

        with_cleanup(&pg, std::slice::from_ref(&base), &[env], async {
            let base_creds = pg.create_base(&base, &[]).await.unwrap();
            pg.query(&base_creds, &write("CREATE TABLE t AS SELECT 7 AS v")).await.unwrap();
            pg.freeze_base(&base).await.unwrap();
            // A stray admin session on the template makes CREATE DATABASE fail with 55006;
            // create_fork terminates it and retries.
            pg.admin_exec("thaw", &format!("ALTER DATABASE \"{base_name}\" WITH ALLOW_CONNECTIONS true"))
                .await
                .unwrap();
            let mut stray = pg.admin_connect(&base_name).await.unwrap();
            pg.admin_exec("refreeze", &format!("ALTER DATABASE \"{base_name}\" WITH ALLOW_CONNECTIONS false"))
                .await
                .unwrap();
            let fork = pg.create_fork(&env, Some(&base), &[], None).await.unwrap();
            assert_eq!(single(&pg, &fork, "SELECT v FROM t").await, DbValue::Int(7));
            assert!(sqlx::raw_sql("SELECT 1").execute(&mut stray).await.is_err());

            // Orphan-scan drops by name, template flag included.
            pg.drop_database_named(&fork.database).await.unwrap();
            pg.drop_role_named(&fork.user).await.unwrap();
            pg.drop_database_named(&base_name).await.unwrap();
            pg.drop_role_named(&base_name).await.unwrap();
            let dbs = pg.mage_databases().await.unwrap();
            assert!(!dbs.contains(&base_name) && !dbs.contains(&fork.database));
            assert!(matches!(pg.database_size(&base_name).await, Err(ExecError::NotFound(_))));
        })
        .await;
    }

    #[tokio::test]
    async fn db_query_enforces_transaction_mode_limits_and_maps_types() {
        let Some(url) = test_url() else { return };
        let pg = RunnerPg::connect_lazy(&url, None, PgStrategy::Auto).unwrap();
        let env = random_env();

        with_cleanup(&pg, &[], &[env], async {
            // Learn setup_db: empty database with extensions, no template.
            let c = pg.create_fork(&env, None, &["pgcrypto".to_owned()], None).await.unwrap();
            assert!(pg.mage_databases().await.unwrap().contains(&c.database));
            assert!(pg.database_size(&c.database).await.unwrap() > 0);
            assert_eq!(
                single(&pg, &c, "SELECT count(*) FROM pg_extension WHERE extname = 'pgcrypto'").await,
                DbValue::Int(1)
            );
            assert_eq!(
                single(&pg, &c, "SELECT count(*) FROM pg_tables WHERE schemaname = 'public'").await,
                DbValue::Int(0)
            );

            pg.query(&c, &write("CREATE TABLE t (id int, v text)")).await.unwrap();
            // Read-only by default.
            let denied = pg.query(&c, &query("INSERT INTO t VALUES (1, 'a')")).await;
            assert!(matches!(&denied, Err(ExecError::Invalid(m)) if m.contains("read-only")), "{denied:?}");
            // write=true commits; row_count is rows affected.
            let ins = pg.query(&c, &write("INSERT INTO t SELECT g, 'r' || g FROM generate_series(1, 10) g")).await.unwrap();
            assert_eq!(ins.row_count, 10);
            assert!(ins.columns.is_empty() && ins.rows.is_empty());
            assert_eq!(single(&pg, &c, "SELECT count(*) FROM t").await, DbValue::Int(10));

            // max_rows truncates.
            let page = pg.query(&c, &DbQuery { max_rows: 3, ..query("SELECT id FROM t ORDER BY id") }).await.unwrap();
            assert!(page.truncated);
            assert_eq!(page.rows, vec![vec![DbValue::Int(1)], vec![DbValue::Int(2)], vec![DbValue::Int(3)]]);
            let exact = pg.query(&c, &DbQuery { max_rows: 10, ..query("SELECT id FROM t") }).await.unwrap();
            assert!(!exact.truncated);
            assert_eq!(exact.row_count, 10);
            // A truncated write still commits everything.
            let upd = pg
                .query(&c, &DbQuery { max_rows: 2, ..write("UPDATE t SET v = 'u' RETURNING id") })
                .await
                .unwrap();
            assert!(upd.truncated);
            assert_eq!(upd.rows.len(), 2);
            assert_eq!(upd.row_count, 10);
            assert_eq!(single(&pg, &c, "SELECT count(*) FROM t WHERE v = 'u'").await, DbValue::Int(10));
            // Empty result keeps the column list.
            let empty = pg.query(&c, &query("SELECT id, v FROM t WHERE false")).await.unwrap();
            assert_eq!(
                empty.columns,
                vec![
                    DbColumn { name: "id".into(), type_name: "int4".into() },
                    DbColumn { name: "v".into(), type_name: "text".into() }
                ]
            );
            assert!(empty.rows.is_empty() && !empty.truncated);

            // statement_timeout.
            let started = Instant::now();
            let slow = pg
                .query(&c, &DbQuery { timeout: Duration::from_millis(200), ..query("SELECT pg_sleep(5)") })
                .await;
            assert!(matches!(&slow, Err(ExecError::Invalid(m)) if m.contains("statement timeout")), "{slow:?}");
            assert!(started.elapsed() < Duration::from_secs(3));

            // One statement per call.
            assert!(matches!(pg.query(&c, &query("SELECT 1; SELECT 2")).await, Err(ExecError::Invalid(_))));

            // Parameters bind with their own types; NULL is inferred from context.
            let bound = pg
                .query(
                    &c,
                    &DbQuery {
                        params: vec![
                            DbValue::Int(41),
                            DbValue::Text("x".into()),
                            DbValue::Null,
                            DbValue::Bool(true),
                            DbValue::Float(0.5),
                        ],
                        ..query("SELECT $1 + 1, $2 || 'y', $3::int IS NULL, NOT $4, $5 * 2")
                    },
                )
                .await
                .unwrap();
            assert_eq!(
                bound.rows[0],
                vec![
                    DbValue::Int(42),
                    DbValue::Text("xy".into()),
                    DbValue::Bool(true),
                    DbValue::Bool(false),
                    DbValue::Float(1.0)
                ]
            );
            let filtered = pg
                .query(&c, &DbQuery { params: vec![DbValue::Int(4)], ..query("SELECT v FROM t WHERE id = $1") })
                .await
                .unwrap();
            assert_eq!(filtered.rows, vec![vec![DbValue::Text("u".into())]]);

            // Type mapping.
            let typed = pg
                .query(
                    &c,
                    &query(
                        "SELECT true, 2::int2, 3::int4, 4::int8, 1.5::float4, 2.25::float8, 'a'::text, \
                         'b'::varchar, 'c'::char(2), NULL::text, \
                         'a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11'::uuid, '{\"k\": [1, 2]}'::jsonb, \
                         '{\"k\":1}'::json, '\\x01ff'::bytea, 12.340::numeric, -0.5::numeric, \
                         'NaN'::numeric, '2024-01-02 03:04:05.5+00'::timestamptz, \
                         '2024-01-02 03:04:05'::timestamp, '2024-01-02'::date, '03:04:05'::time, \
                         '1 day'::interval",
                    ),
                )
                .await
                .unwrap();
            assert_eq!(
                typed.rows[0],
                vec![
                    DbValue::Bool(true),
                    DbValue::Int(2),
                    DbValue::Int(3),
                    DbValue::Int(4),
                    DbValue::Float(1.5),
                    DbValue::Float(2.25),
                    DbValue::Text("a".into()),
                    DbValue::Text("b".into()),
                    DbValue::Text("c ".into()),
                    DbValue::Null,
                    DbValue::Text("a0eebc99-9c0b-4ef8-bb6d-6bb9bd380a11".into()),
                    DbValue::Text("{\"k\": [1, 2]}".into()),
                    DbValue::Text("{\"k\":1}".into()),
                    DbValue::Text("\\x01ff".into()),
                    DbValue::Text("12.340".into()),
                    DbValue::Text("-0.5".into()),
                    DbValue::Text("NaN".into()),
                    DbValue::Text("2024-01-02T03:04:05.5Z".into()),
                    DbValue::Text("2024-01-02T03:04:05.000000".into()),
                    DbValue::Text("2024-01-02".into()),
                    DbValue::Text("03:04:05.000000".into()),
                    DbValue::Text("<unsupported type interval>".into()),
                ]
            );
            let type_names: Vec<&str> = typed.columns.iter().map(|c| c.type_name.as_str()).collect();
            assert_eq!(&type_names[..4], &["bool", "int2", "int4", "int8"]);
            assert_eq!(type_names[8], "bpchar");
            assert_eq!(type_names[21], "interval");

            // Errors: bad SQL is Invalid with the server message; bad credentials are Unavailable.
            let bad = pg.query(&c, &query("SELECT * FROM missing_table")).await;
            assert!(matches!(&bad, Err(ExecError::Invalid(m)) if m.contains("missing_table")), "{bad:?}");
            // Only meaningful when the server checks passwords (executor.md pg_hba: scram); a
            // `trust` test container accepts any password.
            let trust: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_hba_file_rules WHERE auth_method = 'trust' AND type = 'host')")
                .fetch_one(&pg.pool)
                .await
                .unwrap_or(false);
            if trust {
                println!("password check skipped: test server uses trust authentication");
            } else {
                let wrong = DbCreds { password: "wrong".into(), ..c.clone() };
                let denied = pg.query(&wrong, &query("SELECT 1")).await;
                assert!(matches!(&denied, Err(ExecError::Unavailable(m)) if !m.contains("wrong")), "{denied:?}");
            }
        })
        .await;
    }
}
