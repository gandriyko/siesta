//! New / Edit connection form (a dialog): reached from the connection picker and the DB tab.
//!
//! It edits the GLOBAL connection files only (`~/.config/siesta/connections.json`; the password
//! goes to the system keychain, or to `connections.private.json` when that is unavailable); the reading, merging and writing is `db_conn::global`.
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
    /// `None` (opened from the Connections page): no file's pick changes.
    target: Option<PathBuf>,
    /// The connection being edited (`None`: a new one).
    original: Option<String>,
    /// Keys of the entry the form does not show (kept on save).
    base: serde_json::Map<String, serde_json::Value>,
    /// The keychain account of the stored password (an empty password field keeps it).
    keychain: Option<String>,
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
    /// For a locked connection: what the personal "My password" field needs (`None`: no `siesta.json` found).
    pub(crate) team: Option<Team>,
}

/// A team connection's personal password: where the private file goes and what could make it moot.
pub(crate) struct Team {
    pub name: String,
    /// The `siesta.json` that defines it; `siesta.private.json` lives next to it.
    pub def_file: PathBuf,
    /// `def_file` relative to the workspace root, for the hint.
    pub rel: String,
    /// The account of the effective `${keychain}` marker, reused on save.
    pub account: Option<String>,
    /// A later file that sets this connection's password (the saved one would not take effect).
    pub overridden_by: Option<PathBuf>,
    /// What Test connects with (the password field replaces its password).
    pub conn: Option<db_conn::Connection>,
}

impl ConnForm {
    #[allow(clippy::too_many_arguments)]
    fn new(app: WeakEntity<Siesta>, dir: PathBuf, target: Option<PathBuf>, original: Option<String>, f: ConnectionForm, locked: Option<(Vec<PathBuf>, String)>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let input = |placeholder: &str, value: &str, window: &mut Window, cx: &mut Context<Self>| {
            let (placeholder, value) = (placeholder.to_string(), value.to_string());
            cx.new(|cx| InputState::new(window, cx).placeholder(placeholder).default_value(value))
        };
        let password = {
            let value = f.password.clone();
            let hint = if f.keychain.is_some() { "Leave empty to keep the password in the system keychain" } else { "Stored in the system keychain" };
            cx.new(|cx| InputState::new(window, cx).placeholder(hint).masked(true).default_value(value))
        };
        Self {
            keychain: f.keychain.clone(),
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
            team: None,
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
            keychain: self.keychain.clone(),
            base: self.base.clone(),
        }
    }

    /// Writes the global files; on success the dialog closes and the file it was opened for picks the connection.
    pub fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let form = self.collect(cx);
        let warning = match db_conn::save_global(&self.dir, &form, self.original.as_deref()) {
            Ok(w) => w,
            Err(e) => {
                self.error = Some(e);
                return cx.notify();
            }
        };
        let (name, target) = (form.name.trim().to_string(), self.target.clone());
        window.close_dialog(cx);
        if let Some(w) = warning {
            window.push_notification(w, cx);
        }
        self.app.update(cx, |this, cx| this.connection_saved(target, name, window, cx)).ok();
    }

    /// A throwaway session with the form's values; the result is shown in the form, nothing is saved.
    pub fn test(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let conn = match self.team.as_ref().map(|t| t.conn.clone().ok_or("this connection has errors, see the list".to_string())) {
            Some(c) => c.map(|mut c| {
                let pw = self.password.read(cx).value().to_string();
                if !pw.is_empty() {
                    (c.password, c.keychain) = (Some(pw), None);
                }
                c
            }),
            None => self.collect(cx).to_connection(&self.dir),
        };
        let conn = match conn {
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
            Some("It is removed from connections.json and connections.private.json; its password is deleted from the system keychain."),
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
            .children(self.team.as_ref().map(|t| self.password_section(t, cx)))
            .child(h_flex().justify_end().child(Button::new("conn-close").label("Close").on_click(|_, window, cx| window.close_dialog(cx))))
            .into_any_element()
    }

    fn status(&self, cx: &App) -> Option<Div> {
        let (muted, danger, success) = (cx.theme().muted_foreground, cx.theme().danger, cx.theme().success);
        match &self.test {
            Test::Idle => None,
            Test::Running => Some(div().text_sm().text_color(muted).child("Connecting…")),
            Test::Ok(t) => Some(div().text_sm().text_color(success).child(t.clone())),
            Test::Failed(e) => Some(div().text_sm().text_color(danger).child(e.clone())),
        }
    }

    /// "My password": a personal password for a team connection, kept in the keychain with a reference in `siesta.private.json`.
    fn password_section(&self, t: &Team, cx: &mut Context<Self>) -> Div {
        let (muted, danger) = (cx.theme().muted_foreground, cx.theme().danger);
        let hint = format!("Stored in your system keychain; siesta.private.json next to {} gets a `${{keychain:…}}` reference. The team's siesta.json is not changed.", t.rel);
        v_flex()
            .gap_1()
            .child(Self::field("My password", muted, Input::new(&self.password).mask_toggle()))
            .child(div().text_xs().text_color(muted).child(hint))
            .children(t.overridden_by.as_ref().map(|p| div().text_xs().text_color(danger).child(format!("The password saved here will not take effect: {} sets it later in the chain.", p.display()))))
            .children(self.error.clone().map(|e| div().text_sm().text_color(danger).child(e)))
            .children(self.status(cx))
            .child(
                h_flex()
                    .gap_2()
                    .pt_1()
                    .child(div().flex_1())
                    .child(Button::new("conn-test").label("Test").on_click(cx.listener(|this, _, window, cx| this.test(window, cx))))
                    .child(Button::new("conn-save-pw").label("Save password").primary().on_click(cx.listener(|this, _, window, cx| this.save_password(window, cx)))),
            )
    }

    /// Stores the personal password; the dialog closes and the app reloads connections and drops their sessions.
    pub fn save_password(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(t) = &self.team else { return };
        let password = self.password.read(cx).value().to_string();
        if password.is_empty() {
            return window.close_dialog(cx); // empty = keep
        }
        match db_conn::set_workspace_password(&t.def_file, &t.name, &password, t.account.as_deref()) {
            Err(e) => {
                self.error = Some(e);
                cx.notify();
            }
            Ok(out) => {
                let name = t.name.clone();
                window.close_dialog(cx);
                if let Some(w) = out.warning {
                    window.push_notification(w, cx);
                }
                self.app.update(cx, |app, cx| app.workspace_password_saved(name, out.gitignore, window, cx)).ok();
            }
        }
    }
}

