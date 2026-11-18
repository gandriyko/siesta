//! Running statements: the tokio runtime, one session per (file, connection), the run pipeline
//! (confirm → session → run → result) and the hooks of the main view (`run_current`, `cancel_run`, …).
//!
//! sqlx needs tokio, GPUI has its own executor: jobs run on `RT`, the GPUI task that awaits a job
//! delivers its result to the view (the same bridge as Respite's `start`). A job is never aborted
//! (a dropped future would leave a connection mid-query): it is cancelled on the server instead.

use std::{
    ops::Range,
    path::{Path, PathBuf},
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use db_conn::Connection;
use db_exec::{CancelHandle, DbError, Outcome, Page, Session};
use gpui::*;
use gpui_component::WindowExt as _;
use siesta_settings::ConfirmDestructive;
use sql_split::{Dialect, Kind};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

use crate::{
    Siesta,
    results::{Done, Key, Tab},
};

/// reqwest-style: sqlx needs tokio; requests run here, results are awaited from GPUI.
pub static RT: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| tokio::runtime::Runtime::new().expect("tokio runtime"));

/// Status dot of the connection picker.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Dot {
    Grey,
    Green,
    Red,
}

/// The databases a connection's server offers, as the database picker shows them.
#[derive(Clone, Debug, PartialEq)]
pub enum DbList {
    Loading,
    Ready(Vec<String>),
    Failed(String),
}

/// (file, connection, database): the database is the effective one (`None` = the server's default).
pub type SlotKey = (PathBuf, String, Option<String>);

/// The session of one (file, connection, database): opened lazily by the first run, closed with the file.
pub struct Slot {
    handles: Handles,
    pub dot: Dot,
    pub in_tx: bool,
}

/// What the tokio side shares with the GUI: the session (runs take its lock, so they go one by
/// one), the cancel handle (usable while a run holds the lock; it follows the session across
/// reconnects) and the generation.
///
/// Every job remembers the generation it was started in. Cancel and close bump it, which
/// invalidates all jobs of the file that are still queued (or mid-batch): they end as "Cancelled"
/// without running more. A job started after the bump is unaffected.
#[derive(Clone)]
pub struct Handles {
    session: Arc<AsyncMutex<Option<Session>>>,
    cancel: Arc<Mutex<Option<CancelHandle>>>,
    generation: Arc<AtomicU64>,
}

impl Handles {
    fn new() -> Self {
        Handles { session: Arc::new(AsyncMutex::new(None)), cancel: Arc::new(Mutex::new(None)), generation: Arc::new(AtomicU64::new(0)) }
    }

