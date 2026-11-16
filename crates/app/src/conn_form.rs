//! New / Edit connection form (a dialog): reached from the connection picker, the DB tab and the
//! Connections page. One form for every connection; it writes `<config dir>/connections/<name>.json`
//! and keeps the password only encrypted there as `password_enc` (reading and writing is `db_conn::global`).

use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

use db_conn::{ConnectionForm, Driver, SslMode};
use db_exec::{DbError, Session};
use gpui::{prelude::FluentBuilder as _, *};
use gpui_component::{
    ActiveTheme as _, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    h_flex,
    input::{Input, InputEvent, InputState},
    menu::{DropdownMenu as _, PopupMenuItem},
    tab::{Tab, TabBar},
    v_flex,
};

use crate::{Siesta, runner::RT};

const DRIVERS: [(&str, Driver); 2] = [("Postgres", Driver::Postgres), ("MySQL / MariaDB", Driver::MySql)];
const SSL_MODES: [(&str, Option<SslMode>); 6] = [
    ("Default", None),
    ("disable", Some(SslMode::Disable)),
    ("prefer", Some(SslMode::Prefer)),
    ("require", Some(SslMode::Require)),
    ("verify-ca", Some(SslMode::VerifyCa)),
    ("verify-full", Some(SslMode::VerifyFull)),
];

enum Test {
    Idle,
    Running,
    Cancelled,
    Ok(String),
    Failed(String),
}

/// The running Test: dropping it (cancel, form closed) drops the GPUI task, so nothing is written
/// back into the form, and aborts the connection attempt on the tokio runtime.
struct Running {
    _task: Task<()>,
    abort: tokio::task::AbortHandle,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.abort.abort();
    }
}

pub(crate) struct ConnForm {
    app: WeakEntity<Siesta>,
    /// The config dir the files live in.
    dir: PathBuf,
    /// The file the form was opened for: the saved connection is picked there, and only there.
    /// `None` (opened from the Connections page): no file's pick changes.
    target: Option<PathBuf>,
    /// The connection being edited (`None`: a new one).
    original: Option<String>,
    /// Keys of the file the form does not show (kept on save).
    base: serde_json::Map<String, serde_json::Value>,
    /// The file holds an encrypted password (an empty password field keeps it).
    has_password: bool,
    pub name: Entity<InputState>,
    pub url: Entity<InputState>,
    /// Why the URL was not (fully) applied, or what it ignored; shown muted under the field.
    url_note: Option<String>,
    _sub: Subscription,
    pub host: Entity<InputState>,
    pub port: Entity<InputState>,
    database: Entity<InputState>,
    user: Entity<InputState>,
    pub password: Entity<InputState>,
    schema: Entity<InputState>,
    init_sql: Entity<InputState>,
    color: Entity<InputState>,
    pub driver: Option<Driver>,
    ssl: Option<SslMode>,
    read_only: bool,
    prod: bool,
    error: Option<String>,
    test: Test,
    running: Option<Running>,
}

