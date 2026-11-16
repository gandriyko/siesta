//! Sessions and query execution on sqlx: paging / sorting / filter wrapper, cancel, value types.
//!
//! One live connection per [`Session`] (no pool), so `SET`, temp tables and transactions persist
//! between runs. Async, but needs a tokio runtime (sqlx); cancel a run with [`CancelHandle`], not
//! by dropping the future.
//!
//! Notices: MySQL warnings are read with `SHOW WARNINGS` after non-query statements. Postgres
//! `NOTICE`s are not reachable (sqlx only logs them), so `notices` stays empty for Postgres.

mod schema;
mod value;
mod wrap;

use std::collections::HashMap;
use std::fmt;
use std::time::{Duration, Instant};

use db_conn::{Connection, Driver, SslMode};
use futures_util::{Stream, StreamExt};
use sql_split::{Dialect, Kind};
use sqlx::mysql::{MySqlConnectOptions, MySqlConnection, MySqlRow, MySqlSslMode};
use sqlx::postgres::types::Oid;
use sqlx::postgres::{PgConnectOptions, PgConnection, PgDatabaseError, PgErrorPosition, PgRow, PgSslMode};
use sqlx::types::chrono;
use sqlx::{AssertSqlSafe, SqlSafeStr as _, Column as _, ConnectOptions as _, Connection as _, Either, Executor, Row, Statement, TypeInfo, ValueRef};

pub use schema::{ColumnInfo, Index, Schema, SchemaNode, Table, TableKind};
pub use value::Value;
pub use wrap::{Cond, Op, Page, parse_filter, quote_ident, strip, wrap};

/// Default for [`Session::max_rows`].
pub const MAX_ROWS: usize = 10_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Column {
    pub name: String,
    /// Postgres: `int8`, `jsonb`, `varchar`, `int4[]`; MySQL: `BIGINT`, `JSON`, `VARCHAR`.
    pub type_name: String,
}

