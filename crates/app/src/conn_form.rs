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
    ActiveTheme as _, IconName, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    h_flex,
    input::{Input, InputEvent, InputState},
    menu::{DropdownMenu as _, PopupMenuItem},
    tab::{Tab, TabBar},
    tooltip::Tooltip,
    v_flex,
};

use crate::{Siesta, format, runner::RT};

const DRIVERS: [(&str, Driver); 4] = [("Postgres", Driver::Postgres), ("MySQL / MariaDB", Driver::MySql), ("Elasticsearch", Driver::Elastic), ("MongoDB", Driver::Mongo)];

/// The driver's default port (also the Port field's hint).
fn default_port(d: Driver) -> &'static str {
    match d {
        Driver::Postgres => "5432",
        Driver::MySql => "3306",
        Driver::Elastic => "9200",
        Driver::Mongo => "27017",
    }
}

/// The hints of the fields that differ per driver: (URL, Host, User, Init SQL).
fn driver_hints(d: Driver) -> [String; 4] {
    match d {
        Driver::Postgres => ["postgres://user:password@host:5432/db (fills the fields below)".into(), "localhost".into(), format!("{} (your OS user)", db_conn::os_user()), "SET TIME ZONE 'UTC'".into()],
        Driver::MySql => ["mysql://user:password@host:3306/db (fills the fields below)".into(), "localhost".into(), "root".into(), "SET time_zone = '+00:00'".into()],
        Driver::Elastic => ["https://user:password@host:9200 (fills the fields below)".into(), "localhost".into(), "elastic (empty: the password is an API key)".into(), String::new()],
        Driver::Mongo => ["mongodb://user:password@host:27017/db, or mongodb+srv://… (kept as the URL)".into(), "localhost (empty: from the URL)".into(), "admin".into(), String::new()],
    }
}
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
    /// The data dir the password `key` lives in.
    keys: PathBuf,
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
    /// Keeps the Name field a valid connection name while typing.
    _name_sub: Subscription,
    pub host: Entity<InputState>,
    /// Connect through a Unix socket: `socket_path` takes the Host field's place.
    socket: bool,
    pub socket_path: Entity<InputState>,
    /// Re-detects the socket when the port changes.
    _port_sub: Subscription,
    pub port: Entity<InputState>,
    database: Entity<InputState>,
    user: Entity<InputState>,
    pub password: Entity<InputState>,
    schema: Entity<InputState>,
    init_sql: Entity<InputState>,
    /// A name from `format::COLORS`, a `#rrggbb` written by hand (kept), or empty.
    color: String,
    pub driver: Option<Driver>,
    ssl: Option<SslMode>,
    read_only: bool,
    prod: bool,
    error: Option<String>,
    test: Test,
    running: Option<Running>,
    /// Databases the last ↻ listed: the Database field offers them.
    databases: Vec<String>,
    /// The running ↻ (dropped with the form: the connection attempt is aborted).
    loading_dbs: Option<Running>,
    /// "Loading…" or why the list could not be read; muted under the field.
    db_note: Option<String>,
}