    /// The generation a job started now belongs to.
    fn token(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// Cancel / close: every job started so far is invalid.
    fn invalidate(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }

    fn valid(&self, token: u64) -> bool {
        self.token() == token
    }
}

impl Slot {
    pub(crate) fn new() -> Self {
        Slot { handles: Handles::new(), dot: Dot::Grey, in_tx: false }
    }
}

/// What a run that was cancelled before it started reports.
fn cancelled() -> DbError {
    DbError { message: "Cancelled".into(), ..Default::default() }
}

/// One statement to run.
#[derive(Clone, Debug, PartialEq)]
pub struct Spec {
    pub text: String,
    pub kind: Kind,
    pub page_size: Option<usize>,
    /// `-- @confirm`
    pub confirm: bool,
    /// Where the statement was in the editor when it was picked (to place a server error).
    pub range: Option<Range<usize>>,
}

impl Spec {
    pub fn of(st: &sql_split::Statement, src: &str) -> Self {
        Spec { text: st.text(src).to_string(), kind: st.kind, page_size: st.page_size, confirm: st.confirm, range: Some(st.range.clone()) }
    }
}

/// The confirmation dialog of a non-prod connection: `-- @confirm`, and destructive statements when
/// `run.confirm_destructive` is `always`. (A `prod` connection asks for every run, see [`Siesta::start`];
/// the setting's `prod` value therefore adds nothing here.)
pub fn needs_confirm(spec: &Spec, mode: ConfirmDestructive, dialect: Dialect) -> bool {
    spec.confirm || (mode == ConfirmDestructive::Always && sql_split::is_destructive(&spec.text, dialect))
}

/// The statements of the selected text (`range` of `src`), split with the file's dialect.
pub fn selection_specs(src: &str, range: std::ops::Range<usize>, dialect: Dialect) -> Vec<Spec> {
    let Some(text) = src.get(range.clone()) else { return vec![] };
    let shift = |r: Range<usize>| r.start + range.start..r.end + range.start;
    sql_split::split(text, dialect)
        .statements
        .iter()
        .map(|s| Spec { range: Some(shift(s.range.clone())), ..Spec::of(s, text) })
        .collect()
}

#[derive(Clone)]
pub struct Stmt {
    pub text: String,
    pub kind: Kind,
    pub page: Page,
    pub range: Option<Range<usize>>,
    /// A run the user started (goes to history); page / sort / filter re-runs do not.
    pub fresh: bool,
}

#[derive(Clone, Copy)]
struct Opts {
    stop_on_error: bool,
    timeout_ms: Option<u64>,
    connect_timeout_ms: u64,
    max_rows: usize,
}

pub struct Report {
    /// One per executed statement (fewer than asked when the batch stopped).
    pub results: Vec<Result<Outcome, DbError>>,
    pub in_tx: bool,
    /// A session exists (false: it could not be opened).
    pub live: bool,
    /// Cancelled before it started: it ran nothing and says nothing about the session.
    pub skipped: bool,
}

/// The session of `h`, opened if needed, with the settings applied. `Ok(None)`: the job's
/// generation was invalidated while it waited for the lock (cancel / close): nothing is opened.
async fn acquire(h: &Handles, conn: &Connection, opts: Opts, token: u64) -> Result<Option<OwnedMutexGuard<Option<Session>>>, DbError> {
    let mut g = h.session.clone().lock_owned().await;
    if !h.valid(token) {
        return Ok(None);
    }
    if g.is_none() {
        let s = Session::open_with(conn, Duration::from_millis(opts.connect_timeout_ms)).await?;
        // One handle for the life of the session: it follows the session across reconnects.
        *h.cancel.lock().unwrap() = Some(s.cancel_handle());
        *g = Some(s);
    }
    if let Some(s) = g.as_mut() {
        s.max_rows = opts.max_rows;
        s.set_statement_timeout(opts.timeout_ms).await?;
    }
    Ok(Some(g))
}

async fn run_job(h: Handles, conn: Connection, stmts: Vec<Stmt>, opts: Opts, token: u64) -> Report {
    let mut g = match acquire(&h, &conn, opts, token).await {
        Ok(Some(g)) => g,
        Ok(None) => return Report { results: vec![Err(cancelled())], in_tx: false, live: false, skipped: true },
        Err(e) => return Report { results: vec![Err(e)], in_tx: false, live: false, skipped: false },
    };
    let s = g.as_mut().expect("session opened");
    let mut results = vec![];
    for st in stmts {
        if !h.valid(token) {
            break;
        }
        let r = s.run(&st.text, st.kind, &st.page).await;
        let failed = r.is_err();
        results.push(r);
        if failed && opts.stop_on_error {
            break;
        }
    }
    Report { results, in_tx: s.in_transaction(), live: true, skipped: false }
}

async fn count_job(h: Handles, conn: Connection, text: String, filters: Vec<(String, String)>, opts: Opts, token: u64) -> Result<u64, DbError> {
    let mut g = acquire(&h, &conn, opts, token).await?.ok_or_else(cancelled)?;
    g.as_mut().expect("session opened").count(&text, &filters).await
}

/// Reads the structure on the file's session (queued behind a running statement) and saves it
/// to the disk cache.
async fn schema_job(h: Handles, conn: Connection, opts: Opts, file: Option<PathBuf>, token: u64) -> Result<db_exec::Schema, DbError> {
    let mut g = acquire(&h, &conn, opts, token).await?.ok_or_else(cancelled)?;
    let schema = g.as_mut().expect("session opened").schema().await?;
    if let Some(file) = file {
        let copy = schema.clone();
        let _ = tokio::task::spawn_blocking(move || crate::schema_cache::save(&file, &copy)).await;
    }
    Ok(schema)
}

async fn ddl_job(h: Handles, conn: Connection, opts: Opts, schema: String, table: String, token: u64) -> Result<String, DbError> {
    let mut g = acquire(&h, &conn, opts, token).await?.ok_or_else(cancelled)?;
    g.as_mut().expect("session opened").table_ddl(&schema, &table).await
}

/// Where the columns of a statement come from (editing a result), on the file's session.
async fn source_job(h: Handles, conn: Connection, opts: Opts, text: String, token: u64) -> Result<db_exec::Source, DbError> {
    let mut g = acquire(&h, &conn, opts, token).await?.ok_or_else(cancelled)?;
    g.as_mut().expect("session opened").source(&text).await
}

/// What saving edited cells did.
pub struct Applied {
    /// Rows updated, or why nothing (or, inside the user's transaction, not everything) was saved.
    pub result: Result<usize, String>,
    pub in_tx: bool,
    /// The UPDATE statements that ran, for the history.
    pub ran: Vec<(String, Result<Outcome, DbError>)>,
}

/// The UPDATEs of edited cells: each must change exactly one row. Without an open transaction they
/// run in their own (`BEGIN` … `COMMIT`, `ROLLBACK` on the first failure); inside the user's
/// transaction they just run, and committing is the user's.
async fn apply_job(h: Handles, conn: Connection, opts: Opts, stmts: Vec<String>, token: u64) -> Applied {
    let fail = |e: DbError| Applied { result: Err(e.to_string()), in_tx: false, ran: vec![] };
    let mut g = match acquire(&h, &conn, opts, token).await {
        Ok(Some(g)) => g,
        Ok(None) => return fail(cancelled()),
        Err(e) => return fail(e),
    };
    let s = g.as_mut().expect("session opened");
    let none = Page::default();
    let own = !s.in_transaction();
    if own && let Err(e) = s.run("BEGIN", Kind::Other, &none).await {
        return Applied { result: Err(e.to_string()), in_tx: s.in_transaction(), ran: vec![] };
    }
    let mut ran = vec![];
    let mut failure = None;
    for sql in &stmts {
        let r = s.run(sql, Kind::Other, &none).await;
        failure = match &r {
            Err(e) => Some(format!("{}: {e}", one_line(sql))),
            Ok(o) if o.rows_affected != Some(1) => Some(format!("{} changed {} rows, not 1", one_line(sql), o.rows_affected.unwrap_or(0))),
            Ok(_) => None,
        };
        ran.push((sql.clone(), r));
        if failure.is_some() {
            break;
        }
    }
    let result = match (failure, own) {
        (Some(f), true) => {
            let _ = s.run("ROLLBACK", Kind::Other, &none).await;
            Err(format!("{f}. Rolled back: nothing was saved."))
        }
        (Some(f), false) => Err(format!("{f}. The transaction is still open: roll it back, or fix the rest and commit.")),
        (None, true) => s.run("COMMIT", Kind::Other, &none).await.map(|_| stmts.len()).map_err(|e| format!("COMMIT failed: {e}")),
        (None, false) => Ok(stmts.len()),
    };
    Applied { result, in_tx: s.in_transaction(), ran }
}

/// Close the session on the runtime; `finish` (`COMMIT` / `ROLLBACK`) runs first. The handle
/// completes when the session is closed.
fn close_slot(slot: Slot, finish: Option<&'static str>) -> tokio::task::JoinHandle<()> {
    let Slot { handles, .. } = slot;
    handles.invalidate(); // queued runs of this session end as "Cancelled"
    let cancel = handles.cancel.lock().unwrap().clone();
    RT.spawn(async move {
        // A running statement would keep the lock: cancel it first.
        if let Some(c) = cancel.filter(|_| handles.session.try_lock().is_err()) {
            let _ = c.cancel().await;
        }
        if let Some(mut s) = handles.session.lock().await.take() {
            if let Some(sql) = finish {
                let _ = s.run(sql, Kind::Other, &Page::default()).await;
            }
            s.close().await;
        }
    })
}

/// Byte offset of the statement `text` in `src`: at `range` when the text there is still the
/// statement, else its first copy (the file was edited since the run).
fn statement_start(src: &str, text: &str, range: Option<Range<usize>>) -> Option<usize> {
    range.filter(|r| src.get(r.clone()) == Some(text)).map(|r| r.start).or_else(|| src.find(text))
}

/// A formatted statement on one line, for a message.
fn one_line(s: &str) -> String {
    first_line(&s.split_whitespace().collect::<Vec<_>>().join(" "))
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").chars().take(120).collect()
}

impl Siesta {
    fn opts(&self) -> Opts {
        let s = &self.settings.settings;
        Opts { stop_on_error: s.run.stop_on_error, timeout_ms: s.run.statement_timeout_ms, connect_timeout_ms: s.run.connect_timeout_ms, max_rows: s.results.max_rows }
    }