#[derive(Debug, Clone)]
pub struct Outcome {
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Value>>,
    /// Wrapped queries only: a row beyond the page exists.
    pub has_more: bool,
    /// Unwrapped statements only: rows were cut at `max_rows`.
    pub truncated: bool,
    /// Set for statements that are neither SELECT-like nor SHOW / EXPLAIN (DML, DDL).
    pub rows_affected: Option<u64>,
    pub notices: Vec<String>,
    /// What was sent to the server (the wrapper for pages).
    pub executed_sql: String,
    /// Bind parameters of `executed_sql`.
    pub params: Vec<String>,
    pub in_transaction: bool,
    pub duration: Duration,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DbError {
    /// SQLSTATE (Postgres and MySQL).
    pub code: Option<String>,
    pub message: String,
    pub detail: Option<String>,
    pub hint: Option<String>,
    /// Postgres: 1-based char position inside the statement text passed to `run`
    /// (mapped back through the page wrapper); `None` when it falls outside that text.
    pub position: Option<usize>,
    /// The connection is gone (I/O error, server shutdown / terminate).
    pub connection_lost: bool,
}

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.code {
            Some(c) => write!(f, "{c}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for DbError {}

impl DbError {
    fn msg(message: impl Into<String>) -> Self {
        DbError { message: message.into(), ..Default::default() }
    }

    /// `prefix` chars precede the user's text in the sent SQL; the user's text is `len` chars long.
    fn from_sqlx(e: sqlx::Error, prefix: usize, len: usize) -> Self {
        let mut out = DbError::msg(e.to_string());
        match &e {
            sqlx::Error::Io(_) | sqlx::Error::Tls(_) | sqlx::Error::WorkerCrashed => out.connection_lost = true,
            sqlx::Error::Database(d) => {
                out.message = d.message().to_string();
                out.code = d.code().map(|c| c.into_owned());
                if let Some(pg) = d.try_downcast_ref::<PgDatabaseError>() {
                    out.detail = pg.detail().map(str::to_string);
                    out.hint = pg.hint().map(str::to_string);
                    out.position = match pg.position() {
                        // One past the end is "at end of input": the newline after the statement.
                        Some(PgErrorPosition::Original(p)) if p > prefix && p <= prefix + len + 1 => Some(p - prefix),
                        _ => None,
                    };
                }
                let code = out.code.as_deref().unwrap_or("");
                out.connection_lost = matches!(code, "57P01" | "57P02" | "57P03") || code.starts_with("08");
            }
            _ => {}
        }
        out
    }
}

enum Conn {
    Pg(PgConnection),
    My(MySqlConnection),
}

enum Opts {
    Pg(PgConnectOptions),
    My(MySqlConnectOptions),
}

/// Cancels the running statement of a session from a separate short connection.
/// The session stays usable (the cancelled run fails with SQLSTATE `57014` / `70100`).
#[derive(Clone)]
pub struct CancelHandle {
    opts: std::sync::Arc<Opts>,
    backend_id: u64,
}

impl CancelHandle {
    pub async fn cancel(&self) -> Result<(), DbError> {
        let e = |e| DbError::from_sqlx(e, 0, 0);
        match &*self.opts {
            Opts::Pg(o) => {
                let mut c = PgConnection::connect_with(o).await.map_err(e)?;
                sqlx::query("SELECT pg_cancel_backend($1)").bind(self.backend_id as i32).execute(&mut c).await.map_err(e)?;
                let _ = c.close().await;
            }
            Opts::My(o) => {
                let mut c = MySqlConnection::connect_with(o).await.map_err(e)?;
                sqlx::raw_sql(AssertSqlSafe(format!("KILL QUERY {}", self.backend_id))).execute(&mut c).await.map_err(e)?;
                let _ = c.close().await;
            }
        }
        Ok(())
    }
}

pub struct Session {
    conn: Conn,
    opts: std::sync::Arc<Opts>,
    spec: Connection,
    backend_id: u64,
    in_tx: bool,
    pg_types: HashMap<u32, String>,
    /// Row cap for statements that cannot be wrapped (default [`MAX_ROWS`]).
    pub max_rows: usize,
}

fn pg_options(c: &Connection) -> Result<PgConnectOptions, DbError> {
    let mut o = match &c.url {
        Some(u) => u.parse::<PgConnectOptions>().map_err(|e| DbError::msg(format!("invalid url: {e}")))?,
        None => PgConnectOptions::new(),
    };
    if let Some(h) = &c.host {
        o = o.host(h);
    }
    if let Some(p) = c.port {
        o = o.port(p);
    }
    if let Some(d) = &c.database {
        o = o.database(d);
    }
    if let Some(u) = &c.user {
        o = o.username(u);
    }
    if let Some(p) = &c.password {
        o = o.password(p);
    }
    if let Some(m) = c.ssl.mode {
        o = o.ssl_mode(match m {
            SslMode::Disable => PgSslMode::Disable,
            SslMode::Prefer => PgSslMode::Prefer,
            SslMode::Require => PgSslMode::Require,
            SslMode::VerifyCa => PgSslMode::VerifyCa,
            SslMode::VerifyFull => PgSslMode::VerifyFull,
        });
    }
    if let Some(p) = &c.ssl.ca {
        o = o.ssl_root_cert(p);
    }
    if let Some(p) = &c.ssl.cert {
        o = o.ssl_client_cert(p);
    }
    if let Some(p) = &c.ssl.key {
        o = o.ssl_client_key(p);
    }
    Ok(o.application_name("siesta").disable_statement_logging())
}

fn mysql_options(c: &Connection) -> Result<MySqlConnectOptions, DbError> {
    let mut o = match &c.url {
        Some(u) => u.parse::<MySqlConnectOptions>().map_err(|e| DbError::msg(format!("invalid url: {e}")))?,
        None => MySqlConnectOptions::new(),
    };
    if let Some(h) = &c.host {
        o = o.host(h);
    }
    if let Some(p) = c.port {
        o = o.port(p);
    }
    if let Some(d) = &c.database {
        o = o.database(d);
    }
    if let Some(u) = &c.user {
        o = o.username(u);
    }
    if let Some(p) = &c.password {
        o = o.password(p);
    }
    if let Some(m) = c.ssl.mode {
        o = o.ssl_mode(match m {
            SslMode::Disable => MySqlSslMode::Disabled,
            SslMode::Prefer => MySqlSslMode::Preferred,
            SslMode::Require => MySqlSslMode::Required,
            SslMode::VerifyCa => MySqlSslMode::VerifyCa,
            SslMode::VerifyFull => MySqlSslMode::VerifyIdentity,
        });
    }
    if let Some(p) = &c.ssl.ca {
        o = o.ssl_ca(p);
    }
    if let Some(p) = &c.ssl.cert {
        o = o.ssl_client_cert(p);
    }
    if let Some(p) = &c.ssl.key {
        o = o.ssl_client_key(p);
    }
    Ok(o.disable_statement_logging())
}

fn dialect(d: Driver) -> Dialect {
    match d {
        Driver::Postgres => Dialect::Postgres,
        Driver::MySql => Dialect::MySql,
    }
}

type SqlxResult<T> = Result<T, sqlx::Error>;

fn sql_str(s: &str) -> sqlx::SqlStr {
    AssertSqlSafe(s.to_string()).into_sql_str()
}

impl Session {
    /// Connects and applies `schema`, `read_only` and `init_sql`.
    pub async fn open(c: &Connection) -> Result<Session, DbError> {
        let e = |e| DbError::from_sqlx(e, 0, 0);
        let (conn, opts) = match c.driver {
            Driver::Postgres => {
                let o = pg_options(c)?;
                (Conn::Pg(PgConnection::connect_with(&o).await.map_err(e)?), Opts::Pg(o))
            }
            Driver::MySql => {
                let o = mysql_options(c)?;
                (Conn::My(MySqlConnection::connect_with(&o).await.map_err(e)?), Opts::My(o))
            }
        };
        let mut s = Session { conn, opts: opts.into(), spec: c.clone(), backend_id: 0, in_tx: false, pg_types: HashMap::new(), max_rows: MAX_ROWS };
        s.init().await.map_err(e)?;
        Ok(s)
    }

    async fn init(&mut self) -> SqlxResult<()> {
        match &mut self.conn {
            Conn::Pg(c) => {
                self.backend_id = sqlx::query("SELECT pg_backend_pid()").fetch_one(&mut *c).await?.try_get::<i32, _>(0)? as u64;
                if let Some(s) = &self.spec.schema {
                    sqlx::query("SELECT set_config('search_path', $1, false)").bind(s).execute(&mut *c).await?;
                }
                if self.spec.read_only {
                    sqlx::raw_sql("SET default_transaction_read_only = on").execute(&mut *c).await?;
                }
            }
            Conn::My(c) => {
                self.backend_id = sqlx::query("SELECT CONNECTION_ID()").fetch_one(&mut *c).await?.try_get::<u64, _>(0)?;
                if self.spec.read_only {
                    sqlx::raw_sql("SET SESSION TRANSACTION READ ONLY").execute(&mut *c).await?;
                }
            }
        }
        if let Some(init) = self.spec.init_sql.clone() {
            // One statement at a time: works the same on both servers.
            for st in sql_split::split(&init, self.dialect()).statements {
                self.simple(st.text(&init)).await?;
            }
        }
        Ok(())
    }

    async fn simple(&mut self, sql: &str) -> SqlxResult<()> {
        let q = sqlx::raw_sql(AssertSqlSafe(sql.to_string()));
        match &mut self.conn {
            Conn::Pg(c) => q.execute(c).await.map(drop),
            Conn::My(c) => q.execute(c).await.map(drop),
        }
    }

    pub fn dialect(&self) -> Dialect {
        dialect(self.spec.driver)
    }

    /// `pg_backend_pid()` / `CONNECTION_ID()` of the current connection.
    pub fn backend_id(&self) -> u64 {
        self.backend_id
    }

    /// An open transaction, tracked from the executed `BEGIN` / `COMMIT` / `ROLLBACK`
    /// statements (sqlx does not expose the server's transaction state).
    pub fn in_transaction(&self) -> bool {
        self.in_tx
    }

    pub fn cancel_handle(&self) -> CancelHandle {
        CancelHandle { opts: self.opts.clone(), backend_id: self.backend_id }
    }

    pub async fn close(self) {
        match self.conn {
            Conn::Pg(c) => drop(c.close().await),
            Conn::My(c) => drop(c.close().await),
        }
    }

    /// Runs one statement (or, for unwrapped kinds, a script). If the connection turned out to be
    /// dead it is reopened once and the statement rerun, with a notice that session state was lost.
    pub async fn run(&mut self, sql: &str, kind: Kind, page: &Page) -> Result<Outcome, DbError> {
        match self.run_once(sql, kind, page).await {
            Err(e) if e.connection_lost => {
                let spec = self.spec.clone();
                let max_rows = self.max_rows;
                *self = Session::open(&spec).await?;
                self.max_rows = max_rows;
                if kind == Kind::Other {
                    // The statement may have run before the connection died: never rerun it blindly.
                    return Err(DbError {
                        message: "Connection was lost and reopened; session state is lost. The statement may or may not have run — check and run it again.".into(),
                        connection_lost: true,
                        ..Default::default()
                    });
                }
                let mut out = self.run_once(sql, kind, page).await?;
                out.notices.insert(0, "Connection was lost and reopened; session state (variables, temporary tables, open transaction) was reset.".into());
                Ok(out)
            }
            r => r,
        }
    }

    /// `SELECT count(*)` over the same statement and filters (no paging).
    pub async fn count(&mut self, sql: &str, filters: &[(String, String)]) -> Result<u64, DbError> {
        let d = self.dialect();
        let cols = if filters.iter().any(|f| !f.1.trim().is_empty()) { self.columns_of(strip(sql)).await? } else { Vec::new() };
        let w = wrap::build_count(sql, d, filters, &cols);
        let types = (d == Dialect::Postgres).then(|| vec!["int8".to_string()]);
        let raw = self.fetch(&w.sql, &w.params, types.as_deref(), None, 1, false).await.map_err(|e| DbError::from_sqlx(e, w.prefix_chars, w.inner_chars))?;
        match raw.rows.first().and_then(|r| r.first()) {
            Some(Value::Int(n)) => Ok(*n as u64),
            other => Err(DbError::msg(format!("unexpected count result: {other:?}"))),
        }
    }

    async fn run_once(&mut self, sql: &str, kind: Kind, page: &Page) -> Result<Outcome, DbError> {
        let started = Instant::now();
        let d = self.dialect();
        let (exec_sql, params, prefix, len, known, limit) = if kind == Kind::Query {
            let cols = if page.filters.iter().any(|f| !f.1.trim().is_empty()) { self.columns_of(strip(sql)).await? } else { Vec::new() };
            let w = wrap::build(sql, d, page, &cols);
            (w.sql, w.params, w.prefix_chars, w.inner_chars, (!cols.is_empty()).then_some(cols), page.size as usize + 1)
        } else {
            (sql.to_string(), Vec::new(), 0, usize::MAX - 1, None, self.max_rows)
        };
        let types: Option<Vec<String>> = (d == Dialect::Postgres && !params.is_empty()).then(|| known.iter().flatten().map(|c| c.type_name.clone()).collect());
        let describe_empty = kind != Kind::Other || sql.to_lowercase().contains("returning");
        let mut raw = self
            .fetch(&exec_sql, &params, types.as_deref(), known.as_deref(), limit, describe_empty)
            .await
            .map_err(|e| DbError::from_sqlx(e, prefix, len))?;
        let mut has_more = false;
        if kind == Kind::Query {
            has_more = raw.rows.len() > page.size as usize;
            raw.rows.truncate(page.size as usize);
            raw.truncated = false;
        }
        let mut notices = Vec::new();
        if kind == Kind::Other && d == Dialect::MySql {
            notices = self.mysql_warnings().await;
        }
        // MariaDB reports 0 affected rows for `DELETE … RETURNING`: the returned rows are the count.
        if d == Dialect::MySql && raw.affected == 0 {
            raw.affected = raw.rows.len() as u64;
        }
        self.in_tx = tx_state(self.in_tx, sql, d);
        Ok(Outcome {
            columns: raw.columns,
            rows: raw.rows,
            has_more,
            truncated: raw.truncated,
            rows_affected: (kind == Kind::Other).then_some(raw.affected),
            notices,
            executed_sql: exec_sql,
            params,
            in_transaction: self.in_tx,
            duration: started.elapsed(),
        })
    }

    /// Column names and types of a statement, without running it.
    async fn columns_of(&mut self, inner: &str) -> Result<Vec<Column>, DbError> {
        let len = inner.chars().count();
        let e = |e| DbError::from_sqlx(e, 0, len);
        let sql = sql_str(inner);
        let cols = match &mut self.conn {
            Conn::Pg(c) => c.prepare(sql).await.map_err(e)?.columns().iter().map(|c| (c.name().to_string(), pg_name(c.type_info()), c.type_info().oid().map(|o| o.0))).collect(),
            Conn::My(c) => c.prepare(sql).await.map_err(e)?.columns().iter().map(|c| (c.name().to_string(), c.type_info().name().to_string(), None)).collect(),
        };
        Ok(self.name_columns(cols).await)
    }

    /// Resolves Postgres types sqlx doesn't know by name (enums, extensions) through `pg_type`.
    async fn name_columns(&mut self, cols: Vec<(String, String, Option<u32>)>) -> Vec<Column> {
        let mut out = Vec::new();
        for (name, mut ty, oid) in cols {
            if let (true, Some(oid), Conn::Pg(_)) = (ty == "?", oid, &self.conn) {
                ty = self.pg_type_by_oid(oid).await;
            }
            out.push(Column { name, type_name: ty });
        }
        out
    }

    async fn pg_type_by_oid(&mut self, oid: u32) -> String {
        if let Some(n) = self.pg_types.get(&oid) {
            return n.clone();
        }
        let Conn::Pg(c) = &mut self.conn else { return "?".into() };
        let name = sqlx::query("SELECT typname::text FROM pg_type WHERE oid = $1")
            .bind(Oid(oid))
            .fetch_optional(c)
            .await
            .ok()
            .flatten()
            .and_then(|r| r.try_get::<String, _>(0).ok())
            .map(|n| value::pg_type_name(&n))
            .unwrap_or_else(|| format!("oid:{oid}"));
        self.pg_types.insert(oid, name.clone());
        name
    }

    /// Streams the rows of one statement. `types` (Postgres, text-cast columns) and `known`
    /// override what the rows themselves say.
    async fn fetch(&mut self, sql: &str, params: &[String], types: Option<&[String]>, known: Option<&[Column]>, limit: usize, describe_empty: bool) -> SqlxResult<Raw> {
        match &mut self.conn {
            Conn::Pg(c) => {
                let stream = if params.is_empty() {
                    sqlx::raw_sql(AssertSqlSafe(sql.to_string())).fetch_many(&mut *c)
                } else {
                    // Prepared statements are used for filtered pages only: one statement, rows only.
                    params.iter().fold(sqlx::query(AssertSqlSafe(sql.to_string())), |q, p| q.bind(p.clone())).fetch(&mut *c).map(|r| r.map(Either::Right)).boxed()
                };
                let (rows, affected, truncated) = drain(stream, limit, |q| q.rows_affected()).await?;
                let meta: Vec<(String, String, Option<u32>)> = match (rows.first(), known) {
                    (_, Some(k)) => k.iter().map(|c| (c.name.clone(), c.type_name.clone(), None)).collect(),
                    (Some(r), None) => r.columns().iter().map(|c| (c.name().to_string(), pg_name(c.type_info()), c.type_info().oid().map(|o| o.0))).collect(),
                    (None, None) if describe_empty => match c.prepare(sql_str(sql)).await {
                        Ok(s) => s.columns().iter().map(|c| (c.name().to_string(), pg_name(c.type_info()), c.type_info().oid().map(|o| o.0))).collect(),
                        Err(_) => Vec::new(),
                    },
                    _ => Vec::new(),
                };
                let mut columns = self.name_columns(meta).await;
                if let Some(t) = types {
                    for (c, t) in columns.iter_mut().zip(t) {
                        c.type_name = t.clone();
                    }
                }
                let rows = rows
                    .iter()
                    .map(|r| {
                        columns
                            .iter()
                            .enumerate()
                            .map(|(i, c)| match pg_cell(r, i) {
                                None => Value::Null,
                                Some(s) => value::from_pg_text(&c.type_name, &s),
                            })
                            .collect()
                    })
                    .collect();
                Ok(Raw { columns, rows, affected, truncated })
            }
            Conn::My(c) => {
                let stream = if params.is_empty() {
                    sqlx::raw_sql(AssertSqlSafe(sql.to_string())).fetch_many(&mut *c)
                } else {
                    params.iter().fold(sqlx::query(AssertSqlSafe(sql.to_string())), |q, p| q.bind(p.clone())).fetch(&mut *c).map(|r| r.map(Either::Right)).boxed()
                };
                let (rows, affected, truncated) = drain(stream, limit, |q| q.rows_affected()).await?;
                let columns: Vec<Column> = match rows.first() {
                    Some(r) => r.columns().iter().map(|c| Column { name: c.name().to_string(), type_name: c.type_info().name().to_string() }).collect(),
                    None if describe_empty => match c.prepare(sql_str(sql)).await {
                        Ok(s) => s.columns().iter().map(|c| Column { name: c.name().to_string(), type_name: c.type_info().name().to_string() }).collect(),
                        Err(_) => Vec::new(),
                    },
                    None => Vec::new(),
                };
                let rows = rows.iter().map(|r| columns.iter().enumerate().map(|(i, c)| my_value(r, i, &c.type_name)).collect()).collect();
                Ok(Raw { columns, rows, affected, truncated })
            }
        }
    }

    /// `SHOW WARNINGS` right after a statement; best effort.
    async fn mysql_warnings(&mut self) -> Vec<String> {
        let Conn::My(c) = &mut self.conn else { return Vec::new() };
        let Ok(rows) = sqlx::raw_sql("SHOW WARNINGS").fetch_all(c).await else { return Vec::new() };
        rows.iter()
            .map(|r| {
                let t = |i, ty| my_value(r, i, ty).display();
                format!("{} {}: {}", t(0, "VARCHAR"), t(1, "INT"), t(2, "VARCHAR"))
            })
            .collect()
    }
}

struct Raw {
    columns: Vec<Column>,
    rows: Vec<Vec<Value>>,
    affected: u64,
    truncated: bool,
}

/// Collects rows up to `limit`; a further row sets `truncated` and stops reading (sqlx drains
/// the rest of the result before the connection's next use).
async fn drain<Q, R>(mut s: impl Stream<Item = SqlxResult<Either<Q, R>>> + Unpin, limit: usize, affected: impl Fn(&Q) -> u64) -> SqlxResult<(Vec<R>, u64, bool)> {
    let (mut rows, mut n, mut truncated) = (Vec::new(), 0, false);
    while let Some(item) = s.next().await {
        match item? {
            Either::Left(q) => n += affected(&q),
            Either::Right(r) if rows.len() >= limit => {
                let _ = r;
                truncated = true;
                break;
            }
            Either::Right(r) => rows.push(r),
        }
    }
    Ok((rows, n, truncated))
}

fn pg_name(t: &sqlx::postgres::PgTypeInfo) -> String {
    value::pg_type_name(t.name())
}

/// Postgres cell as server text (simple-query results and `::text` casts are text).
fn pg_cell(row: &PgRow, i: usize) -> Option<String> {
    let v = row.try_get_raw(i).ok()?;
    if v.is_null() {
        return None;
    }
    Some(match v.as_str() {
        Ok(s) => s.to_string(),
        Err(_) => String::from_utf8_lossy(v.as_bytes().unwrap_or_default()).into_owned(),
    })
}

fn my_value(row: &MySqlRow, i: usize, ty: &str) -> Value {
    match row.try_get_raw(i) {
        Ok(v) if v.is_null() => return Value::Null,
        Err(e) => return Value::Other { type_name: ty.into(), text: format!("<{e}>") },
        _ => {}
    }
    let r: SqlxResult<Value> = (|| {
        Ok(match ty {
            "BOOLEAN" => Value::Bool(row.try_get_unchecked::<i64, _>(i)? != 0),
            "TINYINT" | "SMALLINT" | "MEDIUMINT" | "INT" | "BIGINT" => Value::Int(row.try_get_unchecked::<i64, _>(i)?),
            "YEAR" => Value::Int(row.try_get_unchecked::<u16, _>(i)? as i64),
            "TINYINT UNSIGNED" | "SMALLINT UNSIGNED" | "MEDIUMINT UNSIGNED" | "INT UNSIGNED" | "BIGINT UNSIGNED" => {
                let n = row.try_get_unchecked::<u64, _>(i)?;
                i64::try_from(n).map(Value::Int).unwrap_or_else(|_| Value::Decimal(n.to_string()))
            }
            // f32 → shortest text → f64, so 0.1 stays 0.1.
            "FLOAT" => Value::Float(row.try_get_unchecked::<f32, _>(i)?.to_string().parse().unwrap_or(f64::NAN)),
            "DOUBLE" => Value::Float(row.try_get_unchecked::<f64, _>(i)?),
            "DECIMAL" => Value::Decimal(row.try_get_unchecked::<String, _>(i)?),
            "JSON" => Value::Json(row.try_get_unchecked::<serde_json::Value, _>(i)?),
            "CHAR" | "VARCHAR" | "TEXT" | "TINYTEXT" | "MEDIUMTEXT" | "LONGTEXT" | "ENUM" | "SET" => Value::Text(row.try_get_unchecked::<String, _>(i)?),
            "BINARY" | "VARBINARY" | "BLOB" | "TINYBLOB" | "MEDIUMBLOB" | "LONGBLOB" => Value::Bytes(row.try_get_unchecked::<Vec<u8>, _>(i)?),
            "DATE" => Value::DateTime(row.try_get_unchecked::<chrono::NaiveDate, _>(i)?.to_string()),
            "DATETIME" | "TIMESTAMP" => Value::DateTime(row.try_get_unchecked::<chrono::NaiveDateTime, _>(i)?.to_string()),
            "TIME" => {
                // Can be negative and exceed 24h, so not chrono; sqlx's own Display doesn't pad hours.
                let t = row.try_get_unchecked::<sqlx::mysql::types::MySqlTime, _>(i)?;
                // (`MySqlTime::is_negative` is inverted in sqlx 0.9.0; ask the sign itself.)
                let sign = if t.sign().is_negative() { "-" } else { "" };
                let frac = if t.microseconds() > 0 { format!(".{:06}", t.microseconds()) } else { String::new() };
                Value::DateTime(format!("{sign}{:02}:{:02}:{:02}{frac}", t.hours(), t.minutes(), t.seconds()))
            }
            "BIT" => Value::Int(row.try_get_unchecked::<Vec<u8>, _>(i)?.iter().fold(0i64, |a, b| a << 8 | *b as i64)),
            _ => {
                let b = row.try_get_unchecked::<Vec<u8>, _>(i)?;
                Value::Other { type_name: ty.into(), text: String::from_utf8_lossy(&b).into_owned() }
            }
        })
    })();
    r.unwrap_or_else(|e| Value::Other { type_name: ty.into(), text: format!("<undecodable: {e}>") })
}

/// Transaction state after running `sql`, tracked from the statements themselves.
/// MySQL DDL commits implicitly. The state is unchanged for anything else (and on errors).
pub(crate) fn tx_state(mut in_tx: bool, sql: &str, d: Dialect) -> bool {
    for st in sql_split::split(sql, d).statements {
        let text = st.text(sql).to_uppercase();
        let mut w = text.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty());
        let (a, b) = (w.next().unwrap_or(""), w.next().unwrap_or(""));
        match (a, b) {
            ("BEGIN", _) | ("START", "TRANSACTION") => in_tx = true,
            ("COMMIT" | "END", "PREPARED") | ("ROLLBACK", "PREPARED") | ("ROLLBACK", "TO") => {}
            ("COMMIT" | "END" | "ROLLBACK" | "ABORT", _) => in_tx = false,
            ("CREATE" | "DROP", "TEMPORARY") => {}
            ("CREATE" | "ALTER" | "DROP" | "TRUNCATE" | "RENAME", _) if d == Dialect::MySql => in_tx = false,
            _ => {}
        }
    }
    in_tx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transaction_tracking() {
        let pg = Dialect::Postgres;
        assert!(tx_state(false, "BEGIN", pg));
        assert!(tx_state(false, "start transaction isolation level serializable", pg));
        assert!(!tx_state(true, "COMMIT", pg));
        assert!(!tx_state(true, "rollback", pg));
        assert!(!tx_state(true, "END", pg));
        assert!(tx_state(true, "ROLLBACK TO SAVEPOINT a", pg));
        assert!(tx_state(true, "SELECT 1", pg));
        assert!(tx_state(true, "CREATE TABLE t(a int)", pg));
        assert!(!tx_state(false, "SELECT 1", pg));
        assert!(!tx_state(false, "BEGIN; INSERT INTO t VALUES (1); COMMIT", pg));
        assert!(tx_state(false, "SELECT 1; BEGIN", pg));
        let my = Dialect::MySql;
        assert!(!tx_state(true, "CREATE TABLE t(a int)", my));
        assert!(tx_state(true, "CREATE TEMPORARY TABLE t(a int)", my));
        assert!(!tx_state(true, "drop table t", my));
    }

    #[test]
    fn errors_map_without_database() {
        let e = DbError::from_sqlx(sqlx::Error::Io(std::io::Error::other("boom")), 0, 0);
        assert!(e.connection_lost);
        let e = DbError::from_sqlx(sqlx::Error::RowNotFound, 0, 0);
        assert!(!e.connection_lost);
    }
}
