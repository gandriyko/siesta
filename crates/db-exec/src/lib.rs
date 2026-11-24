//! Sessions and query execution on sqlx: paging / sorting / filter wrapper, cancel, value types.
//!
//! One live connection per [`Session`] (no pool), so `SET`, temp tables and transactions persist
//! between runs. Async, but needs a tokio runtime (sqlx); cancel a run with [`CancelHandle`], not
//! by dropping the future.
//!
//! Notices: MySQL warnings are read with `SHOW WARNINGS` after non-query statements. Postgres
//! `NOTICE`s are not reachable (sqlx only logs them), so `notices` stays empty for Postgres.

mod es;
pub mod history;
mod schema;
mod source;
mod value;
mod wrap;

use std::collections::HashMap;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use db_conn::{Connection, Driver, Socket, SslMode};
use std::path::PathBuf;
use futures_util::{Stream, StreamExt};
use sql_split::{Dialect, Kind};
use sqlx::mysql::{MySqlConnectOptions, MySqlConnection, MySqlRow, MySqlSslMode};
use sqlx::postgres::types::Oid;
use sqlx::postgres::{PgConnectOptions, PgConnection, PgDatabaseError, PgErrorPosition, PgRow, PgSslMode};
use sqlx::types::chrono;
use sqlx::{AssertSqlSafe, SqlSafeStr as _, Column as _, ConnectOptions as _, Connection as _, Either, Executor, Row, Statement, TypeInfo, ValueRef};

pub use schema::{ColumnInfo, Index, Schema, is_system_database, SchemaNode, Table, TableKind};
pub use source::{Source, SourceTable};
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
    /// The server paged this query (wrapper or appended `LIMIT`); `false` for statements that
    /// cannot be paged, and for queries run unwrapped because their column names repeat
    /// (sorting and filters are then local or off, see `notices`).
    pub wrapped: bool,
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
    /// Elasticsearch: the whole JSON response (hits wrapper, aggregations, shards), for the
    /// result panel's Raw view. `None` for SQL.
    pub raw: Option<serde_json::Value>,
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
    /// Refused by siesta itself, not sent (the `read_only` guard).
    pub refused: bool,
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
    Es(es::Es),
}

enum Opts {
    Pg(PgConnectOptions),
    My(MySqlConnectOptions),
    /// The base url, and what a cancel wakes to drop the request in flight.
    Es(String, Arc<tokio::sync::Notify>),
}

/// Default connect timeout (setting `run.connect_timeout_ms`).
pub const CONNECT_TIMEOUT: Duration = Duration::from_millis(10_000);

impl Opts {
    /// `host:port` of the effective connection; `None` for a unix socket.
    fn target(&self) -> Option<String> {
        let (host, port, socket) = match self {
            Opts::Pg(o) => (o.get_host(), o.get_port(), o.get_socket().is_some()),
            Opts::My(o) => (o.get_host(), o.get_port(), o.get_socket().is_some()),
            Opts::Es(base, _) => return Some(base.clone()),
        };
        (!socket && !host.starts_with('/')).then(|| format!("{host}:{port}"))
    }
}

/// The error of a connection attempt that got no answer in time (counts as a lost connection).
fn connect_timeout_error(target: Option<&str>, timeout: Duration) -> DbError {
    let ms = timeout.as_millis();
    let secs = if ms.is_multiple_of(1000) { (ms / 1000).to_string() } else { format!("{:.1}", ms as f64 / 1000.0) };
    DbError {
        message: format!(
            "No response from {} within {secs} s. Check the driver and port (Postgres usually listens on 5432, MySQL / MariaDB on 3306, Elasticsearch on 9200).",
            target.unwrap_or("the server")
        ),
        connection_lost: true,
        ..Default::default()
    }
}

/// Bounds a whole connection attempt: a server that speaks another protocol never answers the handshake.
async fn within<T>(opts: &Opts, timeout: Duration, f: impl Future<Output = Result<T, DbError>>) -> Result<T, DbError> {
    tokio::time::timeout(timeout, f).await.unwrap_or_else(|_| Err(connect_timeout_error(opts.target().as_deref(), timeout)))
}

/// Cancels the running statement of a session from a separate short connection.
/// The session stays usable (the cancelled run fails with SQLSTATE `57014` / `70100`).
/// It follows the session's live connection: after an automatic reconnect it targets the new
/// backend, and while the connection is being reopened it does nothing (the old id may be reused).
/// It acts only when the server still shows this session's [`Target`] under that id.
#[derive(Clone)]
pub struct CancelHandle {
    opts: std::sync::Arc<Opts>,
    target: Arc<Mutex<Target>>,
    connect_timeout: Duration,
}

