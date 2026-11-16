//! New / Edit connection form (a dialog): reached from the connection picker and the DB tab.
//!
//! It edits the GLOBAL connection files only (`~/.config/siesta/connections.json`, and the
//! password in `connections.private.json`); the reading, merging and writing is `db_conn::global`.
//! A connection that a workspace `siesta.json` (or an extra file) defines belongs to the team's
//! files: the dialog shows where it lives and offers to open that file instead.

use std::{path::PathBuf, time::Instant};

use db_conn::{ConnectionForm, Driver, SslMode};
use db_exec::{DbError, Session};
use gpui::{prelude::FluentBuilder as _, *};
use gpui_component::{
    ActiveTheme as _, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    h_flex,
    input::{Input, InputState},
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
    Ok(String),
    Failed(String),
}

pub(crate) struct ConnForm {
    app: WeakEntity<Siesta>,
    /// The config dir the files live in.
    dir: PathBuf,
    /// The file the form was opened for: the saved connection is picked there, and only there.
    target: PathBuf,
    /// The connection being edited (`None`: a new one).
    original: Option<String>,
    /// Keys of the entry the form does not show (kept on save).
    base: serde_json::Map<String, serde_json::Value>,
    pub name: Entity<InputState>,
    url: Entity<InputState>,
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
    /// A connection of a team file: where it is defined (highest priority last), and what it resolves to.
    locked: Option<(Vec<PathBuf>, String)>,
}

