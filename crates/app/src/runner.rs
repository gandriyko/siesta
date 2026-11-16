//! Running statements: the tokio runtime, one session per (file, connection), the run pipeline
//! (confirm → session → run → result) and the hooks of the main view (`run_current`, `cancel_run`, …).
//!
//! sqlx needs tokio, GPUI has its own executor: jobs run on `RT`, the GPUI task that awaits a job
//! delivers its result to the view (the same bridge as Respite's `start`). A job is never aborted
//! (a dropped future would leave a connection mid-query): it is cancelled on the server instead.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, LazyLock, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use db_conn::{Connection, Driver};
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

/// The session of one (file, connection): opened lazily by the first run, closed with the file.
pub struct Slot {
    handles: Handles,
    pub dot: Dot,
    pub in_tx: bool,
}

/// What the tokio side shares with the GUI: the session (runs take its lock, so they go one by
/// one), the cancel handle (usable while a run holds the lock) and the "stop the batch" flag.
#[derive(Clone)]
pub struct Handles {
    session: Arc<AsyncMutex<Option<Session>>>,
    cancel: Arc<Mutex<Option<CancelHandle>>>,
    stop: Arc<AtomicBool>,
}

impl Slot {
    fn new() -> Self {
        Slot {
            handles: Handles { session: Arc::new(AsyncMutex::new(None)), cancel: Arc::new(Mutex::new(None)), stop: Arc::new(AtomicBool::new(false)) },
            dot: Dot::Grey,
            in_tx: false,
        }
    }
}

/// One statement to run.
#[derive(Clone, Debug, PartialEq)]
pub struct Spec {
    pub text: String,
    pub kind: Kind,
    pub page_size: Option<usize>,
    /// `-- @confirm`
    pub confirm: bool,
}

impl Spec {
    pub fn of(st: &sql_split::Statement, src: &str) -> Self {
        Spec { text: st.text(src).to_string(), kind: st.kind, page_size: st.page_size, confirm: st.confirm }
    }
}

/// The confirmation dialog is needed for `-- @confirm`, and per `run.confirm_destructive` for
/// destructive statements (`prod`: on a connection matching `prod_pattern` or `read_only`).
pub fn needs_confirm(spec: &Spec, mode: ConfirmDestructive, prod: bool, dialect: Dialect) -> bool {
    spec.confirm
        || match mode {
            ConfirmDestructive::Never => false,
            ConfirmDestructive::Always => sql_split::is_destructive(&spec.text, dialect),
            ConfirmDestructive::Prod => prod && sql_split::is_destructive(&spec.text, dialect),
        }
}

/// The statements of the selected text (`range` of `src`), split with the file's dialect.
pub fn selection_specs(src: &str, range: std::ops::Range<usize>, dialect: Dialect) -> Vec<Spec> {
    let Some(text) = src.get(range) else { return vec![] };
    sql_split::split(text, dialect).statements.iter().map(|s| Spec::of(s, text)).collect()
}

#[derive(Clone)]
pub struct Stmt {
    pub text: String,
    pub kind: Kind,
    pub page: Page,
}

#[derive(Clone, Copy)]
struct Opts {
    stop_on_error: bool,
    timeout_ms: Option<u64>,
    max_rows: usize,
}

pub struct Report {
    /// One per executed statement (fewer than asked when the batch stopped).
    pub results: Vec<Result<Outcome, DbError>>,
    pub in_tx: bool,
    /// A session exists (false: it could not be opened).
    pub live: bool,
}

async fn acquire(h: &Handles, conn: &Connection, opts: Opts) -> Result<OwnedMutexGuard<Option<Session>>, DbError> {
    let mut g = h.session.clone().lock_owned().await;
    if g.is_none() {
        let mut s = Session::open(conn).await?;
        if let Some(ms) = opts.timeout_ms {
            let sql = match conn.driver {
                Driver::Postgres => format!("SET statement_timeout = {ms}"),
                Driver::MySql => format!("SET SESSION max_execution_time = {ms}"),
            };
            s.run(&sql, Kind::Other, &Page::default()).await?;
        }
        *h.cancel.lock().unwrap() = Some(s.cancel_handle());
        *g = Some(s);
    }
    if let Some(s) = g.as_mut() {
        s.max_rows = opts.max_rows;
    }
    Ok(g)
}