/// What a cancel must find on the server before it acts: the backend id plus a tag the server
/// reports for it. Postgres: that backend's `application_name`, unique per connection
/// ([`pg_options`]). MySQL / MariaDB: server identity and the connection's client `HOST`
/// (host:port). Behind a pooler or load balancer the same id can be another client's connection.
#[derive(Clone, Default)]
struct Target {
    /// `pg_backend_pid()` / `CONNECTION_ID()`; 0 = no live connection.
    id: u64,
    tag: String,
}

/// The [`Target`] tag of MySQL connection `?`, from any connection to the same server.
const MY_TAG: &str = "SELECT CONCAT_WS('/', @@hostname, @@port, @@server_id, HOST) FROM information_schema.PROCESSLIST WHERE ID = ?";

impl CancelHandle {
    pub async fn cancel(&self) -> Result<(), DbError> {
        if let Opts::Es(_, abort) = &*self.opts {
            abort.notify_waiters();
            return Ok(());
        }
        let Target { id, tag } = self.target.lock().unwrap().clone();
        if id == 0 {
            return Ok(());
        }
        let e = |e| DbError::from_sqlx(e, 0, 0);
        let other = || DbError::msg(format!("Nothing cancelled: server connection {id} no longer belongs to this session (a pooler or load balancer in between?)"));
        within(&self.opts, self.connect_timeout, async {
            match &*self.opts {
                Opts::Pg(o) => {
                    let mut c = PgConnection::connect_with(o).await.map_err(e)?;
                    // Check and cancel in one statement.
                    let hit = sqlx::query("SELECT pg_cancel_backend(pid) FROM pg_stat_activity WHERE pid = $1 AND application_name = $2")
                        .bind(id as i32)
                        .bind(&tag)
                        .fetch_optional(&mut c)
                        .await
                        .map_err(e)?;
                    let _ = c.close().await;
                    hit.ok_or_else(other)?;
                }
                Opts::My(o) => {
                    let mut c = MySqlConnection::connect_with(o).await.map_err(e)?;
                    // ponytail: check, then KILL: MySQL has no atomic form; the gap is one round trip.
                    let now: Option<String> = sqlx::query_scalar(MY_TAG).bind(id).fetch_optional(&mut c).await.map_err(e)?;
                    if now.as_deref() != Some(tag.as_str()) {
                        let _ = c.close().await;
                        return Err(other());
                    }
                    sqlx::raw_sql(AssertSqlSafe(format!("KILL QUERY {id}"))).execute(&mut c).await.map_err(e)?;
                    let _ = c.close().await;
                }
                Opts::Es(..) => {}
            }
            Ok(())
        })
        .await
    }
}

pub struct Session {
    conn: Conn,
    opts: std::sync::Arc<Opts>,
    spec: Connection,
    /// Shared with the [`CancelHandle`]s; id 0 while there is no live connection.
    target: Arc<Mutex<Target>>,
    in_tx: bool,
    /// MySQL `SET autocommit = 0`: every statement belongs to an open transaction.
    autocommit_off: bool,
    /// MariaDB (not MySQL): it has `max_statement_time` instead of `max_execution_time`.
    mariadb: bool,
    /// Wanted statement timeout; `timeout_applied` is what the server has.
    timeout_ms: Option<u64>,
    timeout_applied: Option<u64>,
    pg_types: HashMap<u32, String>,
    connect_timeout: Duration,
    /// Row cap for statements that cannot be wrapped (default [`MAX_ROWS`]).
    pub max_rows: usize,
}

/// The Unix socket to use (Postgres: folder, MySQL: file): the typed path, or the detected one.
fn socket_path(c: &Connection) -> Result<Option<PathBuf>, DbError> {
    match &c.socket {
        None => Ok(None),
        Some(Socket::Path(p)) => Ok(Some(PathBuf::from(p))),
        Some(Socket::Auto) => db_conn::detect_socket(c.driver, c.port).map(Some).ok_or_else(|| {
            let name = if c.driver == Driver::Postgres { "Postgres" } else { "MySQL / MariaDB" };
            DbError::msg(format!("No Unix socket found for {name} (looked in {}); type its path in the connection form", db_conn::socket_candidates(c.driver).join(", ")))
        }),
    }
}