impl ConnForm {
    fn new(app: WeakEntity<Siesta>, dir: PathBuf, target: Option<PathBuf>, original: Option<String>, f: ConnectionForm, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = |placeholder: &str, value: &str, window: &mut Window, cx: &mut Context<Self>| {
            let (placeholder, value) = (placeholder.to_string(), value.to_string());
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder).default_value(value))
        };
        let password = {
            let hint = if f.has_password { "•••••••• (unchanged)" } else { "Stored encrypted in the connection file" };
            cx.new(|cx| InputState::new(window, cx).placeholder(hint).masked(true))
        };
        let url = input("postgres://user:password@host:5432/db (fills the fields below)", "", window, cx);
        let _sub = cx.subscribe_in(&url, window, |this, url, ev: &InputEvent, window, cx| {
            if matches!(ev, InputEvent::Change) {
                let text = url.read(cx).value().to_string();
                this.apply_url(&text, window, cx);
            }
        });
        Self {
            _sub,
            url_note: None,
            has_password: f.has_password,
            app,
            dir,
            target,
            original,
            name: input("local", &f.name, window, cx),
            url,
            host: input("localhost", &f.host, window, cx),
            port: input("5432", &f.port, window, cx),
            database: input("", &f.database, window, cx),
            user: input("", &f.user, window, cx),
            password,
            schema: input("public (Postgres search_path)", &f.schema, window, cx),
            init_sql: input("SET TIME ZONE 'UTC'", &f.init_sql, window, cx),
            color: input("red, blue, #rrggbb", &f.color, window, cx),
            driver: f.driver,
            ssl: f.ssl_mode,
            read_only: f.read_only,
            prod: f.prod,
            base: f.base,
            error: None,
            test: Test::Idle,
            running: None,
        }
    }

    /// A typed / pasted URL fills the other fields when it parses completely; otherwise they stay untouched.
    pub(crate) fn apply_url(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.url_note = None;
        if !text.trim().is_empty() {
            match db_conn::parse_url(text) {
                Err(e) => self.url_note = Some(e),
                Ok(p) => {
                    let set = |i: &Entity<InputState>, v: String, window: &mut Window, cx: &mut Context<Self>| i.update(cx, |s, cx| s.set_value(v, window, cx));
                    set(&self.host, p.host, window, cx);
                    set(&self.port, p.port.map(|p| p.to_string()).unwrap_or_default(), window, cx);
                    set(&self.database, p.database, window, cx);
                    set(&self.user, p.user, window, cx);
                    if !p.password.is_empty() {
                        set(&self.password, p.password, window, cx);
                    }
                    self.driver = p.driver;
                    self.ssl = p.ssl_mode;
                    if !p.ignored.is_empty() {
                        self.url_note = Some(format!("Ignored: {}", p.ignored.join(", ")));
                    }
                }
            }
        }
        cx.notify();
    }

    /// What the fields hold now.
    pub fn collect(&self, cx: &App) -> ConnectionForm {
        let v = |i: &Entity<InputState>| i.read(cx).value().to_string();
        ConnectionForm {
            name: v(&self.name),
            driver: self.driver,
            url: v(&self.url),
            host: v(&self.host),
            port: v(&self.port),
            database: v(&self.database),
            user: v(&self.user),
            password: v(&self.password),
            ssl_mode: self.ssl,
            schema: v(&self.schema),
            init_sql: v(&self.init_sql),
            color: v(&self.color),
            read_only: self.read_only,
            prod: self.prod,
            has_password: self.has_password,
            base: self.base.clone(),
        }
    }

    /// Writes the file with the encrypted password; on success the dialog closes and the file it was
    /// opened for picks the connection. If only the password could not be stored, the connection
    /// is saved, every file sees it, and the form stays open with the error.
    pub fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let form = self.collect(cx);
        let name = form.name.trim().to_string();
        let password_error = match db_conn::save(&self.dir, &form, self.original.as_deref()) {
            Ok(e) => e,
            Err(e) => {
                self.error = Some(e);
                return cx.notify();
            }
        };
        if let Some(e) = password_error {
            // The file is written: a second Save is an edit of it.
            self.original = Some(name);
            self.error = Some(format!("Saved, but the password was not: {e}"));
            self.app.update(cx, |this, cx| this.connection_changed(window, cx)).ok();
            return cx.notify();
        }
        let target = self.target.clone();
        self.running = None;
        window.close_dialog(cx);
        self.app.update(cx, |this, cx| this.connection_saved(target, name, window, cx)).ok();
    }

    /// A throwaway session with the form's values; the result is shown in the form, nothing is saved.
    pub fn test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let conn = match self.collect(cx).to_connection(&self.dir, self.original.as_deref()) {
            Ok(c) => c,
            Err(e) => {
                self.test = Test::Failed(e);
                return cx.notify();
            }
        };
        self.test = Test::Running;
        let timeout = self.app.read_with(cx, |a, _| Duration::from_millis(a.settings.settings.run.connect_timeout_ms)).unwrap_or(db_exec::CONNECT_TIMEOUT);
        let handle = RT.spawn(async move {
            let t = Instant::now();
            let s = Session::open_with(&conn, timeout).await?;
            s.close().await;
            Ok::<_, DbError>(t.elapsed())
        });
        let abort = handle.abort_handle();
        let task = cx.spawn_in(window, async move |this, cx| {
            let r = handle.await;
            this.update(cx, |this, cx| {
                this.running = None;
                this.test = match r {
                    Ok(Ok(t)) => Test::Ok(format!("Connected in {} ms", t.as_millis())),
                    Ok(Err(e)) => Test::Failed(e.to_string()),
                    Err(e) => Test::Failed(e.to_string()),
                };
                cx.notify();
            })
            .ok();
        });
        self.running = Some(Running { _task: task, abort });
        cx.notify();
    }

    /// Aborts the running Test.
    fn cancel_test(&mut self, cx: &mut Context<Self>) {
        if self.running.take().is_some() {
            self.test = Test::Cancelled;
            cx.notify();
        }
    }

    fn delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.original.clone() else { return };
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Delete connection '{name}'?"),
            Some("The connection file is removed, together with its encrypted password."),
            &["Delete", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            this.update_in(cx, |this, window, cx| match db_conn::delete(&this.dir, &name) {
                Ok(_) => {
                    this.running = None;
                    window.close_dialog(cx);
                    this.app.update(cx, |app, cx| app.connection_deleted(&name, window, cx)).ok();
                }
                Err(e) => {
                    this.error = Some(e);
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    fn field(label: &str, muted: Hsla, control: impl IntoElement) -> Div {
        v_flex().gap_1().child(div().text_xs().text_color(muted).child(label.to_string())).child(control)
    }

    fn status(&self, cx: &App) -> Option<Div> {
        let (muted, danger, success) = (cx.theme().muted_foreground, cx.theme().danger, cx.theme().success);
        match &self.test {
            Test::Idle => None,
            Test::Running => Some(div().text_sm().text_color(muted).child("Connecting…")),
            Test::Cancelled => Some(div().text_sm().text_color(muted).child("Cancelled")),
            Test::Ok(t) => Some(div().text_sm().text_color(success).child(t.clone())),
            Test::Failed(e) => Some(div().text_sm().text_color(danger).child(e.clone())),
        }
    }
}

impl Render for ConnForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (muted, danger) = (cx.theme().muted_foreground, cx.theme().danger);
        let field = |label: &str, input: &Entity<InputState>| Self::field(label, muted, Input::new(input));

        let driver_ix = DRIVERS.iter().position(|(_, d)| Some(*d) == self.driver).unwrap_or(0);
        let driver = DRIVERS.iter().fold(
            TabBar::new("conn-driver").segmented().small().selected_index(driver_ix).on_click(cx.listener(|this, ix: &usize, _, cx| {
                this.driver = Some(DRIVERS[(*ix).min(DRIVERS.len() - 1)].1);
                cx.notify();
            })),
            |bar, (label, _)| bar.child(Tab::new().label(*label)),
        );
        let (view, current) = (cx.entity().downgrade(), self.ssl);
        let ssl_label = SSL_MODES.iter().find(|(_, m)| *m == current).map_or("Default", |(l, _)| *l);
        let ssl = Button::new("conn-ssl").label(ssl_label).dropdown_caret(true).outline().w_full().dropdown_menu(move |menu, _, _| {
            SSL_MODES.iter().fold(menu, |menu, (label, mode)| {
                let (view, mode) = (view.clone(), *mode);
                menu.item(PopupMenuItem::new(*label).checked(current == mode).on_click(move |_, _, cx| {
                    view.update(cx, |this, cx| {
                        this.ssl = mode;
                        cx.notify();
                    })
                    .ok();
                }))
            })
        });
        let flag = |id: &'static str, label: &'static str, on: bool, set: fn(&mut ConnForm, bool), cx: &mut Context<Self>| {
            Checkbox::new(id).label(label).checked(on).on_click(cx.listener(move |this, v: &bool, _, cx| {
                set(this, *v);
                cx.notify();
            }))
        };
        let testing = matches!(self.test, Test::Running);
        let status = self.status(cx);
        let editing = self.original.is_some();
        let location = format!("Saved to {}; the password is kept only encrypted (key: {}).",db_conn::connections_dir(&self.dir).display(), db_conn::crypto::key_path(&self.dir).display());
        v_flex()
            .gap_2()
            .child(field("Name", &self.name))
            .child(Self::field("Driver", muted, driver))
            .child(field("URL", &self.url))
            .children(self.url_note.clone().map(|n| div().text_xs().text_color(muted).child(n)))
            .child(h_flex().gap_2().child(div().flex_1().child(field("Host", &self.host))).child(div().w(px(100.)).child(field("Port", &self.port))))
            .child(h_flex().gap_2().child(div().flex_1().child(field("Database", &self.database))).child(div().flex_1().child(field("User", &self.user))))
            .child(Self::field("Password", muted, Input::new(&self.password).mask_toggle()))
            .child(h_flex().gap_2().child(div().flex_1().child(Self::field("SSL mode", muted, ssl))).child(div().flex_1().child(field("Color", &self.color))))
            .child(field("Schema (search_path)", &self.schema))
            .child(field("Init SQL", &self.init_sql))
            .child(h_flex().gap_4().child(flag("conn-ro", "Read only", self.read_only, |t, v| t.read_only = v, cx)).child(flag("conn-prod", "Production", self.prod, |t, v| t.prod = v, cx)))
            .child(div().text_xs().text_color(muted).child(location))
            .children(self.error.clone().map(|e| div().text_sm().text_color(danger).child(e)))
            .children(status)
            .child(
                h_flex()
                    .gap_2()
                    .pt_1()
                    .when(editing, |r| r.child(Button::new("conn-delete").label("Delete").danger().on_click(cx.listener(|this, _, window, cx| this.delete(window, cx)))))
                    .child(div().flex_1())
                    .child(if testing {
                        Button::new("conn-test").label("Cancel").on_click(cx.listener(|this, _, _, cx| this.cancel_test(cx)))
                    } else {
                        Button::new("conn-test").label("Test").on_click(cx.listener(|this, _, window, cx| this.test(window, cx)))
                    })
                    .child(Button::new("conn-cancel").label("Close").on_click(cx.listener(|this, _, window, cx| {
                        this.running = None;
                        window.close_dialog(cx);
                    })))
                    .child(Button::new("conn-save").label("Save").primary().on_click(cx.listener(|this, _, window, cx| this.save(window, cx)))),
            )
    }
}

impl Siesta {
    /// "New connection…" (`edit == None`) or "Edit connection…" of `name`; the saved connection is picked by the current file.
    pub(crate) fn open_conn_form(&mut self, edit: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        self.open_conn_form_for(edit, false, true, window, cx)
    }

    /// The form for any connection, independent of the current file's pick. `duplicate`: a new
    /// connection prefilled from `edit` (`<name>-copy`, no password). `pick`: the current file
    /// picks the saved connection (the picker and DB tab); the Connections page never changes picks.
    pub(crate) fn open_conn_form_for(&mut self, edit: Option<String>, duplicate: bool, pick: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dir) = self.config_dir.clone() else { return window.push_notification("No config folder (is HOME set?): connections cannot be saved", cx) };
        let target = if pick {
            let Some(d) = self.doc().filter(|d| d.sql) else { return };
            Some((*d.path).clone())
        } else {
            None
        };
        let mut data = ConnectionForm { driver: Some(Driver::Postgres), ..Default::default() };
        let mut original = None;
        if let Some(name) = &edit {
            let Some(f) = db_conn::read_form(&dir, name) else {
                return window.push_notification(format!("Connection '{name}' could not be read"), cx);
            };
            if duplicate {
                data = ConnectionForm { name: format!("{name}-copy"), has_password: false, ..f };
            } else {
                (original, data) = (Some(name.clone()), f);
            }
        }
        let title = match &original {
            None => "New connection".to_string(),
            Some(n) => format!("Edit connection {n}"),
        };
        let app = cx.entity().downgrade();
        let form = cx.new(|cx| ConnForm::new(app, dir, target, original, data, window, cx));
        self.conn_form = Some(form.clone());
        let name_input = form.read(cx).name.clone();
        window.open_dialog(cx, move |dialog, _, _| dialog.w(px(560.)).title(title.clone()).child(form.clone()));
        name_input.update(cx, |s, cx| s.focus(window, cx));
    }

    /// A connection file changed through the form: every open file sees it, and live sessions of
    /// connections that were edited reconnect on the next run.
    pub(crate) fn connection_changed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.reload_connections(window, cx);
        self.conns_seen = Some(self.conns_stamp());
    }

    /// The form saved `name`: every open file sees it; only `target` (the file the form was opened for) picks it.
    pub(crate) fn connection_saved(&mut self, target: Option<PathBuf>, name: String, window: &mut Window, cx: &mut Context<Self>) {
        self.connection_changed(window, cx);
        // The next run reconnects with the saved values and password.
        self.close_connection_sessions(&name);
        if let Some(target) = target {
            self.pick_for(&target, Some(name), window, cx);
        }
        self.conn_form = None;
    }

    /// Deletes a connection (its file, with the password) from the Connections page.
    pub(crate) fn delete_connection(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dir) = self.config_dir.clone() else { return };
        match db_conn::delete(&dir, name) {
            Ok(_) => self.connection_deleted(name, window, cx),
            Err(e) => window.push_notification(e, cx),
        }
    }

    /// A connection was deleted: files that had picked it are left with "No connection".
    pub(crate) fn connection_deleted(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.connection_changed(window, cx);
        self.close_connection_sessions(name);
        let paths: Vec<PathBuf> = self.docs.iter().filter(|d| d.picked.as_deref() == Some(name)).map(|d| (*d.path).clone()).collect();
        for p in paths {
            self.pick_for(&p, None, window, cx);
        }
        self.conn_form = None;
    }
}