impl ConnForm {
    #[allow(clippy::too_many_arguments)]
    fn new(app: WeakEntity<Siesta>, dir: PathBuf, keys: PathBuf, target: Option<PathBuf>, original: Option<String>, f: ConnectionForm, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = |placeholder: &str, value: &str, window: &mut Window, cx: &mut Context<Self>| {
            let (placeholder, value) = (placeholder.to_string(), value.to_string());
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder).default_value(value))
        };
        let password = {
            let hint = if f.has_password { "•••••••• (unchanged)" } else { "Stored encrypted in the connection file" };
            cx.new(|cx| InputState::new(window, cx).placeholder(hint).masked(true))
        };
        let url = input("", "", window, cx);
        let _sub = cx.subscribe_in(&url, window, |this, url, ev: &InputEvent, window, cx| {
            if matches!(ev, InputEvent::Change) {
                let text = url.read(cx).value().to_string();
                this.apply_url(&text, window, cx);
            }
        });
        // The Name field turns what is typed into a valid name (also an old name opened for editing,
        // so Save renames it); `normalize_name` is idempotent, so the set_value below settles at once.
        let name = input("local", &db_conn::normalize_name(&f.name), window, cx);
        let _name_sub = cx.subscribe_in(&name, window, |_, name, ev: &InputEvent, window, cx| {
            if matches!(ev, InputEvent::Change) {
                let text = name.read(cx).value().to_string();
                let fixed = db_conn::normalize_name(&text);
                if fixed != text {
                    name.update(cx, |s, cx| s.set_value(fixed, window, cx));
                }
            }
        });
        let port = input("", &f.port, window, cx);
        let _port_sub = cx.subscribe_in(&port, window, |this, _, ev: &InputEvent, window, cx| {
            if matches!(ev, InputEvent::Change) {
                this.socket_hint(window, cx);
            }
        });
        let mut this = Self {
            _sub,
            _port_sub,
            _name_sub,
            url_note: None,
            has_password: f.has_password,
            app,
            dir,
            keys,
            target,
            original,
            name,
            url,
            host: input("localhost", &f.host, window, cx),
            socket: f.socket,
            socket_path: input("", &f.socket_path, window, cx),
            port,
            database: input("", &f.database, window, cx),
            user: input("", &f.user, window, cx),
            password,
            schema: input("public (Postgres search_path)", &f.schema, window, cx),
            init_sql: input("", &f.init_sql, window, cx),
            color: f.color.trim().to_string(),
            driver: f.driver,
            ssl: f.ssl_mode,
            read_only: f.read_only,
            prod: f.prod,
            base: f.base,
            error: None,
            test: Test::Idle,
            running: None,
            databases: Vec::new(),
            loading_dbs: None,
            db_note: None,
        };
        this.driver_hints(window, cx);
        this
    }

    /// Picks the driver: the fields' hints follow it, and a port left at the other driver's
    /// default moves to this one's.
    fn set_driver(&mut self, driver: Driver, window: &mut Window, cx: &mut Context<Self>) {
        let port = self.port.read(cx).value().trim().to_string();
        if DRIVERS.iter().any(|(_, d)| *d != driver && port == default_port(*d)) {
            self.port.update(cx, |s, cx| s.set_value(default_port(driver), window, cx));
        }
        self.driver = Some(driver);
        // Only Postgres and MySQL connect through a Unix socket.
        self.socket &= matches!(driver, Driver::Postgres | Driver::MySql);
        self.driver_hints(window, cx);
        cx.notify();
    }

    /// The hints of the driver-specific fields (Port, URL, Host, User, Init SQL) for the current driver.
    fn driver_hints(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let d = self.driver.unwrap_or(Driver::Postgres);
        let [url, host, user, init] = driver_hints(d);
        for (input, hint) in [(&self.port, default_port(d).to_string()), (&self.url, url), (&self.host, host), (&self.user, user), (&self.init_sql, init)] {
            input.update(cx, |s, cx| s.set_placeholder(hint.clone(), window, cx));
        }
        self.socket_hint(window, cx);
    }

    /// The socket field's hint: the detected socket, or why there is none.
    fn socket_hint(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.socket {
            return;
        }
        let d = self.driver.unwrap_or(Driver::Postgres);
        let port = self.port.read(cx).value().trim().parse::<u16>().ok();
        let hint = match db_conn::detect_socket(d, port) {
            Some(p) => format!("{} (detected)", p.display()),
            None => "not found: type the socket path".to_string(),
        };
        self.socket_path.update(cx, |s, cx| s.set_placeholder(hint, window, cx));
    }

    /// A typed / pasted URL fills the other fields when it parses completely; otherwise they stay untouched.
    pub(crate) fn apply_url(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.url_note = None;
        if !text.trim().is_empty() {
            match db_conn::parse_url(text) {
                // SRV, several hosts or options: the URL itself is saved (without its password).
                Err(e) if e == db_conn::MONGO_URL_KEPT => {
                    if let Some(pw) = db_conn::url_password(text) {
                        self.password.update(cx, |s, cx| s.set_value(pw, window, cx));
                    }
                    self.driver = Some(Driver::Mongo);
                    self.driver_hints(window, cx);
                    self.url_note = Some(e);
                }
                Err(e) => self.url_note = Some(e),
                Ok(p) => {
                    let set = |i: &Entity<InputState>, v: String, window: &mut Window, cx: &mut Context<Self>| i.update(cx, |s, cx| s.set_value(v, window, cx));
                    set(&self.host, p.host, window, cx);
                    self.socket = !p.socket.is_empty();
                    if self.socket {
                        set(&self.socket_path, p.socket, window, cx);
                    }
                    set(&self.port, p.port.map(|p| p.to_string()).unwrap_or_default(), window, cx);
                    set(&self.database, p.database, window, cx);
                    set(&self.user, p.user, window, cx);
                    if !p.password.is_empty() {
                        set(&self.password, p.password, window, cx);
                    }
                    self.driver = p.driver;
                    self.driver_hints(window, cx);
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
            socket: self.socket,
            socket_path: v(&self.socket_path),
            port: v(&self.port),
            database: v(&self.database),
            user: v(&self.user),
            password: v(&self.password),
            ssl_mode: self.ssl,
            schema: v(&self.schema),
            init_sql: v(&self.init_sql),
            color: self.color.clone(),
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
        let renamed = self.original.clone().filter(|o| *o != name);
        let password_error = match db_conn::save(&self.dir, &self.keys, &form, self.original.as_deref()) {
            Ok(e) => e,
            Err(e) => {
                self.error = Some(e);
                return cx.notify();
            }
        };
        if let Some(e) = password_error {
            // The file is written: a second Save is an edit of it.
            self.original = Some(name.clone());
            self.error = Some(format!("Saved, but the password was not: {e}"));
            self.app
                .update(cx, |this, cx| {
                    this.connection_changed(window, cx);
                    if let Some(old) = &renamed {
                        this.rename_references(&[(old.clone(), name.clone())], window, cx);
                    }
                })
                .ok();
            return cx.notify();
        }
        let target = self.target.clone();
        self.running = None;
        window.close_dialog(cx);
        self.app.update(cx, |this, cx| this.connection_saved(target, renamed, name, window, cx)).ok();
    }

    /// A throwaway session with the form's values; the result is shown in the form, nothing is saved.
    pub fn test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let conn = match self.collect(cx).to_connection(&self.dir, &self.keys, self.original.as_deref()) {
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

    /// The ↻ in the Database field: connects with the form's values (a wrong database name doesn't
    /// block the list: it falls back to the server's default databases) and offers the databases the credentials can open.
    fn load_databases(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let form = self.collect(cx);
        let conn = match form.to_connection(&self.dir, &self.keys, self.original.as_deref()) {
            Ok(c) => c,
            Err(e) => {
                self.db_note = Some(crate::db_error_line(&e));
                return cx.notify();
            }
        };
        self.db_note = Some("Loading…".into());
        let timeout = self.app.read_with(cx, |a, _| Duration::from_millis(a.settings.settings.run.connect_timeout_ms)).unwrap_or(db_exec::CONNECT_TIMEOUT);
        let handle = RT.spawn(async move {
            db_exec::list_databases(&conn, timeout).await
        });
        let abort = handle.abort_handle();
        let task = cx.spawn_in(window, async move |this, cx| {
            let r = handle.await;
            this.update(cx, |this, cx| {
                this.loading_dbs = None;
                match r {
                    Ok(Ok(names)) => {
                        this.db_note = names.is_empty().then(|| "No databases".to_string());
                        this.databases = names;
                    }
                    // One short line: the full error would widen the field (Test shows it whole).
                    Ok(Err(e)) => this.db_note = Some(crate::db_error_line(&e.to_string())),
                    Err(e) => this.db_note = Some(crate::db_error_line(&e.to_string())),
                }
                cx.notify();
            })
            .ok();
        });
        self.loading_dbs = Some(Running { _task: task, abort });
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
            TabBar::new("conn-driver").segmented().small().selected_index(driver_ix).on_click(cx.listener(|this, ix: &usize, window, cx| {
                this.set_driver(DRIVERS[(*ix).min(DRIVERS.len() - 1)].1, window, cx);
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
        // ↻ loads the server's databases; then ⌄ picks one, or "(none)" for the server default.
        let database = {
            let refresh = Button::new("conn-db-refresh")
                .icon(IconName::RefreshCw)
                .ghost()
                .xsmall()
                .loading(self.loading_dbs.is_some())
                .tooltip("Load the server's databases")
                .on_click(cx.listener(|this, _, window, cx| this.load_databases(window, cx)));
            let input = Input::new(&self.database);
            if self.databases.is_empty() {
                input.suffix(refresh)
            } else {
                let (view, names, db) = (cx.entity().downgrade(), self.databases.clone(), self.database.clone());
                let current = db.read(cx).value().to_string();
                input.suffix(h_flex().child(refresh).child(Button::new("conn-db-list").icon(IconName::ChevronDown).ghost().xsmall().dropdown_menu(move |menu, _, _| {
                    std::iter::once("").chain(names.iter().map(String::as_str)).fold(menu.scrollable(true).max_h(px(320.)), |menu, name| {
                        let (view, db, value) = (view.clone(), db.clone(), name.to_string());
                        let label = if name.is_empty() { "(none)".to_string() } else { value.clone() };
                        menu.item(PopupMenuItem::new(label).checked(current == name).on_click(move |_, window, cx| {
                            db.update(cx, |s, cx| s.set_value(value.clone(), window, cx));
                            view.update(cx, |_, cx| cx.notify()).ok();
                        }))
                    })
                })))
            }
        };
        // One clipped line under the Database / User row; the whole text is the tooltip.
        let db_note = self.db_note.clone().map(|n| {
            let full = n.clone();
            div().id("db-note").w_full().text_xs().text_color(muted).truncate().child(n).tooltip(move |window, cx| Tooltip::new(full.clone()).build(window, cx))
        });
        // Colour: "None" or one of the palette, each a swatch in the current theme's shade.
        let color = {
            let dark = cx.theme().is_dark();
            let swatch = move |name: &str| div().flex_none().size(px(10.)).rounded_full().when_some(format::parse_color(name, dark), |d, c| d.bg(c)).when(name.is_empty(), |d| d.border_1().border_color(muted));
            let label = |name: &str| if name.is_empty() { "None".to_string() } else { name.to_string() };
            let (view, current) = (cx.entity().downgrade(), self.color.clone());
            Button::new("conn-color")
                .outline()
                .w_full()
                .dropdown_caret(true)
                .child(h_flex().flex_1().gap_2().items_center().child(swatch(&current)).child(label(&current)))
                .dropdown_menu(move |menu, _, _| {
                    std::iter::once("").chain(format::COLORS.iter().map(|(n, ..)| *n)).fold(menu.scrollable(true), |menu, name| {
                        let (view, value, text) = (view.clone(), name.to_string(), label(name));
                        menu.item(
                            PopupMenuItem::element(move |_, _| h_flex().gap_2().items_center().child(swatch(name)).child(text.clone()))
                                .checked(current.eq_ignore_ascii_case(name))
                                .on_click(move |_, _, cx| {
                                    view.update(cx, |this, cx| {
                                        this.color = value.clone();
                                        cx.notify();
                                    })
                                    .ok();
                                }),
                        )
                    })
                })
        };
        let testing = matches!(self.test, Test::Running);
        let status = self.status(cx);
        let editing = self.original.is_some();
        let location = format!("Saved to {}; the password is kept only encrypted (key: {}).", db_conn::connections_dir(&self.dir).display(), db_conn::crypto::key_path(&self.keys).display());
        // Elasticsearch is HTTP: no socket, database, schema or init SQL; the password may be an API key.
        // MongoDB: no socket, schema or init SQL either, but databases.
        let es = self.driver == Some(Driver::Elastic);
        let sql = self.driver.is_none_or(|d| matches!(d, Driver::Postgres | Driver::MySql));
        let user = div().flex_1().min_w_0().child(field("User", &self.user));
        v_flex()
            .gap_2()
            .child(field("Name", &self.name))
            .child(Self::field("Driver", muted, driver))
            .child(field("URL", &self.url))
            .children(self.url_note.clone().map(|n| div().text_xs().text_color(muted).child(n)))
            .child(
                h_flex()
                    .gap_2()
                    .child(div().flex_1().min_w_0().child(if self.socket { field("Socket", &self.socket_path) } else { field("Host", &self.host) }))
                    // A MySQL socket is a file: no port.
                    .when(!(self.socket && self.driver == Some(Driver::MySql)), |r| r.child(div().w(px(100.)).child(field("Port", &self.port)))),
            )
            .when(sql, |f| {
                f.child(
                    Checkbox::new("conn-socket")
                        .label("Unix socket")
                        .checked(self.socket)
                        .tooltip("Connect through the server's local Unix socket instead of TCP. Leave the path empty to use the detected one, or type your own.")
                        .on_click(cx.listener(|this, v: &bool, window, cx| {
                            this.socket = *v;
                            this.socket_hint(window, cx);
                            cx.notify();
                        })),
                )
            })
            .child(if es { h_flex().child(user) } else { h_flex().gap_2().items_start().child(div().flex_1().min_w_0().child(Self::field("Database", muted, database))).child(user) })
            .when(!es, |f| f.children(db_note))
            .child(Self::field(if es { "Password or API key" } else { "Password" }, muted, Input::new(&self.password).mask_toggle()))
            .child(h_flex().gap_2().child(div().flex_1().child(Self::field(if es { "SSL mode (https)" } else if sql { "SSL mode" } else { "SSL mode (TLS)" }, muted, ssl))).child(div().flex_1().child(Self::field("Color", muted, color))))
            // `search_path` exists only in Postgres; MySQL's schema is the database.
            .when(self.driver.is_none_or(|d| d == Driver::Postgres), |f| f.child(field("Schema (search_path)", &self.schema)))
            .when(sql, |f| f.child(field("Init SQL", &self.init_sql)))
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
        let (Some(dir), Some(keys)) = (self.config_dir.clone(), self.base.clone()) else { return window.push_notification("No home folder (is HOME set?): connections cannot be saved", cx) };
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
        let form = cx.new(|cx| ConnForm::new(app, dir, keys, target, original, data, window, cx));
        self.conn_form = Some(form.downgrade());
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

    /// The form saved `name` (`renamed`: it was called that before): every open file sees it; only
    /// `target` (the file the form was opened for) picks it.
    pub(crate) fn connection_saved(&mut self, target: Option<PathBuf>, renamed: Option<String>, name: String, window: &mut Window, cx: &mut Context<Self>) {
        self.connection_changed(window, cx);
        // The next run reconnects with the saved values and password.
        self.close_connection_sessions(&name);
        if let Some(old) = &renamed {
            self.close_connection_sessions(old);
            self.rename_references(&[(old.clone(), name.clone())], window, cx);
        }
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

    /// A connection was deleted: its sessions close. Files that name it keep their `-- @connection`
    /// line, which the picker then shows in red as unknown (nothing is rewritten behind the user's back).
    pub(crate) fn connection_deleted(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.connection_changed(window, cx);
        self.close_connection_sessions(name);
        self.conn_form = None;
    }
}