impl ConnForm {
    #[allow(clippy::too_many_arguments)]
    fn new(app: WeakEntity<Siesta>, dir: PathBuf, target: PathBuf, original: Option<String>, f: ConnectionForm, locked: Option<(Vec<PathBuf>, String)>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = |placeholder: &str, value: &str, window: &mut Window, cx: &mut Context<Self>| {
            let (placeholder, value) = (placeholder.to_string(), value.to_string());
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder).default_value(value))
        };
        let password = {
            let value = f.password.clone();
            cx.new(|cx| InputState::new(window, cx).placeholder("Stored in connections.private.json").masked(true).default_value(value))
        };
        Self {
            app,
            dir,
            target,
            original,
            name: input("local", &f.name, window, cx),
            url: input("postgres://user@host:5432/db (or fill the fields below)", &f.url, window, cx),
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
            locked,
        }
    }

    /// The files of a read-only (team) connection, for tests.
    #[cfg(test)]
    pub fn locked_paths(&self) -> Option<Vec<PathBuf>> {
        self.locked.as_ref().map(|(p, _)| p.clone())
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
            base: self.base.clone(),
        }
    }

    /// Writes the global files; on success the dialog closes and the file it was opened for picks the connection.
    pub fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let form = self.collect(cx);
        if let Err(e) = db_conn::save_global(&self.dir, &form, self.original.as_deref()) {
            self.error = Some(e);
            return cx.notify();
        }
        let (name, target) = (form.name.trim().to_string(), self.target.clone());
        window.close_dialog(cx);
        self.app.update(cx, |this, cx| this.connection_saved(target, name, window, cx)).ok();
    }

    /// A throwaway session with the form's values; the result is shown in the form, nothing is saved.
    pub fn test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let conn = match self.collect(cx).to_connection(&self.dir) {
            Ok(c) => c,
            Err(e) => {
                self.test = Test::Failed(e);
                return cx.notify();
            }
        };
        self.test = Test::Running;
        let handle = RT.spawn(async move {
            let t = Instant::now();
            let s = Session::open(&conn).await?;
            s.close().await;
            Ok::<_, DbError>(t.elapsed())
        });
        cx.spawn_in(window, async move |this, cx| {
            let r = handle.await;
            this.update(cx, |this, cx| {
                this.test = match r {
                    Ok(Ok(t)) => Test::Ok(format!("Connected in {} ms", t.as_millis())),
                    Ok(Err(e)) => Test::Failed(e.to_string()),
                    Err(e) => Test::Failed(e.to_string()),
                };
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.original.clone() else { return };
        let answer = window.prompt(
            PromptLevel::Warning,
            &format!("Delete connection '{name}'?"),
            Some("It is removed from connections.json and connections.private.json (the password too)."),
            &["Delete", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            this.update_in(cx, |this, window, cx| match db_conn::delete_global(&this.dir, &name) {
                Ok(_) => {
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

    fn locked_view(&self, paths: &[PathBuf], resolved: &str, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let files = paths.iter().enumerate().map(|(i, p)| {
            let path = p.clone();
            h_flex()
                .gap_2()
                .items_center()
                .child(div().flex_1().min_w_0().truncate().text_sm().child(p.display().to_string()))
                .child(Button::new(("open-conn-file", i)).label("Open file").small().on_click(cx.listener(move |this, _, window, cx| {
                    let path = path.clone();
                    window.close_dialog(cx);
                    this.app.update(cx, |app, cx| _ = app.open(path, None, window, cx)).ok();
                })))
        });
        v_flex()
            .gap_3()
            .child(div().text_sm().child("This connection is defined in a project file, which is shared with the team. Edit it there:"))
            .child(v_flex().gap_1().children(files))
            .child(div().text_xs().text_color(muted).child("As Siesta reads it (passwords hidden):"))
            .child(div().p_2().rounded_md().border_1().border_color(cx.theme().border).font_family(cx.theme().mono_font_family.clone()).text_xs().child(resolved.to_string()))
            .child(h_flex().justify_end().child(Button::new("conn-close").label("Close").on_click(|_, window, cx| window.close_dialog(cx))))
            .into_any_element()
    }
}

impl Render for ConnForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some((paths, resolved)) = self.locked.clone() {
            return self.locked_view(&paths, &resolved, cx);
        }
        let (muted, danger, success) = (cx.theme().muted_foreground, cx.theme().danger, cx.theme().success);
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
        let status = match &self.test {
            Test::Idle => None,
            Test::Running => Some(div().text_sm().text_color(muted).child("Connecting…")),
            Test::Ok(t) => Some(div().text_sm().text_color(success).child(t.clone())),
            Test::Failed(e) => Some(div().text_sm().text_color(danger).child(e.clone())),
        };
        let editing = self.original.is_some();
        v_flex()
            .gap_2()
            .child(field("Name", &self.name))
            .child(Self::field("Driver", muted, driver))
            .child(field("URL", &self.url))
            .child(h_flex().gap_2().child(div().flex_1().child(field("Host", &self.host))).child(div().w(px(100.)).child(field("Port", &self.port))))
            .child(h_flex().gap_2().child(div().flex_1().child(field("Database", &self.database))).child(div().flex_1().child(field("User", &self.user))))
            .child(Self::field("Password", muted, Input::new(&self.password).mask_toggle()))
            .child(h_flex().gap_2().child(div().flex_1().child(Self::field("SSL mode", muted, ssl))).child(div().flex_1().child(field("Color", &self.color))))
            .child(field("Schema (search_path)", &self.schema))
            .child(field("Init SQL", &self.init_sql))
            .child(h_flex().gap_4().child(flag("conn-ro", "Read only", self.read_only, |t, v| t.read_only = v, cx)).child(flag("conn-prod", "Production", self.prod, |t, v| t.prod = v, cx)))
            .child(div().text_xs().text_color(muted).child("Saved to connections.json; the password goes to connections.private.json. A project's siesta.json overrides a connection of the same name."))
            .children(self.error.clone().map(|e| div().text_sm().text_color(danger).child(e)))
            .children(status)
            .child(
                h_flex()
                    .gap_2()
                    .pt_1()
                    .when(editing, |r| r.child(Button::new("conn-delete").label("Delete").danger().on_click(cx.listener(|this, _, window, cx| this.delete(window, cx)))))
                    .child(div().flex_1())
                    .child(Button::new("conn-test").label("Test").on_click(cx.listener(|this, _, window, cx| this.test(window, cx))))
                    .child(Button::new("conn-cancel").label("Cancel").on_click(|_, window, cx| window.close_dialog(cx)))
                    .child(Button::new("conn-save").label("Save").primary().on_click(cx.listener(|this, _, window, cx| this.save(window, cx)))),
            )
            .into_any_element()
    }
}

impl Siesta {
    /// "New connection…" (`edit == None`) or "Edit connection…" of `name`, for the current file.
    pub(crate) fn open_conn_form(&mut self, edit: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dir) = self.config_dir.clone() else { return window.push_notification("No config folder (is HOME set?): connections cannot be saved", cx) };
        let Some(d) = self.doc().filter(|d| d.sql) else { return };
        let target = (*d.path).clone();
        let mut data = ConnectionForm { driver: Some(Driver::Postgres), ..Default::default() };
        let (mut original, mut locked) = (None, None);
        if let Some(name) = &edit {
            let globals = [dir.join("connections.json"), dir.join("connections.private.json")];
            let team: Vec<PathBuf> = d.conns.sources.get(name).into_iter().flatten().filter(|p| !globals.contains(p)).cloned().collect();
            if !team.is_empty() {
                let resolved = match (d.conns.connections.get(name), d.conns.errors.get(name)) {
                    (Some(c), _) => serde_json::to_string_pretty(&db_conn::masked(c)).unwrap_or_default(),
                    (None, Some(e)) => format!("error: {e}"),
                    _ => String::new(),
                };
                locked = Some((team, resolved));
            } else if let Some(f) = db_conn::read_global_form(&dir, name) {
                (original, data) = (Some(name.clone()), f);
            } else {
                return window.push_notification(format!("'{name}' is not in a file Siesta can edit"), cx);
            }
        }
        let title = match (&edit, &locked) {
            (None, _) => "New connection".to_string(),
            (Some(n), Some(_)) => format!("Connection {n}"),
            (Some(n), None) => format!("Edit connection {n}"),
        };
        let app = cx.entity().downgrade();
        let form = cx.new(|cx| ConnForm::new(app, dir, target, original, data, locked, window, cx));
        self.conn_form = Some(form.clone());
        let name_input = form.read(cx).name.clone();
        window.open_dialog(cx, move |dialog, _, _| dialog.w(px(560.)).title(title.clone()).child(form.clone()));
        name_input.update(cx, |s, cx| s.focus(window, cx));
    }

    /// The form saved `name`: every open file sees it; only `target` (the file the form was opened for) picks it.
    pub(crate) fn connection_saved(&mut self, target: PathBuf, name: String, window: &mut Window, cx: &mut Context<Self>) {
        self.reload_connections(window, cx);
        self.conns_seen = Some(self.conns_stamp());
        self.pick_for(&target, Some(name), window, cx);
        self.conn_form = None;
    }

    /// A global connection was deleted: files that had picked it are left with "No connection".
    pub(crate) fn connection_deleted(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.reload_connections(window, cx);
        self.conns_seen = Some(self.conns_stamp());
        let paths: Vec<PathBuf> = self.docs.iter().filter(|d| d.picked.as_deref() == Some(name)).map(|d| (*d.path).clone()).collect();
        for p in paths {
            self.pick_for(&p, None, window, cx);
        }
        self.conn_form = None;
    }
}