    /// The connection of `path` by name: spec, or why it is unusable.
    pub(crate) fn connection_of(&self, path: &Path) -> Result<(String, Connection), String> {
        let d = self.docs.iter().find(|d| *d.path == path).ok_or("The file is not open")?;
        let name = d.connection().ok_or("No connection: pick one in the top-right corner")?;
        match d.effective_connection() {
            Some(c) => Ok((name.to_string(), c)),
            None => Err(match d.conns.errors.get(name) {
                Some(e) => format!("Connection '{name}' is unusable: {e}"),
                None => format!("Unknown connection '{name}'"),
            }),
        }
    }

    pub(crate) fn dot_of(&self, path: &Path, conn: Option<&str>, database: Option<String>) -> Dot {
        conn.and_then(|c| self.slots.get(&(path.to_path_buf(), c.to_string(), database))).map_or(Dot::Grey, |s| s.dot)
    }

    /// The open transaction of any session of `path`.
    pub(crate) fn in_tx(&self, path: &Path) -> bool {
        self.slots.iter().any(|((p, ..), s)| p == path && s.in_tx)
    }

    /// Closes the sessions of `path` on `conn` that point at another database than `keep`: the file
    /// moved to a different database. Call after the transaction question was answered.
    pub(crate) fn close_other_database_sessions(&mut self, path: &Path, conn: &str, keep: &Option<String>) {
        let keys: Vec<_> = self.slots.keys().filter(|(p, c, db)| p == path && c == conn && db != keep).cloned().collect();
        for slot in keys.into_iter().filter_map(|k| self.slots.remove(&k)) {
            close_slot(slot, None);
        }
    }

    /// An open transaction in a session of `path` on `conn` that is not on database `keep`.
    pub(crate) fn in_tx_elsewhere(&self, path: &Path, conn: &str, keep: &Option<String>) -> bool {
        self.slots.iter().any(|((p, c, db), s)| p == path && c == conn && db != keep && s.in_tx)
    }

    /// Cmd+Enter, double click on a statement, the result panel's "Run": the selection, or the
    /// statement under the cursor.
    pub(crate) fn run_current(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(d) = self.doc().filter(|d| d.sql) else { return };
        let (src, sel) = d.editor.read_with(cx, |e, _| (e.value(), e.selected_range()));
        let specs = if sel.is_empty() { d.active_statement().map(|s| vec![Spec::of(s, &src)]).unwrap_or_default() } else { selection_specs(&src, sel, d.dialect) };
        self.start((*d.path).clone(), specs, window, cx);
    }

    /// Cmd+Shift+Enter: run every statement of the current file in order.
    pub(crate) fn run_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(d) = self.doc().filter(|d| d.sql) else { return };
        let src = d.editor.read(cx).value();
        let specs = d.file.statements.iter().map(|s| Spec::of(s, &src)).collect();
        self.start((*d.path).clone(), specs, window, cx);
    }

    /// Cmd+Alt+L: run the last statement(s) again, in whatever file they were.
    pub(crate) fn repeat_last(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.store.last.clone() {
            Some((path, specs)) => self.start(path, specs, window, cx),
            None => window.push_notification("Nothing to repeat yet", cx),
        }
    }

    /// Esc / Cmd+.: cancel what the current file is running (the result panel's "Stop").
    pub(crate) fn cancel_run(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(d) = self.doc() else { return };
        let path = (*d.path).clone();
        if !self.store.entries.iter().any(|(k, e)| k.0 == path && e.running.is_some()) {
            return;
        }
        for ((p, ..), slot) in &self.slots {
            if *p != path {
                continue;
            }
            // Queued runs of the file die with it; the cancel handle hits the one that executes.
            slot.handles.invalidate();
            if let Some(c) = slot.handles.cancel.lock().unwrap().clone() {
                RT.spawn(async move {
                    let _ = c.cancel().await;
                });
            }
        }
        cx.notify();
    }