async fn run_job(h: Handles, conn: Connection, stmts: Vec<Stmt>, opts: Opts) -> Report {
    let mut g = match acquire(&h, &conn, opts).await {
        Ok(g) => g,
        Err(e) => return Report { results: vec![Err(e)], in_tx: false, live: false },
    };
    // A cancel that arrived while this job waited for the lock was meant for the run before it.
    h.stop.store(false, Ordering::Relaxed);
    let s = g.as_mut().expect("session opened");
    let mut results = vec![];
    for st in stmts {
        if h.stop.load(Ordering::Relaxed) {
            break;
        }
        let r = s.run(&st.text, st.kind, &st.page).await;
        // A reconnect inside `run` changes the backend id.
        *h.cancel.lock().unwrap() = Some(s.cancel_handle());
        let failed = r.is_err();
        results.push(r);
        if failed && opts.stop_on_error {
            break;
        }
    }
    Report { results, in_tx: s.in_transaction(), live: true }
}

async fn count_job(h: Handles, conn: Connection, text: String, filters: Vec<(String, String)>, opts: Opts) -> Result<u64, DbError> {
    let mut g = acquire(&h, &conn, opts).await?;
    g.as_mut().expect("session opened").count(&text, &filters).await
}

/// Close the session on the runtime; `finish` (`COMMIT` / `ROLLBACK`) runs first.
fn close_slot(slot: Slot, finish: Option<&'static str>) {
    let Slot { handles, .. } = slot;
    handles.stop.store(true, Ordering::Relaxed);
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
    });
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").chars().take(120).collect()
}

impl Siesta {
    fn opts(&self) -> Opts {
        let s = &self.settings.settings;
        Opts { stop_on_error: s.run.stop_on_error, timeout_ms: s.run.statement_timeout_ms, max_rows: s.results.max_rows }
    }

    /// The connection of `path` by name: spec, or why it is unusable.
    fn connection_of(&self, path: &Path) -> Result<(String, Connection), String> {
        let d = self.docs.iter().find(|d| *d.path == path).ok_or("The file is not open")?;
        let name = d.connection().ok_or("No connection: pick one in the top-right corner")?;
        match d.conns.connections.get(name) {
            Some(c) => Ok((name.to_string(), c.clone())),
            None => Err(match d.conns.errors.get(name) {
                Some(e) => format!("Connection '{name}' is unusable: {e}"),
                None => format!("Unknown connection '{name}'"),
            }),
        }
    }

    pub(crate) fn dot_of(&self, path: &Path, conn: Option<&str>) -> Dot {
        conn.and_then(|c| self.slots.get(&(path.to_path_buf(), c.to_string()))).map_or(Dot::Grey, |s| s.dot)
    }