fn pg_options(c: &Connection) -> Result<PgConnectOptions, DbError> {
    let mut o = match &c.url {
        Some(u) => u.parse::<PgConnectOptions>().map_err(|e| DbError::msg(format!("invalid url: {e}")))?,
        None => PgConnectOptions::new(),
    };
    if let Some(dir) = socket_path(c)? {
        // A folder: sqlx connects to `<dir>/.s.PGSQL.<port>`.
        o = o.socket(dir);
    } else if let Some(h) = &c.host {
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
    } else if matches!(o.get_username(), "" | "anonymous" | "unknown") {
        // sqlx falls back to a stub name when it can't read the OS user: use the real one like libpq.
        o = o.username(&db_conn::os_user());
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
    Ok(o.application_name(&unique_app_name()).disable_statement_logging())
}

/// `siesta-<pid>-<time>-<n>`: unique per connection, so a cancel can tell its backend apart ([`Target`]).
fn unique_app_name() -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos();
    format!("siesta-{}-{t:x}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed))
}

fn mysql_options(c: &Connection) -> Result<MySqlConnectOptions, DbError> {
    let mut o = match &c.url {
        Some(u) => u.parse::<MySqlConnectOptions>().map_err(|e| DbError::msg(format!("invalid url: {e}")))?,
        None => MySqlConnectOptions::new(),
    };
    if let Some(file) = socket_path(c)? {
        o = o.socket(file);
    } else if let Some(h) = &c.host {
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

pub fn dialect(d: Driver) -> Dialect {
    match d {
        Driver::Postgres => Dialect::Postgres,
        Driver::MySql => Dialect::MySql,
        Driver::Elastic => Dialect::Elastic,
    }
}

type SqlxResult<T> = Result<T, sqlx::Error>;

fn sql_str(s: &str) -> sqlx::SqlStr {
    AssertSqlSafe(s.to_string()).into_sql_str()
}

/// The databases `c`'s credentials can open. Postgres connects to a database even to list them, and
/// the default one (the user's name) often doesn't exist: on SQLSTATE `3D000` it retries on
/// `postgres`, then `template1`.
pub async fn list_databases(c: &Connection, connect_timeout: Duration) -> Result<Vec<String>, DbError> {
    if c.driver == Driver::Elastic {
        // A cluster has no databases; `@database` does not apply.
        return Ok(Vec::new());
    }
    let mut tried: Vec<Option<String>> = vec![c.database.clone().filter(|d| !d.is_empty())];
    let mut conn = c.clone();
    conn.database = tried[0].clone();
    loop {
        match Session::open_with(&conn, connect_timeout).await {
            Ok(mut s) => {
                let names = s.databases().await;
                s.close().await;
                return names;
            }
            Err(e) if c.driver == Driver::Postgres && e.code.as_deref() == Some("3D000") => {
                let Some(next) = ["postgres", "template1"].into_iter().find(|n| !tried.iter().any(|t| t.as_deref() == Some(*n))) else { return Err(e) };
                tried.push(Some(next.into()));
                conn.database = Some(next.into());
            }
            Err(e) => return Err(e),
        }
    }
}

impl Session {
    /// Connects and applies `schema`, `read_only` and `init_sql`.
    pub async fn open(c: &Connection) -> Result<Session, DbError> {
        Self::open_with(c, CONNECT_TIMEOUT).await
    }

    /// [`open`](Self::open) with a connect timeout over the whole attempt (TLS, handshake, setup
    /// statements); also used by cancel and reconnect of this session.
    pub async fn open_with(c: &Connection, connect_timeout: Duration) -> Result<Session, DbError> {
        // The one place the password is decrypted (also on reconnect: `spec` keeps the resolved copy).
        let resolved = c.resolve_password().map_err(DbError::msg)?;
        let c = &resolved;
        let e = |e| {
            let mut e = DbError::from_sqlx(e, 0, 0);
            // SQLSTATE class 28: invalid authorization (Postgres 28P01 / 28000, MySQL 28000).
            if !c.has_password() && e.code.as_deref().is_some_and(|code| code.starts_with("28")) {
                // In the message, not the hint: connect errors are shown as plain text everywhere.
                e.message = format!("{}. No password is stored for `{1}`: set it in the connection form or with `siesta connections set-password {1}`", e.message, c.name);
            }
            e
        };
        let opts: Arc<Opts> = match c.driver {
            Driver::Postgres => Opts::Pg(pg_options(c)?),
            Driver::MySql => Opts::My(mysql_options(c)?),
            Driver::Elastic => Opts::Es(es::base_url(c)?, Arc::default()),
        }
        .into();
        // sqlx 0.9 has no connect timeout on the connect options: this one is the guarantee.
        within(&opts, connect_timeout, async {
            let conn = match &*opts {
                Opts::Pg(o) => Conn::Pg(PgConnection::connect_with(o).await.map_err(e)?),
                Opts::My(o) => Conn::My(MySqlConnection::connect_with(o).await.map_err(e)?),
                Opts::Es(_, abort) => {
                    let mut es = es::Es::open(c).await?;
                    es.abort = abort.clone();
                    Conn::Es(es)
                }
            };
            let mut s = Session {
                conn,
                opts: opts.clone(),
                spec: c.clone(),
                target: Arc::default(),
                in_tx: false,
                autocommit_off: false,
                mariadb: false,
                timeout_ms: None,
                timeout_applied: None,
                pg_types: HashMap::new(),
                connect_timeout,
                max_rows: MAX_ROWS,
            };
            s.init().await.map_err(e)?;
            Ok(s)
        })
        .await
    }

    /// The statement timeout of this session (`None` = none), applied now and again after a
    /// reconnect: Postgres `statement_timeout`, MySQL `max_execution_time` (SELECT only),
    /// MariaDB `max_statement_time`.
    pub async fn set_statement_timeout(&mut self, ms: Option<u64>) -> Result<(), DbError> {
        self.timeout_ms = ms;
        self.apply_timeout().await.map_err(|e| DbError::from_sqlx(e, 0, 0))
    }

    async fn apply_timeout(&mut self) -> SqlxResult<()> {
        if self.timeout_applied == self.timeout_ms {
            return Ok(());
        }
        let ms = self.timeout_ms.unwrap_or(0);
        let sql = match (&mut self.conn, self.mariadb) {
            (Conn::Pg(_), _) => format!("SET statement_timeout = {ms}"),
            (Conn::My(_), false) => format!("SET SESSION max_execution_time = {ms}"),
            // Seconds, fractions allowed.
            (Conn::My(_), true) => format!("SET SESSION max_statement_time = {}", ms as f64 / 1000.0),
            // The whole HTTP request.
            (Conn::Es(es), _) => {
                es.timeout = (ms > 0).then(|| Duration::from_millis(ms));
                self.timeout_applied = self.timeout_ms;
                return Ok(());
            }
        };
        self.simple(&sql).await?;
        self.timeout_applied = self.timeout_ms;
        Ok(())
    }

    async fn init(&mut self) -> SqlxResult<()> {
        match &mut self.conn {
            Conn::Pg(c) => {
                if let Some(s) = &self.spec.schema {
                    sqlx::query("SELECT set_config('search_path', $1, false)").bind(s).execute(&mut *c).await?;
                }
                if self.spec.read_only {
                    sqlx::raw_sql("SET default_transaction_read_only = on").execute(&mut *c).await?;
                }
            }
            Conn::My(c) => {
                let version = sqlx::query("SELECT VERSION()").fetch_one(&mut *c).await?.try_get::<String, _>(0)?;
                self.mariadb = version.to_ascii_lowercase().contains("mariadb");
                if self.spec.read_only {
                    sqlx::raw_sql("SET SESSION TRANSACTION READ ONLY").execute(&mut *c).await?;
                }
            }
            // No session state on the server: `read_only` is the client check, `init_sql` is SQL.
            Conn::Es(_) => return Ok(()),
        }
        if let Some(init) = self.spec.init_sql.clone() {
            // One statement at a time: works the same on both servers.
            for st in sql_split::split(&init, self.dialect()).statements {
                self.simple(st.text(&init)).await?;
            }
        }
        // After `init_sql`: it may set `application_name`.
        self.read_target().await
    }

    /// Reads this connection's [`Target`] (shared with the cancel handles).
    async fn read_target(&mut self) -> SqlxResult<()> {
        let t = match &mut self.conn {
            Conn::Pg(c) => {
                let r = sqlx::query("SELECT pg_backend_pid(), current_setting('application_name')").fetch_one(&mut *c).await?;
                Target { id: r.try_get::<i32, _>(0)? as u64, tag: r.try_get(1)? }
            }
            Conn::My(c) => {
                let id: u64 = sqlx::query_scalar("SELECT CONNECTION_ID()").fetch_one(&mut *c).await?;
                Target { id, tag: sqlx::query_scalar(MY_TAG).bind(id).fetch_one(&mut *c).await? }
            }
            Conn::Es(_) => Target::default(),
        };
        *self.target.lock().unwrap() = t;
        Ok(())
    }

    async fn simple(&mut self, sql: &str) -> SqlxResult<()> {
        let q = sqlx::raw_sql(AssertSqlSafe(sql.to_string()));
        match &mut self.conn {
            Conn::Pg(c) => q.execute(c).await.map(drop)?,
            Conn::My(c) => q.execute(c).await.map(drop)?,
            Conn::Es(_) => return Ok(()),
        }
        if let Some(on) = autocommit_setting(sql, self.dialect()) {
            self.autocommit_off = !on;
        }
        Ok(())
    }

    pub fn dialect(&self) -> Dialect {
        dialect(self.spec.driver)
    }

    /// `pg_backend_pid()` / `CONNECTION_ID()` of the current connection.
    pub fn backend_id(&self) -> u64 {
        self.target.lock().unwrap().id
    }

    /// An open transaction, tracked from the executed `BEGIN` / `COMMIT` / `ROLLBACK`
    /// statements (sqlx does not expose the server's transaction state). MySQL with
    /// `autocommit = 0` is always in one.
    pub fn in_transaction(&self) -> bool {
        self.in_tx || self.autocommit_off
    }

    pub fn cancel_handle(&self) -> CancelHandle {
        CancelHandle { opts: self.opts.clone(), target: self.target.clone(), connect_timeout: self.connect_timeout }
    }

    /// Replaces a dead connection with a new one: same settings, same cancel handles.
    async fn reopen(&mut self) -> Result<(), DbError> {
        let shared = self.target.clone();
        *shared.lock().unwrap() = Target::default(); // a cancel must not hit a reused id
        let mut new = Session::open_with(&self.spec.clone(), self.connect_timeout).await?;
        new.max_rows = self.max_rows;
        new.timeout_ms = self.timeout_ms;
        new.apply_timeout().await.map_err(|e| DbError::from_sqlx(e, 0, 0))?;
        *shared.lock().unwrap() = new.target.lock().unwrap().clone();
        new.target = shared;
        *self = new;
        Ok(())
    }

    pub async fn close(self) {
        match self.conn {
            Conn::Pg(c) => drop(c.close().await),
            Conn::My(c) => drop(c.close().await),
            Conn::Es(_) => {}
        }
    }

    /// Runs one statement (or, for unwrapped kinds, a script). If the connection turned out to be
    /// dead it is reopened once and the statement rerun, with a notice that session state was lost.
    pub async fn run(&mut self, sql: &str, kind: Kind, page: &Page) -> Result<Outcome, DbError> {
        if self.spec.read_only {
            read_only_check(sql, self.dialect())?;
        }
        match self.run_once(sql, kind, page).await {
            Err(e) if e.connection_lost => {
                self.reopen().await?;
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
            r => {
                // The Postgres tag is `application_name`: follow a statement that may change it.
                // A failed read (aborted transaction) keeps the old tag: cancel then refuses, never misfires.
                let lower = sql.to_ascii_lowercase();
                if self.dialect() == Dialect::Postgres && ["application_name", "reset", "discard"].iter().any(|w| lower.contains(w)) {
                    let _ = self.read_target().await;
                }
                r
            }
        }
    }

    /// `SELECT count(*)` over the same statement and filters (no paging).
    pub async fn count(&mut self, sql: &str, filters: &[(String, String)]) -> Result<u64, DbError> {
        if let Conn::Es(es) = &self.conn {
            return es.count(sql, filters).await;
        }
        let d = self.dialect();
        let filtered = filters.iter().any(|f| wrap::parse_filter(&f.1).is_some());
        let cols = if filtered || d == Dialect::MySql { self.columns_of(strip(sql)).await? } else { Vec::new() };
        if let Some(c) = wrap::duplicate_column(&cols, d).filter(|_| d == Dialect::MySql || filtered) {
            return Err(DbError::msg(format!("Cannot count: the column name `{c}` repeats; alias the columns of the statement")));
        }
        let w = wrap::build_count(sql, d, filters, &cols);
        let types = (d == Dialect::Postgres).then(|| vec!["int8".to_string()]);
        let raw = self.fetch(&w.sql, &w.params, types.as_deref(), None, 1, false).await.map_err(|e| DbError::from_sqlx(e, w.prefix_chars, w.inner_chars))?;
        match raw.rows.first().and_then(|r| r.first()) {
            Some(Value::Int(n)) => Ok(*n as u64),
            other => Err(DbError::msg(format!("unexpected count result: {other:?}"))),
        }
    }

    async fn run_once(&mut self, sql: &str, kind: Kind, page: &Page) -> Result<Outcome, DbError> {
        if let Conn::Es(es) = &self.conn {
            return es.run(sql, kind, page, self.max_rows).await;
        }
        let started = Instant::now();
        let d = self.dialect();
        let mut notices = Vec::new();
        let mut wrapped = kind == Kind::Query;
        let (exec_sql, params, prefix, len, known, limit) = if wrapped {
            // Columns are needed for filters, and (MySQL) to see repeated names before wrapping.
            let need_cols = wrap::has_filters(page) || !page.sort.is_empty() || (d == Dialect::MySql && !wrap::appends(sql, d, page));
            let cols = if need_cols { self.columns_of(strip(sql)).await? } else { Vec::new() };
            match wrap::duplicate_column(&cols, d).map(str::to_string).filter(|_| need_cols && !wrap::appends(sql, d, page)) {
                // `SELECT * FROM a JOIN b` with two `id`s cannot be a derived table (MySQL) and its
                // filters / sort would be ambiguous: run it as it is, like an unwrappable statement.
                Some(dup) => {
                    wrapped = false;
                    notices.push(format!("The column name `{dup}` repeats, so the statement runs unwrapped: rows are cut at results.max_rows, sorting is local, filters are off. Alias the columns for server-side paging."));
                    (sql.to_string(), Vec::new(), 0, usize::MAX - 1, None, self.max_rows)
                }
                None => {
                    let w = wrap::build(sql, d, page, &cols);
                    (w.sql, w.params, w.prefix_chars, w.inner_chars, (!cols.is_empty()).then_some(cols), page.size as usize + 1)
                }
            }
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
        if wrapped {
            has_more = raw.rows.len() > page.size as usize;
            raw.rows.truncate(page.size as usize);
            raw.truncated = false;
        }
        if raw.extra_sets > 0 {
            notices.push(format!("{} more result set{} not shown", raw.extra_sets, if raw.extra_sets == 1 { "" } else { "s" }));
        }
        if kind == Kind::Other && d == Dialect::MySql {
            notices.extend(self.mysql_warnings().await);
        }
        // MariaDB reports 0 affected rows for `DELETE … RETURNING`: the returned rows are the count.
        if d == Dialect::MySql && raw.affected == 0 {
            raw.affected = raw.rows.len() as u64;
        }
        self.in_tx = tx_state(self.in_tx, sql, d);
        if let Some(on) = autocommit_setting(sql, d) {
            self.autocommit_off = !on;
        }
        Ok(Outcome {
            columns: raw.columns,
            rows: raw.rows,
            wrapped,
            has_more,
            truncated: raw.truncated,
            rows_affected: (kind == Kind::Other).then_some(raw.affected),
            notices,
            executed_sql: exec_sql,
            params,
            in_transaction: self.in_transaction(),
            raw: None,
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
            Conn::Es(_) => Vec::new(),
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
                let (rows, affected, truncated, extra_sets) = drain(stream, limit, |q| q.rows_affected()).await?;
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
                Ok(Raw { columns, rows, affected, truncated, extra_sets })
            }
            Conn::My(c) => {
                let stream = if params.is_empty() {
                    sqlx::raw_sql(AssertSqlSafe(sql.to_string())).fetch_many(&mut *c)
                } else {
                    params.iter().fold(sqlx::query(AssertSqlSafe(sql.to_string())), |q, p| q.bind(p.clone())).fetch(&mut *c).map(|r| r.map(Either::Right)).boxed()
                };
                let (rows, affected, truncated, extra_sets) = drain(stream, limit, |q| q.rows_affected()).await?;
                let columns: Vec<Column> = match rows.first() {
                    Some(r) => r.columns().iter().map(|c| Column { name: c.name().to_string(), type_name: c.type_info().name().to_string() }).collect(),
                    None if describe_empty => match c.prepare(sql_str(sql)).await {
                        Ok(s) => s.columns().iter().map(|c| Column { name: c.name().to_string(), type_name: c.type_info().name().to_string() }).collect(),
                        Err(_) => Vec::new(),
                    },
                    None => Vec::new(),
                };
                let rows = rows.iter().map(|r| columns.iter().enumerate().map(|(i, c)| my_value(r, i, &c.type_name)).collect()).collect();
                Ok(Raw { columns, rows, affected, truncated, extra_sets })
            }
            // `run_once` / `count` answer Elasticsearch before any SQL is built.
            Conn::Es(_) => Ok(Raw { columns: Vec::new(), rows: Vec::new(), affected: 0, truncated: false, extra_sets: 0 }),
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
    /// Result sets with rows after the first one (they are dropped).
    extra_sets: usize,
}

/// Collects the rows of the first result set up to `limit`; a further row sets `truncated` and
/// stops reading (sqlx drains the rest of the result before the connection's next use). Rows of
/// later sets (`CALL p()`, several statements in one run) have other columns and are dropped,
/// the sets are counted.
async fn drain<Q, R>(mut s: impl Stream<Item = SqlxResult<Either<Q, R>>> + Unpin, limit: usize, affected: impl Fn(&Q) -> u64) -> SqlxResult<(Vec<R>, u64, bool, usize)> {
    let (mut rows, mut n, mut truncated) = (Vec::new(), 0, false);
    // `Left` closes a result set. `first_closed`: the first set with rows is over.
    let (mut first_closed, mut in_extra, mut extra) = (false, false, 0);
    while let Some(item) = s.next().await {
        match item? {
            Either::Left(q) => {
                n += affected(&q);
                first_closed |= !rows.is_empty();
                in_extra = false;
            }
            Either::Right(_) if first_closed => {
                if !std::mem::replace(&mut in_extra, true) {
                    extra += 1;
                }
            }
            Either::Right(_) if rows.len() >= limit => {
                truncated = true;
                break;
            }
            Either::Right(r) => rows.push(r),
        }
    }
    Ok((rows, n, truncated, extra))
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
    // A `_bin` collation (MariaDB `JSON` = LONGTEXT utf8mb4_bin, `VARCHAR … COLLATE utf8mb4_bin`) sets the
    // BINARY flag, so sqlx names the column BLOB / VARBINARY; only the `binary` charset is truly binary.
    let textual = <String as sqlx::Type<sqlx::MySql>>::compatible(row.column(i).type_info());
    let r: SqlxResult<Value> = (|| {
        Ok(match ty {
            "BINARY" | "VARBINARY" | "BLOB" | "TINYBLOB" | "MEDIUMBLOB" | "LONGBLOB" if textual => value::mysql_text(row.try_get_unchecked::<String, _>(i)?),
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
            "CHAR" | "VARCHAR" | "TEXT" | "TINYTEXT" | "MEDIUMTEXT" | "LONGTEXT" => value::mysql_text(row.try_get_unchecked::<String, _>(i)?),
            "ENUM" | "SET" => Value::Text(row.try_get_unchecked::<String, _>(i)?),
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

/// `Some(true)` / `Some(false)` when `sql` (MySQL) turns `autocommit` on / off.
pub(crate) fn autocommit_setting(sql: &str, d: Dialect) -> Option<bool> {
    if d != Dialect::MySql {
        return None;
    }
    let mut out = None;
    for st in sql_split::split(sql, d).statements {
        let text = st.text(sql).to_ascii_uppercase();
        let w: Vec<&str> = text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).filter(|w| !w.is_empty()).collect();
        if w.first() != Some(&"SET") {
            continue;
        }
        if let Some(i) = w.iter().position(|x| *x == "AUTOCOMMIT") {
            match w.get(i + 1) {
                Some(&"0" | &"OFF" | &"FALSE") => out = Some(false),
                Some(&"1" | &"ON" | &"TRUE") => out = Some(true),
                _ => {}
            }
        }
    }
    out
}

/// The `read_only` guard: refuses a switch to read-write and everything that writes (by text,
/// see `sql_split::read_only_violation`). Public so a caller can check a whole batch up front.
pub fn read_only_check(sql: &str, d: Dialect) -> Result<(), DbError> {
    let message = if read_write_switch(sql, d) {
        "Read-only connection: the statement switches the session to read-write, which is not allowed. \
         read_only is a safety default, not a security boundary; use a read-only database user for real protection."
            .to_string()
    } else if let Some(m) = sql_split::read_only_violation(sql, d) {
        m
    } else {
        return Ok(());
    };
    Err(DbError { message, refused: true, ..Default::default() })
}

/// A statement that would lift the session's read-only state (the cheap guard behind
/// `read_only`): `BEGIN` / `START TRANSACTION` / `SET … TRANSACTION` `READ WRITE`,
/// `SET [default_]transaction_read_only`, `SET SESSION CHARACTERISTICS … READ WRITE`,
/// `RESET ALL` / `DISCARD ALL`, `set_config('…transaction_read_only', …)`.
pub(crate) fn read_write_switch(sql: &str, d: Dialect) -> bool {
    sql_split::split(sql, d).statements.iter().any(|st| {
        let text = st.text(sql).to_ascii_lowercase();
        let w: Vec<&str> = text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).filter(|w| !w.is_empty()).collect();
        let has = |x: &str| w.contains(&x);
        let read_write = w.windows(2).any(|p| p == ["read", "write"]);
        let ro_var = has("transaction_read_only") || has("default_transaction_read_only") || has("tx_read_only");
        match w.first().copied() {
            Some("begin" | "start") => read_write,
            Some("set") => read_write || ro_var,
            Some("reset") => has("all") || ro_var,
            Some("discard") => has("all"),
            Some("select" | "with") => has("set_config") && ro_var,
            _ => false,
        }
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn socket_hosts_and_default_user() {
        let mut c = Connection { name: "t".into(), driver: Driver::Postgres, url: None, host: None, socket: Some(Socket::Path("/tmp".into())), port: None, database: None, user: None, password: None, password_enc: None, ssl: Default::default(), read_only: false, prod: false, schema: None, init_sql: None, color: None };
        let o = pg_options(&c).unwrap();
        assert!(!matches!(o.get_username(), "" | "anonymous" | "unknown"));
        assert_eq!(Opts::Pg(o).target(), None);
        c.user = Some("bob".into());
        assert_eq!(pg_options(&c).unwrap().get_username(), "bob");
        c.user = None;
        c.url = Some("postgres://alice@h/d".into());
        assert_eq!(pg_options(&c).unwrap().get_username(), "alice");
        let m = Connection { name: "t".into(), driver: Driver::MySql, url: None, host: None, socket: Some(Socket::Path("/tmp/mysql.sock".into())), port: None, database: None, user: None, password: None, password_enc: None, ssl: Default::default(), read_only: false, prod: false, schema: None, init_sql: None, color: None };
        let o = mysql_options(&m).unwrap();
        assert!(o.get_socket().is_some());
        assert_eq!(Opts::My(o).target(), None);
    }

    use super::*;
    use Dialect::{MySql as MY, Postgres as PG};

    #[test]
    fn read_only_guard_refuses_read_write_switches() {
        for (sql, d, want) in [
            ("BEGIN READ WRITE", PG, true),
            ("begin transaction isolation level serializable, read write", PG, true),
            ("START TRANSACTION READ WRITE", MY, true),
            ("SET TRANSACTION READ WRITE", PG, true),
            ("SET SESSION TRANSACTION READ WRITE", MY, true),
            ("SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE", PG, true),
            ("SET default_transaction_read_only = off", PG, true),
            ("set transaction_read_only to off", PG, true),
            ("SET SESSION transaction_read_only = 0", MY, true),
            ("SET @@session.tx_read_only = 0", MY, true),
            ("RESET ALL", PG, true),
            ("RESET default_transaction_read_only", PG, true),
            ("DISCARD ALL", PG, true),
            ("SELECT set_config('default_transaction_read_only', 'off', false)", PG, true),
            ("SELECT 1; BEGIN READ WRITE; DROP TABLE t", PG, true),
            ("BEGIN", PG, false),
            ("BEGIN READ ONLY", PG, false),
            ("START TRANSACTION READ ONLY", MY, false),
            ("SET search_path = a", PG, false),
            ("SET TRANSACTION READ ONLY", PG, false),
            ("RESET search_path", PG, false),
            ("SHOW default_transaction_read_only", PG, false),
            ("SELECT 'read write'", PG, false),
            ("DROP TABLE t", PG, false),
        ] {
            assert_eq!(read_write_switch(sql, d), want, "{sql}");
        }
    }

    #[test]
    fn autocommit_is_tracked() {
        for (sql, want) in [
            ("SET autocommit = 0", Some(false)),
            ("set SESSION autocommit=OFF", Some(false)),
            ("SET @@autocommit = 1", Some(true)),
            ("SET @@session.autocommit = 0", Some(false)),
            ("SET autocommit = 0; SET autocommit = 1", Some(true)),
            ("SET names utf8mb4", None),
            ("SELECT 'autocommit'", None),
        ] {
            assert_eq!(autocommit_setting(sql, MY), want, "{sql}");
        }
        assert_eq!(autocommit_setting("SET autocommit = 0", PG), None);
    }

    #[tokio::test]
    async fn only_the_first_result_set_is_kept() {
        use futures_util::stream;
        let items = |v: Vec<Either<u64, i32>>| stream::iter(v.into_iter().map(Ok::<_, sqlx::Error>));
        // rows 1,2 | rows 3 | rows 4,5 | OK
        let s = items(vec![Either::Right(1), Either::Right(2), Either::Left(0), Either::Right(3), Either::Left(0), Either::Right(4), Either::Right(5), Either::Left(7)]);
        let (rows, n, truncated, extra) = drain(s, 100, |q| *q).await.unwrap();
        assert_eq!((rows, n, truncated, extra), (vec![1, 2], 7, false, 2));
        // One set: nothing extra. A leading statement without rows (DML) is not a result set.
        let s = items(vec![Either::Left(3), Either::Right(1), Either::Left(0)]);
        assert_eq!(drain(s, 100, |q| *q).await.unwrap(), (vec![1], 3, false, 0));
        // The row cap applies to the first set.
        let s = items(vec![Either::Right(1), Either::Right(2), Either::Right(3), Either::Left(0)]);
        assert_eq!(drain(s, 2, |q| *q).await.unwrap(), (vec![1, 2], 0, true, 0));
    }

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
    fn connect_timeout_message() {
        let e = connect_timeout_error(Some("localhost:25432"), Duration::from_millis(10_000));
        assert!(e.connection_lost);
        assert_eq!(
            e.message,
            "No response from localhost:25432 within 10 s. Check the driver and port (Postgres usually listens on 5432, MySQL / MariaDB on 3306, Elasticsearch on 9200)."
        );
        assert!(connect_timeout_error(None, Duration::from_millis(1500)).message.starts_with("No response from the server within 1.5 s."));
    }

    #[test]
    fn errors_map_without_database() {
        let e = DbError::from_sqlx(sqlx::Error::Io(std::io::Error::other("boom")), 0, 0);
        assert!(e.connection_lost);
        let e = DbError::from_sqlx(sqlx::Error::RowNotFound, 0, 0);
        assert!(!e.connection_lost);
    }
}