    /// "Test connection": a throwaway session, opened and closed.
    pub(crate) fn test_connection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.doc().map(|d| (*d.path).clone()) else { return };
        let (name, conn) = match self.connection_of(&path) {
            Ok(c) => c,
            Err(e) => return window.push_notification(e, cx),
        };
        let timeout = Duration::from_millis(self.settings.settings.run.connect_timeout_ms);
        let handle = RT.spawn(async move {
            let t = Instant::now();
            let s = Session::open_with(&conn, timeout).await?;
            s.close().await;
            Ok::<_, DbError>(t.elapsed())
        });
        cx.spawn_in(window, async move |_, cx| {
            let msg = match handle.await {
                Ok(Ok(t)) => format!("Connection '{name}' works ({} ms)", t.as_millis()),
                Ok(Err(e)) => format!("Connection '{name}' failed: {e}"),
                Err(e) => format!("Connection '{name}' failed: {e}"),
            };
            cx.update(|window, cx| window.push_notification(msg, cx)).ok();
        })
        .detach();
    }

    /// "Disconnect": drop the sessions of the current file; an open transaction is asked about
    /// (Commit / Rollback / Cancel) like when the file is closed.
    pub(crate) fn disconnect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.doc().map(|d| (*d.path).clone()) else { return };
        let done = |window: &mut Window, cx: &mut Context<Self>| {
            window.push_notification("Disconnected", cx);
            cx.notify();
        };
        if self.in_tx(&path) {
            return self.ask_transaction(path, move |_, window, cx| done(window, cx), window, cx);
        }
        self.close_sessions(&path, None);
        done(window, cx);
    }

    /// Close every session of `path` (`finish` runs first); the handles complete when they are closed.
    pub(crate) fn close_sessions(&mut self, path: &Path, finish: Option<&'static str>) -> Vec<tokio::task::JoinHandle<()>> {
        let keys: Vec<_> = self.slots.keys().filter(|(p, ..)| p == path).cloned().collect();
        keys.into_iter().filter_map(|k| self.slots.remove(&k)).map(|slot| close_slot(slot, finish)).collect()
    }

    /// Close the sessions of connection `name` in every file (its credentials changed); an open
    /// transaction goes with the session.
    pub(crate) fn close_connection_sessions(&mut self, name: &str) -> Vec<tokio::task::JoinHandle<()>> {
        let keys: Vec<_> = self.slots.keys().filter(|(_, n, _)| n == name).cloned().collect();
        keys.into_iter().filter_map(|k| self.slots.remove(&k)).map(|slot| close_slot(slot, None)).collect()
    }

    /// Paths with an open transaction in any session.
    pub(crate) fn paths_in_tx(&self) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = self.slots.iter().filter(|(_, s)| s.in_tx).map(|((p, ..), _)| p.clone()).collect();
        v.sort();
        v.dedup();
        v
    }

    /// Resolve the pipeline up to the confirmation, then run.
    pub(crate) fn start(&mut self, path: PathBuf, specs: Vec<Spec>, window: &mut Window, cx: &mut Context<Self>) {
        if specs.is_empty() {
            return window.push_notification("Nothing to run", cx);
        }
        let (name, conn) = match self.connection_of(&path) {
            Ok(c) => c,
            Err(e) => return window.push_notification(e, cx),
        };
        let dialect = crate::doc::dialect_of(conn.driver);
        // A prod connection asks for every non-read statement, one dialog listing the whole batch. Page, sort,
        // filter and "Count all" re-run an already confirmed result and do not come through here.
        let mode = self.settings.settings.run.confirm_destructive;
        // A statement the read-only guard refuses is never asked about: it fails in the result panel.
        let refused = |s: &Spec| conn.read_only && db_exec::read_only_check(&s.text, dialect).is_err();
        let ask: Vec<&Spec> = specs.iter().filter(|s| !refused(s) && ((conn.prod && s.kind.needs_prod_confirm()) || needs_confirm(s, mode, dialect))).collect();
        if ask.is_empty() {
            return self.execute(path, name, conn, specs, window, cx);
        }
        let what = if ask.len() == 1 { "this statement".to_string() } else { format!("these {} statements", ask.len()) };
        let message = if conn.prod { format!("Run {what} on PRODUCTION {name}?") } else { format!("Run {what} on {name}?") };
        let detail = ask.iter().map(|s| s.text.trim()).collect::<Vec<_>>().join("\n\n");
        let answer = window.prompt(PromptLevel::Warning, &message, Some(&detail), &["Run", "Cancel"], cx);
        cx.spawn_in(window, async move |this, cx| {
            if answer.await == Ok(0) {
                this.update_in(cx, |this, window, cx| this.execute(path, name, conn, specs, window, cx)).ok();
            }
        })
        .detach();
    }

    fn execute(&mut self, path: PathBuf, name: String, conn: Connection, specs: Vec<Spec>, window: &mut Window, cx: &mut Context<Self>) {
        let default_size = self.settings.settings.results.page_size.max(1);
        let (mut stmts, mut keys) = (vec![], vec![]);
        for s in &specs {
            let key: Key = (path.clone(), s.text.clone());
            let e = self.store.entries.entry(key.clone()).or_default();
            // A fresh run starts from the first page, unsorted and unfiltered.
            e.page = Page { size: s.page_size.unwrap_or(default_size).max(1) as u64, ..Page::default() };
            e.count = None;
            e.running = Some(Instant::now());
            e.tab = Tab::Result;
            stmts.push(Stmt { text: s.text.clone(), kind: s.kind, page: e.page.clone(), range: s.range.clone(), fresh: true });
            keys.push(key);
        }
        self.store.batches.insert(path.clone(), if keys.len() > 1 { keys.clone() } else { vec![] });
        self.store.last = Some((path.clone(), specs));
        self.spawn_job(path, name, conn, stmts, keys, window, cx);
    }

    /// Page, sort and filter changes: the statement again with its page, in the same session.
    pub(crate) fn rerun(&mut self, key: &Key, window: &mut Window, cx: &mut Context<Self>) {
        let Some(e) = self.store.entries.get_mut(key) else { return };
        let Some(done) = e.done.clone() else { return };
        let Ok((_, conn)) = self.connection_of(&key.0) else { return };
        // The connection of that result, not necessarily the picked one now.
        let conn = self.docs.iter().find(|d| *d.path == key.0).and_then(|d| d.conns.connections.get(&done.conn).map(|c| c.with_database(done.database.as_deref()))).unwrap_or(conn);
        let e = self.store.entries.get_mut(key).expect("entry");
        e.running = Some(Instant::now());
        let stmt = Stmt { text: done.text.clone(), kind: done.kind, page: e.page.clone(), range: done.range.clone(), fresh: false };
        self.spawn_job(key.0.clone(), done.conn.clone(), conn, vec![stmt], vec![key.clone()], window, cx);
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_job(&mut self, path: PathBuf, name: String, conn: Connection, stmts: Vec<Stmt>, keys: Vec<Key>, window: &mut Window, cx: &mut Context<Self>) {
        let slot = self.slots.entry((path.clone(), name.clone(), conn.database.clone())).or_insert_with(Slot::new);
        let handles = slot.handles.clone();
        let opts = self.opts();
        let token = handles.token(); // taken here, not on the runtime: a Cancel right after this must reach the job
        let handle = RT.spawn(run_job(handles, conn.clone(), stmts.clone(), opts, token));
        self.ensure_ticker(window, cx);
        cx.spawn_in(window, async move |this, cx| {
            let report = match handle.await {
                Ok(r) => r,
                Err(e) => Report { results: vec![Err(DbError { message: format!("Internal error: {e}"), ..Default::default() })], in_tx: false, live: false, skipped: true },
            };
            this.update_in(cx, |this, window, cx| this.finish(path, name, conn, stmts, keys, report, window, cx)).ok();
        })
        .detach();
        cx.notify();
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn finish(&mut self, path: PathBuf, name: String, conn: Connection, stmts: Vec<Stmt>, keys: Vec<Key>, report: Report, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(slot) = self.slots.get_mut(&(path.clone(), name.clone(), conn.database.clone()))
            && !report.skipped
        {
            slot.in_tx = report.in_tx;
            slot.dot = if report.live && report.results.last().is_some_and(Result::is_ok) { Dot::Green } else { Dot::Red };
        }
        let color = conn.display_color().map(str::to_string);
        // DDL that worked leaves the cached schema stale.
        let dialect = crate::doc::dialect_of(conn.driver);
        let schema_changed = stmts.iter().zip(&report.results).any(|(s, r)| r.is_ok() && sql_split::is_schema_changing(&s.text, dialect));
        let mut position = None;
        let src = self.docs.iter().find(|d| *d.path == path).map(|d| d.editor.read(cx).value().to_string());
        let mut history = vec![];
        for (i, (stmt, key)) in stmts.into_iter().zip(keys).enumerate() {
            let result = report.results.get(i).cloned();
            if let (Some(r), true, false) = (&result, stmt.fresh, report.skipped) {
                let start = src.as_deref().and_then(|s| statement_start(s, &stmt.text, stmt.range.clone()).map(|at| s[..at].matches('\n').count() + 1));
                history.push(db_exec::history::Entry {
                    source: "gui".into(),
                    line: start.unwrap_or(0),
                    connection: name.clone(),
                    database: conn.database.clone(),
                    sql: stmt.text.clone(),
                    duration_ms: r.as_ref().map_or(0, |o| o.duration.as_millis() as u64),
                    rows: r.as_ref().ok().filter(|o| o.rows_affected.is_none()).map(|o| o.rows.len() as u64),
                    affected: r.as_ref().ok().and_then(|o| o.rows_affected),
                    error: r.as_ref().err().map(|e| e.to_string()),
                    ..Default::default()
                });
            }
            let Some(e) = self.store.entries.get_mut(&key) else { continue };
            e.running = None;
            let Some(result) = result else { continue };
            if let Err(err) = &result
                && let Some(p) = err.position
            {
                position = Some((stmt.text.clone(), stmt.range.clone(), p, err.message.clone()));
            }
            e.id += 1;
            e.done = Some(Arc::new(Done { text: stmt.text, kind: stmt.kind, page: stmt.page, range: stmt.range, conn: name.clone(), database: conn.database.clone(), color: color.clone(), driver: conn.driver, result }));
        }
        self.record_history(&path, history, cx);
        if let Some((text, range, p, msg)) = position {
            self.underline_error(&path, &text, range, p, &msg, cx);
        }
        // ponytail: skipped while an earlier read is still in flight (it may predate the DDL); "Refresh" fixes it.
        if schema_changed {
            self.load_schema(path.clone(), name.clone(), conn.clone(), window, cx);
        }
        if report.results.iter().any(|r| r.as_ref().is_err_and(|e| e.connection_lost)) {
            window.push_notification("Connection was lost and reopened", cx);
        }
        cx.notify();
    }

    /// A server error with a position: red underline at that character until the next edit
    /// (the editor drops diagnostics on every change). `range` is where the statement was in the
    /// editor when it ran; if the text there changed since, the first copy of the statement is used.
    pub(crate) fn underline_error(&mut self, path: &Path, text: &str, range: Option<Range<usize>>, pos: usize, msg: &str, cx: &mut Context<Self>) {
        let Some(d) = self.docs.iter().find(|d| *d.path == path) else { return };
        d.editor.update(cx, |s, cx| {
            let src = s.value();
            let Some(start) = statement_start(&src, text, range) else { return };
            let at = |n: usize| text.char_indices().nth(n).map_or(text.len(), |(b, _)| b);
            let (a, b) = (at(pos.saturating_sub(1)), at(pos));
            // "At end of input": the last character is the culprit.
            let (a, b) = if a == b { (text[..a].char_indices().next_back().map_or(0, |(i, _)| i), a) } else { (a, b) };
            // A word is underlined whole.
            let b = if text[a..b].chars().all(|c| c.is_alphanumeric() || c == '_') { a + text[a..].find(|c: char| !(c.is_alphanumeric() || c == '_')).unwrap_or(text.len() - a) } else { b };
            let rope = s.text().clone();
            use gpui_component::input::RopeExt as _;
            let d = lsp_types::Diagnostic {
                range: lsp_types::Range::new(rope.offset_to_position(start + a), rope.offset_to_position(start + b.max(a))),
                severity: Some(lsp_types::DiagnosticSeverity::ERROR),
                source: Some("server".into()),
                message: first_line(msg),
                ..Default::default()
            };
            if let Some(set) = s.diagnostics_mut() {
                set.extend([d]);
                cx.notify();
            }
        });
    }

    /// Ticks the header's `Running… 3.2 s` while anything runs.
    fn ensure_ticker(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.ticking {
            return;
        }
        self.ticking = true;
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(Duration::from_millis(100)).await;
                let more = this
                    .update(cx, |this, cx| {
                        let any = this.store.entries.values().any(|e| e.running.is_some());
                        if any {
                            cx.notify();
                        } else {
                            this.ticking = false;
                        }
                        any
                    })
                    .unwrap_or(false);
                if !more {
                    break;
                }
            }
        })
        .detach();
    }

    /// The session of a result's (file, connection, database), as a job runs on it.
    fn handles_of(&mut self, path: &Path, name: &str, conn: &Connection) -> (Handles, u64) {
        let handles = self.slots.entry((path.to_path_buf(), name.to_string(), conn.database.clone())).or_insert_with(Slot::new).handles.clone();
        let token = handles.token();
        (handles, token)
    }

    /// Edit mode: where the columns of `text` come from, on the file's session.
    pub(crate) fn source_job(&mut self, path: &Path, name: &str, conn: Connection, text: String) -> tokio::task::JoinHandle<Result<db_exec::Source, DbError>> {
        let (h, token) = self.handles_of(path, name, &conn);
        RT.spawn(source_job(h, conn, self.opts(), text, token))
    }

    /// Edit mode: run the confirmed UPDATEs on the file's session.
    pub(crate) fn apply_job(&mut self, path: &Path, name: &str, conn: Connection, stmts: Vec<String>) -> tokio::task::JoinHandle<Applied> {
        let (h, token) = self.handles_of(path, name, &conn);
        RT.spawn(apply_job(h, conn, self.opts(), stmts, token))
    }

    /// "Count all": `SELECT count(*)` over the statement and its filters, on demand.
    pub(crate) fn count_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = self.shown_key(cx) else { return };
        let Some(e) = self.store.entries.get(&key) else { return };
        let Some(done) = e.done.clone() else { return };
        let filters = e.page.filters.clone();
        let Some(conn) = self.docs.iter().find(|d| *d.path == key.0).and_then(|d| d.conns.connections.get(&done.conn).map(|c| c.with_database(done.database.as_deref()))) else { return };
        let slot = self.slots.entry((key.0.clone(), done.conn.clone(), conn.database.clone())).or_insert_with(Slot::new);
        let token = slot.handles.token();
        let handle = RT.spawn(count_job(slot.handles.clone(), conn, done.text.clone(), filters.clone(), self.opts(), token));
        cx.spawn_in(window, async move |this, cx| {
            let r = handle.await.unwrap_or_else(|e| Err(DbError { message: e.to_string(), ..Default::default() }));
            this.update_in(cx, |this, window, cx| {
                match r {
                    Ok(n) => {
                        if let Some(e) = this.store.entries.get_mut(&key) {
                            e.count = Some((filters, n));
                        }
                    }
                    Err(err) => window.push_notification(format!("Count failed: {err}"), cx),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The disk cache of the connection with cache `key` (`schema_cache::key`), read once, so the
    /// DB tab and completion work before any session.
    pub(crate) fn load_cached_schema(&mut self, key: &str) {
        let Some(base) = self.base.as_deref() else { return };
        let mut cache = self.schemas.borrow_mut();
        if cache.schemas.contains_key(key) || !cache.disk_tried.insert(key.to_string()) {
            return;
        }
        if let Some(s) = crate::schema_cache::load(&crate::schema_cache::path(base, key)) {
            cache.schemas.insert(key.to_string(), Arc::new(s));
        }
    }

    /// A connection was just picked (or restored) for `path`: read its schema in the background,
    /// unless this run of the app already has it. Opens the file's session for that. The server's
    /// database list is read too (once per connection), so the database picker has it when opened.
    pub(crate) fn autoload_schema(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        if !self.docs.iter().any(|d| *d.path == path && d.sql) {
            return;
        }
        let Ok((name, conn)) = self.connection_of(path) else { return };
        if !self.schemas.borrow().fresh.contains(&crate::schema_cache::key(&conn)) {
            self.load_schema(path.to_path_buf(), name, conn, window, cx);
        }
        self.fetch_databases_of(path, false, window, cx);
    }

    /// The database picker was opened: read the current file's server list unless it is known.
    pub(crate) fn fetch_databases(&mut self, force: bool, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(path) = self.doc().map(|d| (*d.path).clone()) {
            self.fetch_databases_of(&path, force, window, cx);
        }
    }

    pub(crate) fn refresh_databases(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.fetch_databases(true, window, cx);
    }

    /// Reads the databases of `path`'s connection on a throwaway session (the file's own sessions
    /// are per database). Cached per connection for the life of the app; `force` reads again.
    fn fetch_databases_of(&mut self, path: &Path, force: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(base) = self.docs.iter().find(|d| *d.path == path && d.sql).and_then(|d| d.connection().and_then(|n| d.conns.connections.get(n)).cloned()) else { return };
        let key = crate::schema_cache::key(&base);
        match self.db_lists.get(&key) {
            Some(DbList::Loading) => return,
            Some(DbList::Ready(_)) if !force => return,
            _ => {}
        }
        self.db_lists.insert(key.clone(), DbList::Loading);
        let timeout = Duration::from_millis(self.settings.settings.run.connect_timeout_ms);
        let handle = RT.spawn(async move {
            db_exec::list_databases(&base, timeout).await
        });
        cx.spawn_in(window, async move |this, cx| {
            let r = handle.await.unwrap_or_else(|e| Err(DbError { message: format!("Internal error: {e}"), ..Default::default() }));
            let rebuilt = this
                .update(cx, |this, cx| {
                    this.db_lists.insert(key, r.map_or_else(|e| DbList::Failed(e.to_string()), DbList::Ready));
                    cx.notify();
                    this.db_menu_builder(cx.entity().downgrade()).zip(this.db_menu.borrow().clone())
                })
                .ok()
                .flatten();
            // The picker may be open: refill its menu with the new state (outside the view's update).
            if let Some((build, menu)) = rebuilt {
                cx.update(|window, cx| {
                    _ = menu.update(cx, |m, cx| m.rebuild(window, cx, |m, w, c| build(m, w, c)));
                })
                .ok();
            }
        })
        .detach();
        cx.notify();
    }

    /// "Refresh schema": read the current file's connection again (opens its session if needed).
    pub(crate) fn refresh_schema(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.doc().map(|d| (*d.path).clone()) else { return };
        match self.connection_of(&path) {
            Ok((name, conn)) => self.load_schema(path, name, conn, window, cx),
            Err(e) => window.push_notification(e, cx),
        }
    }

    /// Reads the schema in the background; the old one (cache) stays until the new one arrives.
    fn load_schema(&mut self, path: PathBuf, name: String, conn: Connection, window: &mut Window, cx: &mut Context<Self>) {
        // Keyed by what the connection points at, not by its name (two files may share a name).
        let key = crate::schema_cache::key(&conn);
        {
            let mut c = self.schemas.borrow_mut();
            if !c.loading.insert(key.clone()) {
                return;
            }
            c.errors.remove(&key);
        }
        let handles = self.slots.entry((path, name, conn.database.clone())).or_insert_with(Slot::new).handles.clone();
        let file = self.base.as_deref().map(|b| crate::schema_cache::path(b, &key));
        let token = handles.token();
        let handle = RT.spawn(schema_job(handles, conn, self.opts(), file, token));
        cx.spawn_in(window, async move |this, cx| {
            let r = handle.await.unwrap_or_else(|e| Err(DbError { message: format!("Internal error: {e}"), ..Default::default() }));
            this.update(cx, |this, cx| {
                let mut c = this.schemas.borrow_mut();
                c.loading.remove(&key);
                match r {
                    Ok(s) => {
                        c.fresh.insert(key.clone());
                        c.schemas.insert(key, Arc::new(s));
                    }
                    Err(e) => {
                        c.errors.insert(key, e.to_string());
                    }
                }
                drop(c);
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    /// "Copy DDL" of a table of the current file's connection, to the clipboard.
    pub(crate) fn copy_ddl(&mut self, schema: &str, table: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.doc().map(|d| (*d.path).clone()) else { return };
        let (name, conn) = match self.connection_of(&path) {
            Ok(c) => c,
            Err(e) => return window.push_notification(e, cx),
        };
        let handles = self.slots.entry((path, name, conn.database.clone())).or_insert_with(Slot::new).handles.clone();
        let token = handles.token();
        let handle = RT.spawn(ddl_job(handles, conn, self.opts(), schema.to_string(), table.to_string(), token));
        let table = table.to_string();
        cx.spawn_in(window, async move |_, cx| {
            let r = handle.await.unwrap_or_else(|e| Err(DbError { message: format!("Internal error: {e}"), ..Default::default() }));
            cx.update(|window, cx| match r {
                Ok(ddl) => {
                    cx.write_to_clipboard(ClipboardItem::new_string(ddl));
                    window.push_notification(format!("DDL of {table} copied"), cx);
                }
                Err(e) => window.push_notification(format!("DDL of {table}: {e}"), cx),
            })
            .ok();
        })
        .detach();
    }

    /// A file with an open transaction is about to lose its session (close, Disconnect): Commit /
    /// Rollback / Cancel. Commit and Rollback close the sessions with that statement and then run
    /// `then`; Cancel runs nothing.
    pub(crate) fn ask_transaction(&mut self, path: PathBuf, then: impl FnOnce(&mut Self, &mut Window, &mut Context<Self>) + 'static, window: &mut Window, cx: &mut Context<Self>) {
        let name = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("{name} has an open transaction"),
            Some("Commit or roll it back before the session closes?"),
            &["Commit", "Rollback", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            let Ok(answer) = answer.await else { return };
            this.update_in(cx, |this, window, cx| {
                let Some(finish) = finish_statement(answer) else { return };
                this.close_sessions(&path, Some(finish));
                then(this, window, cx);
            })
            .ok();
        })
        .detach();
    }

    /// Quit with open transactions: one Commit / Rollback / Cancel for all of them. The sessions
    /// are closed (and the statement has run) before the app exits.
    pub(crate) fn quit_checked(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let open = self.paths_in_tx();
        if open.is_empty() {
            return cx.quit();
        }
        let names: Vec<String> = open.iter().map(|p| p.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned())).collect();
        let message = match names.len() {
            1 => format!("{} has an open transaction", names[0]),
            n => format!("{n} files have an open transaction"),
        };
        let detail = (names.len() > 1).then(|| names.join("\n"));
        let answer = window.prompt(PromptLevel::Warning, &message, detail.as_deref().or(Some("Commit or roll it back before quitting?")), &["Commit", "Rollback", "Cancel"], cx);
        cx.spawn_in(window, async move |this, cx| {
            let Ok(answer) = answer.await else { return };
            let Some(finish) = finish_statement(answer) else { return };
            let Ok(closing) = this.update(cx, |this, _| open.iter().flat_map(|p| this.close_sessions(p, Some(finish))).collect::<Vec<_>>()) else { return };
            // Wait (at most a few seconds) for COMMIT / ROLLBACK to reach the server.
            let wait = RT.spawn(async move {
                for h in closing {
                    let _ = tokio::time::timeout(Duration::from_secs(5), h).await;
                }
            });
            let _ = wait.await;
            this.update(cx, |_, cx| cx.quit()).ok();
        })
        .detach();
    }
}

/// The prompt's answer (`Commit`, `Rollback`, `Cancel`) as the statement that ends the transaction.
fn finish_statement(answer: usize) -> Option<&'static str> {
    match answer {
        0 => Some("COMMIT"),
        1 => Some("ROLLBACK"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: it brings `gpui::test`, which shadows `#[test]`.
    use super::{Spec, needs_confirm, selection_specs};
    // (the modules `super::Handles` etc. are named where used)
    use siesta_settings::ConfirmDestructive;
    use sql_split::{Dialect, Kind};

    fn spec(text: &str, confirm: bool) -> Spec {
        Spec { text: text.into(), kind: Kind::Other, page_size: None, confirm, range: None }
    }

    fn fake_conn() -> db_conn::Connection {
        // Nothing listens there: a job that tried to connect would fail loudly.
        db_conn::Connection {
            name: "t".into(),
            driver: db_conn::Driver::Postgres,
            url: Some("postgres://nobody@127.0.0.1:1/none".into()),
            host: None,
            socket: None,
            port: None,
            database: None,
            user: None,
            password: None,
            password_enc: None,
            ssl: Default::default(),
            read_only: false,
            prod: false,
            schema: None,
            init_sql: None,
            color: None,
        }
    }

    #[test]
    fn cancel_invalidates_queued_runs_without_running_them() {
        use super::{Handles, Opts, Stmt, run_job};
        let (h, rt) = (Handles::new(), &*super::RT);
        let opts = Opts { stop_on_error: true, timeout_ms: None, connect_timeout_ms: 10_000, max_rows: 10 };
        let stmts = || vec![Stmt { text: "SELECT 1".into(), kind: Kind::Query, page: Default::default(), range: None, fresh: false }];
        // A run holds the session; a second one queues behind it.
        let running = rt.block_on(h.session.clone().lock_owned());
        let queued = rt.spawn(run_job(h.clone(), fake_conn(), stmts(), opts, h.token()));
        // Esc (or closing the file) while it waits, then the first run ends.
        h.invalidate();
        drop(running);
        let r = rt.block_on(queued).unwrap();
        assert!(r.skipped && !r.live, "the queued run must not execute (or even connect)");
        assert_eq!(r.results.len(), 1);
        assert_eq!(r.results[0].as_ref().unwrap_err().message, "Cancelled");
        assert!(h.session.try_lock().unwrap().is_none(), "no session was opened for it");
        // A run started after the cancel is a new generation.
        assert!(h.valid(h.token()));
        let r = rt.block_on(run_job(h.clone(), fake_conn(), stmts(), opts, h.token()));
        assert!(!r.skipped, "a fresh run is not cancelled: it tries to connect (and fails here)");
    }

    #[test]
    fn transaction_prompt_answers() {
        use super::finish_statement;
        // Commit / Rollback end the transaction before the session closes; Cancel (and a dismissed prompt) keep everything.
        assert_eq!([finish_statement(0), finish_statement(1), finish_statement(2), finish_statement(9)], [Some("COMMIT"), Some("ROLLBACK"), None, None]);
    }

    #[test]
    fn error_is_underlined_at_its_own_statement() {
        use super::statement_start;
        let src = "SELECT 1;\nSELEC 2;\nSELEC 2;\n";
        let (a, b) = (10..17, 19..26);
        assert_eq!(&src[a.clone()], "SELEC 2");
        assert_eq!(statement_start(src, "SELEC 2", Some(b.clone())), Some(19)); // the second copy
        assert_eq!(statement_start(src, "SELEC 2", Some(a)), Some(10));
        // The text moved since the run: the first copy is the best guess.
        assert_eq!(statement_start(src, "SELEC 2", Some(0..7)), Some(10));
        assert_eq!(statement_start(src, "SELEC 2", None), Some(10));
        assert_eq!(statement_start(src, "nope", Some(b)), None);
    }

    #[test]
    fn confirmation_rules() {
        use ConfirmDestructive::*;
        let (del, sel) = (spec("DELETE FROM t", false), spec("SELECT 1", false));
        let d = Dialect::Postgres;
        // (A prod connection asks for everything before this is even consulted.)
        assert!(!needs_confirm(&del, Prod, d));
        assert!(needs_confirm(&del, Always, d));
        assert!(!needs_confirm(&del, Never, d));
        assert!(!needs_confirm(&sel, Always, d));
        assert!(needs_confirm(&spec("SELECT 1", true), Never, d)); // `-- @confirm` always asks
        assert!(!needs_confirm(&spec("DELETE FROM t WHERE id = 1", false), Always, d));
    }

    /// The job pipeline against a real Postgres: `scripts/test-db.sh` sets `SIESTA_TEST_PG_URL`.
    #[test]
    #[ignore = "needs Postgres (scripts/test-db.sh)"]
    fn jobs_against_postgres() {
        use super::{Opts, Slot, Stmt, count_job, run_job};
        let url = std::env::var("SIESTA_TEST_PG_URL").expect("SIESTA_TEST_PG_URL");
        let conn = db_conn::Connection {
            name: "t".into(),
            driver: db_conn::Driver::Postgres,
            url: Some(url),
            host: None,
            socket: None,
            port: None,
            database: None,
            user: None,
            password: None,
            password_enc: None,
            ssl: Default::default(),
            read_only: false,
            prod: false,
            schema: None,
            init_sql: None,
            color: None,
        };
        let opts = Opts { stop_on_error: true, timeout_ms: Some(5000), connect_timeout_ms: 10_000, max_rows: 1000 };
        let stmt = |text: &str, kind| Stmt { text: text.into(), kind, page: db_exec::Page { size: 2, ..Default::default() }, range: None, fresh: false };
        let slot = Slot::new();
        let h = slot.handles.clone();
        let rt = &*super::RT;
        // Statement timeout from settings, two pages of rows, then a failing statement that stops the batch.
        let r = rt.block_on(run_job(
            h.clone(),
            conn.clone(),
            vec![stmt("SHOW statement_timeout", Kind::Read), stmt("SELECT g FROM generate_series(1, 5) g", Kind::Query), stmt("SELEC 1", Kind::Other), stmt("SELECT 1", Kind::Query)],
            opts,
            h.token(),
        ));
        assert!(r.live);
        assert_eq!(r.results.len(), 3); // stopped at the error
        assert_eq!(r.results[0].as_ref().unwrap().rows[0][0].display(), "5s");
        let page = r.results[1].as_ref().unwrap();
        assert_eq!((page.rows.len(), page.has_more), (2, true));
        assert_eq!(r.results[2].as_ref().unwrap_err().position, Some(1)); // `SELEC` is at character 1
        // The same session keeps its state; count runs on it too.
        let n = rt.block_on(count_job(h.clone(), conn.clone(), "SELECT g FROM generate_series(1, 5) g".into(), vec![("g".into(), ">2".into())], opts, h.token()));
        assert_eq!(n.unwrap(), 3);
        // Cancel from outside while a statement runs: it fails with 57014 and the session survives.
        let cancel = h.cancel.lock().unwrap().clone().unwrap();
        let slow = rt.spawn(run_job(h.clone(), conn.clone(), vec![stmt("SELECT pg_sleep(4)", Kind::Other)], Opts { timeout_ms: None, ..opts }, h.token()));
        rt.block_on(async {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            cancel.cancel().await.unwrap();
        });
        let r = rt.block_on(slow).unwrap();
        assert_eq!(r.results[0].as_ref().unwrap_err().code.as_deref(), Some("57014"));
        let r = rt.block_on(run_job(h.clone(), conn.clone(), vec![stmt("SELECT 1", Kind::Query)], opts, h.token()));
        assert!(r.results[0].is_ok());
        // Esc while a batch runs: the rest of it is not executed.
        let batch = rt.spawn(run_job(h.clone(), conn.clone(), vec![stmt("SELECT pg_sleep(1)", Kind::Other), stmt("SELECT 2", Kind::Query)], Opts { timeout_ms: None, ..opts }, h.token()));
        rt.block_on(async {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        });
        h.invalidate();
        let r = rt.block_on(batch).unwrap();
        assert_eq!(r.results.len(), 1);

        // Editing a result: the table and key of the columns, then the confirmed UPDATEs in one
        // transaction, rolled back when one of them misses its row.
        use super::{apply_job, source_job};
        let setup = ["DROP TABLE IF EXISTS siesta_edit_t", "CREATE TABLE siesta_edit_t (id int PRIMARY KEY, note text)", "INSERT INTO siesta_edit_t VALUES (1, 'a'), (2, 'b')"];
        let r = rt.block_on(run_job(h.clone(), conn.clone(), setup.iter().map(|s| stmt(s, Kind::Other)).collect(), opts, h.token()));
        assert!(r.results.iter().all(Result::is_ok), "{:?}", r.results);
        let src = rt.block_on(source_job(h.clone(), conn.clone(), opts, "SELECT note, id FROM siesta_edit_t".into(), h.token())).unwrap();
        let cols = [db_exec::Column { name: "note".into(), type_name: "text".into() }, db_exec::Column { name: "id".into(), type_name: "int4".into() }];
        let target = crate::edit::plan(&cols, &src).unwrap();
        let rows = vec![vec![db_exec::Value::Text("a".into()), db_exec::Value::Int(1)], vec![db_exec::Value::Text("b".into()), db_exec::Value::Int(2)]];
        let mut changes = crate::edit::Changes::new();
        changes.insert((0, 0), crate::edit::NewValue::text("A"));
        changes.insert((1, 0), None);
        let sql = crate::edit::update_sql(Dialect::Postgres, &target, &rows, &changes).unwrap();
        let a = rt.block_on(apply_job(h.clone(), conn.clone(), opts, sql, h.token()));
        assert_eq!((a.result, a.in_tx, a.ran.len()), (Ok(2), false, 2));
        let notes = |h: &super::Handles| {
            let r = rt.block_on(run_job(h.clone(), conn.clone(), vec![stmt("SELECT coalesce(note, 'NULL') FROM siesta_edit_t ORDER BY id", Kind::Query)], opts, h.token()));
            r.results[0].as_ref().unwrap().rows.iter().map(|r| r[0].display()).collect::<Vec<_>>()
        };
        assert_eq!(notes(&h), ["A", "NULL"]);
        // The second UPDATE finds no row: the first is rolled back too.
        let sql = vec!["UPDATE siesta_edit_t SET note = 'x' WHERE id = 1;".to_string(), "UPDATE siesta_edit_t SET note = 'y' WHERE id = 99;".to_string()];
        let a = rt.block_on(apply_job(h.clone(), conn.clone(), opts, sql, h.token()));
        assert!(a.result.as_ref().unwrap_err().contains("changed 0 rows, not 1") && a.result.as_ref().unwrap_err().contains("Rolled back"), "{:?}", a.result);
        assert!(!a.in_tx);
        assert_eq!(notes(&h), ["A", "NULL"]);
    }

    #[test]
    fn selection_is_split() {
        let src = "SELECT 1;\nSELECT 2; SELECT 3;\n";
        let got = selection_specs(src, 10..src.len(), Dialect::Postgres);
        assert_eq!(got.iter().map(|s| s.text.as_str()).collect::<Vec<_>>(), ["SELECT 2", "SELECT 3"]);
        // Ranges are in the whole file, not in the selection.
        assert_eq!(got.iter().map(|s| s.range.clone().unwrap()).collect::<Vec<_>>(), [10..18, 20..28]);
        assert!(got.iter().all(|s| src[s.range.clone().unwrap()] == s.text));
        assert!(selection_specs(src, 0..0, Dialect::Postgres).is_empty());
    }
}