    /// The open transaction of any session of `path`.
    pub(crate) fn in_tx(&self, path: &Path) -> bool {
        self.slots.iter().any(|((p, _), s)| p == path && s.in_tx)
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
        for ((p, _), slot) in &self.slots {
            if *p != path {
                continue;
            }
            slot.handles.stop.store(true, Ordering::Relaxed);
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
        let handle = RT.spawn(async move {
            let t = Instant::now();
            let s = Session::open(&conn).await?;
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

    /// "Disconnect": drop the sessions of the current file (an open transaction is rolled back by the server).
    pub(crate) fn disconnect(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(path) = self.doc().map(|d| (*d.path).clone()) else { return };
        self.close_sessions(&path, None);
        window.push_notification("Disconnected", cx);
        cx.notify();
    }

    /// Close every session of `path` (`finish` runs first) and forget its results.
    pub(crate) fn close_sessions(&mut self, path: &Path, finish: Option<&'static str>) {
        let keys: Vec<_> = self.slots.keys().filter(|(p, _)| p == path).cloned().collect();
        for k in keys {
            if let Some(slot) = self.slots.remove(&k) {
                close_slot(slot, finish);
            }
        }
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
        let st = &self.settings.settings;
        let prod = st.connections.is_prod(&name) || conn.read_only;
        let dialect = crate::doc::dialect_of(conn.driver);
        let ask: Vec<&Spec> = specs.iter().filter(|s| needs_confirm(s, st.run.confirm_destructive, prod, dialect)).collect();
        if ask.is_empty() {
            return self.execute(path, name, conn, specs, window, cx);
        }
        let message = format!("Run {} on {name}?", if ask.len() == 1 { "this statement".to_string() } else { format!("these {} statements", ask.len()) });
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
            stmts.push(Stmt { text: s.text.clone(), kind: s.kind, page: e.page.clone() });
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
        let conn = self.docs.iter().find(|d| *d.path == key.0).and_then(|d| d.conns.connections.get(&done.conn).cloned()).unwrap_or(conn);
        let e = self.store.entries.get_mut(key).expect("entry");
        e.running = Some(Instant::now());
        let stmt = Stmt { text: done.text.clone(), kind: done.kind, page: e.page.clone() };
        self.spawn_job(key.0.clone(), done.conn.clone(), conn, vec![stmt], vec![key.clone()], window, cx);
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_job(&mut self, path: PathBuf, name: String, conn: Connection, stmts: Vec<Stmt>, keys: Vec<Key>, window: &mut Window, cx: &mut Context<Self>) {
        let slot = self.slots.entry((path.clone(), name.clone())).or_insert_with(Slot::new);
        let handles = slot.handles.clone();
        let opts = self.opts();
        let handle = RT.spawn(run_job(handles, conn.clone(), stmts.clone(), opts));
        self.ensure_ticker(window, cx);
        cx.spawn_in(window, async move |this, cx| {
            let report = match handle.await {
                Ok(r) => r,
                Err(e) => Report { results: vec![Err(DbError { message: format!("Internal error: {e}"), ..Default::default() })], in_tx: false, live: false },
            };
            this.update_in(cx, |this, window, cx| this.finish(path, name, conn, stmts, keys, report, window, cx)).ok();
        })
        .detach();
        cx.notify();
    }

    #[allow(clippy::too_many_arguments)]
    fn finish(&mut self, path: PathBuf, name: String, conn: Connection, stmts: Vec<Stmt>, keys: Vec<Key>, report: Report, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(slot) = self.slots.get_mut(&(path.clone(), name.clone())) {
            slot.in_tx = report.in_tx;
            slot.dot = if report.live && report.results.last().is_some_and(Result::is_ok) { Dot::Green } else { Dot::Red };
        }
        let color = conn.color.clone();
        let mut position = None;
        for (i, (stmt, key)) in stmts.into_iter().zip(keys).enumerate() {
            let Some(e) = self.store.entries.get_mut(&key) else { continue };
            e.running = None;
            let Some(result) = report.results.get(i).cloned() else { continue };
            if let Err(err) = &result
                && let Some(p) = err.position
            {
                position = Some((stmt.text.clone(), p, err.message.clone()));
            }
            e.id += 1;
            e.done = Some(Arc::new(Done { text: stmt.text, kind: stmt.kind, page: stmt.page, conn: name.clone(), color: color.clone(), driver: conn.driver, result }));
        }
        if let Some((text, p, msg)) = position {
            self.underline_error(&path, &text, p, &msg, cx);
        }
        if report.results.iter().any(|r| r.as_ref().is_err_and(|e| e.connection_lost)) {
            window.push_notification("Connection was lost and reopened", cx);
        }
        cx.notify();
    }

    /// A server error with a position: red underline at that character until the next edit
    /// (the editor drops diagnostics on every change).
    pub(crate) fn underline_error(&mut self, path: &Path, text: &str, pos: usize, msg: &str, cx: &mut Context<Self>) {
        let Some(d) = self.docs.iter().find(|d| *d.path == path) else { return };
        d.editor.update(cx, |s, cx| {
            let src = s.value();
            let Some(start) = src.find(text) else { return };
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

    /// "Count all": `SELECT count(*)` over the statement and its filters, on demand.
    pub(crate) fn count_all(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = self.shown_key(cx) else { return };
        let Some(e) = self.store.entries.get(&key) else { return };
        let Some(done) = e.done.clone() else { return };
        let filters = e.page.filters.clone();
        let Some(conn) = self.docs.iter().find(|d| *d.path == key.0).and_then(|d| d.conns.connections.get(&done.conn).cloned()) else { return };
        let slot = self.slots.entry((key.0.clone(), done.conn.clone())).or_insert_with(Slot::new);
        let handle = RT.spawn(count_job(slot.handles.clone(), conn, done.text.clone(), filters.clone(), self.opts()));
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

    /// Closing a file with an open transaction: Commit / Rollback / Cancel, then the close goes on.
    pub(crate) fn ask_transaction(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
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
                let finish = match answer {
                    0 => "COMMIT",
                    1 => "ROLLBACK",
                    _ => return,
                };
                this.close_sessions(&path, Some(finish));
                if let Some(ix) = this.docs.iter().position(|d| *d.path == path) {
                    this.close(ix, window, cx);
                }
            })
            .ok();
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: it brings `gpui::test`, which shadows `#[test]`.
    use super::{Spec, needs_confirm, selection_specs};
    use siesta_settings::ConfirmDestructive;
    use sql_split::{Dialect, Kind};

    fn spec(text: &str, confirm: bool) -> Spec {
        Spec { text: text.into(), kind: Kind::Other, page_size: None, confirm }
    }

    #[test]
    fn confirmation_rules() {
        use ConfirmDestructive::*;
        let (del, sel) = (spec("DELETE FROM t", false), spec("SELECT 1", false));
        let d = Dialect::Postgres;
        assert!(needs_confirm(&del, Prod, true, d)); // destructive on prod
        assert!(!needs_confirm(&del, Prod, false, d)); // not on a dev connection
        assert!(needs_confirm(&del, Always, false, d));
        assert!(!needs_confirm(&del, Never, true, d));
        assert!(!needs_confirm(&sel, Always, true, d));
        assert!(needs_confirm(&spec("SELECT 1", true), Never, false, d)); // `-- @confirm` always asks
        assert!(!needs_confirm(&spec("DELETE FROM t WHERE id = 1", false), Prod, true, d));
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
            port: None,
            database: None,
            user: None,
            password: None,
            ssl: Default::default(),
            read_only: false,
            schema: None,
            init_sql: None,
            color: None,
        };
        let opts = Opts { stop_on_error: true, timeout_ms: Some(5000), max_rows: 1000 };
        let stmt = |text: &str, kind| Stmt { text: text.into(), kind, page: db_exec::Page { size: 2, ..Default::default() } };
        let slot = Slot::new();
        let h = slot.handles.clone();
        let rt = &*super::RT;
        // Statement timeout from settings, two pages of rows, then a failing statement that stops the batch.
        let r = rt.block_on(run_job(
            h.clone(),
            conn.clone(),
            vec![stmt("SHOW statement_timeout", Kind::Read), stmt("SELECT g FROM generate_series(1, 5) g", Kind::Query), stmt("SELEC 1", Kind::Other), stmt("SELECT 1", Kind::Query)],
            opts,
        ));
        assert!(r.live);
        assert_eq!(r.results.len(), 3); // stopped at the error
        assert_eq!(r.results[0].as_ref().unwrap().rows[0][0].display(), "5s");
        let page = r.results[1].as_ref().unwrap();
        assert_eq!((page.rows.len(), page.has_more), (2, true));
        assert_eq!(r.results[2].as_ref().unwrap_err().position, Some(1)); // `SELEC` is at character 1
        // The same session keeps its state; count runs on it too.
        let n = rt.block_on(count_job(h.clone(), conn.clone(), "SELECT g FROM generate_series(1, 5) g".into(), vec![("g".into(), ">2".into())], opts));
        assert_eq!(n.unwrap(), 3);
        // Cancel from outside while a statement runs: it fails with 57014 and the session survives.
        let cancel = h.cancel.lock().unwrap().clone().unwrap();
        let slow = rt.spawn(run_job(h.clone(), conn.clone(), vec![stmt("SELECT pg_sleep(4)", Kind::Other)], Opts { timeout_ms: None, ..opts }));
        rt.block_on(async {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            cancel.cancel().await.unwrap();
        });
        let r = rt.block_on(slow).unwrap();
        assert_eq!(r.results[0].as_ref().unwrap_err().code.as_deref(), Some("57014"));
        let r = rt.block_on(run_job(h, conn, vec![stmt("SELECT 1", Kind::Query)], opts));
        assert!(r.results[0].is_ok());
    }

    #[test]
    fn selection_is_split() {
        let src = "SELECT 1;\nSELECT 2; SELECT 3;\n";
        let got = selection_specs(src, 10..src.len(), Dialect::Postgres);
        assert_eq!(got.iter().map(|s| s.text.as_str()).collect::<Vec<_>>(), ["SELECT 2", "SELECT 3"]);
        assert!(selection_specs(src, 0..0, Dialect::Postgres).is_empty());
    }
}