impl Render for ConnForm {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if let Some((paths, resolved)) = self.locked.clone() {
            return self.locked_view(&paths, &resolved, cx);
        }
        let (muted, danger) = (cx.theme().muted_foreground, cx.theme().danger);
        let field =|label: &str, input: &Entity<InputState>| Self::field(label, muted, Input::new(input));

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
        let status = self.status(cx);
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
            .child(div().text_xs().text_color(muted).child("Saved to connections.json; the password goes to the system keychain. A project's siesta.json overrides a connection of the same name."))
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
        let conns = self.conns_for_page().0;
        let mut data = ConnectionForm { driver: Some(Driver::Postgres), ..Default::default() };
        let (mut original, mut locked, mut team_pw) = (None, None, None);
        if let Some(name) = &edit {
            let globals = [dir.join("connections.json"), dir.join("connections.private.json")];
            let team: Vec<PathBuf> = conns.sources.get(name).into_iter().flatten().filter(|p| !globals.contains(p)).cloned().collect();
            if !team.is_empty() {
                let resolved = match (conns.connections.get(name), conns.errors.get(name)) {
                    (Some(c), _) => serde_json::to_string_pretty(&db_conn::masked(c)).unwrap_or_default(),
                    (None, Some(e)) => format!("error: {e}"),
                    _ => String::new(),
                };
                if let Some(def_file) = db_conn::workspace_def_file(&team).map(PathBuf::from) {
                    let root = self.root_of(&def_file);
                    team_pw = Some(Team {
                        name: name.clone(),
                        rel: def_file.strip_prefix(&root).unwrap_or(&def_file).display().to_string(),
                        overridden_by: db_conn::password_override(conns.sources.get(name).map_or(&[], Vec::as_slice), &def_file, name),
                        account: conns.connections.get(name).and_then(|c| c.keychain.clone()),
                        conn: conns.connections.get(name).cloned(),
                        def_file,
                    });
                }
                locked = Some((team, resolved));
            } else if let Some(f) = db_conn::read_global_form(&dir, name) {
                if duplicate {
                    data = ConnectionForm { name: format!("{name}-copy"), password: String::new(), keychain: None, ..f };
                } else {
                    (original, data) = (Some(name.clone()), f);
                }
            } else {
                return window.push_notification(format!("'{name}' is not in a file Siesta can edit"), cx);
            }
        }
        let edit = edit.filter(|_| !duplicate);
        let title = match (&edit, &locked) {
            (None, _) => "New connection".to_string(),
            (Some(n), Some(_)) => format!("Connection {n}"),
            (Some(n), None) => format!("Edit connection {n}"),
        };
        let app = cx.entity().downgrade();
        let form = cx.new(|cx| ConnForm::new(app, dir, target, original, data, locked, window, cx));
        form.update(cx, |f, _| f.team = team_pw);
        self.conn_form = Some(form.clone());
        let name_input = form.read(cx).name.clone();
        window.open_dialog(cx, move |dialog, _, _| dialog.w(px(560.)).title(title.clone()).child(form.clone()));
        name_input.update(cx, |s, cx| s.focus(window, cx));
    }

    /// The form saved `name`: every open file sees it; only `target` (the file the form was opened for) picks it.
    pub(crate) fn connection_saved(&mut self, target: Option<PathBuf>, name: String, window: &mut Window, cx: &mut Context<Self>) {
        self.reload_connections(window, cx);
        self.conns_seen = Some(self.conns_stamp());
        if let Some(target) = target {
            self.pick_for(&target, Some(name), window, cx);
        }
        self.conn_form = None;
    }

    /// A personal password was saved for the team connection `name`: reload, drop its live sessions
    /// (the next run reconnects with the new password), and offer to git-ignore `siesta.private.json`.
    pub(crate) fn workspace_password_saved(&mut self, name: String, gitignore: Option<PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        self.reload_connections(window, cx);
        self.conns_seen = Some(self.conns_stamp());
        self.conn_form = None;
        self.close_connection_sessions(&name);
        cx.notify();
        let Some(gitignore) = gitignore else { return };
        let answer = window.prompt(PromptLevel::Info, "siesta.private.json is not in .gitignore. Add it?", Some(&format!("It is appended to {}.", gitignore.display())), &["Add", "Not now"], cx);
        cx.spawn_in(window, async move |this, cx| {
            if answer.await == Ok(0)
                && let Err(e) = db_conn::add_to_gitignore(&gitignore)
            {
                this.update_in(cx, |_, window, cx| window.push_notification(e, cx)).ok();
            }
        })
        .detach();
    }

    /// Deletes a global connection (and its keychain entry) from the Connections page.
    pub(crate) fn delete_global_connection(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(dir) = self.config_dir.clone() else { return };
        match db_conn::delete_global(&dir, name) {
            Ok(_) => self.connection_deleted(name, window, cx),
            Err(e) => window.push_notification(e, cx),
        }
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
