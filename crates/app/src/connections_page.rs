//! The Connections page (a window, like Settings): every connection of the current file's chain
//! and every global one, grouped by the file that defines it, with Edit / Test / Duplicate / Delete.
//! Edit, Duplicate and Delete run in the main window (the form is a dialog there) and never
//! change a file's pick.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use db_conn::{Connection, Connections, Driver};
use db_exec::Session;
use gpui::{prelude::FluentBuilder as _, *};
use gpui_component::{
    ActiveTheme as _, Root, Sizable as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    v_flex,
};

use crate::{CloseTab, Siesta, format::parse_color, runner::RT};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PasswordKind {
    None,
    Keychain,
    Env,
    File,
    Url,
}

impl PasswordKind {
    fn label(self) -> Option<&'static str> {
        match self {
            PasswordKind::None => None,
            PasswordKind::Keychain => Some("keychain"),
            PasswordKind::Env => Some("env"),
            PasswordKind::File => Some("file"),
            PasswordKind::Url => Some("in URL"),
        }
    }
}

#[derive(Debug)]
pub(crate) struct ConnRow {
    pub name: String,
    pub driver: Option<Driver>,
    /// `host:port/database`, or the URL with its password masked.
    pub target: String,
    pub color: Option<String>,
    pub prod: bool,
    pub read_only: bool,
    pub password: PasswordKind,
    /// The current file's connection.
    pub used: bool,
    /// The connection could not be built.
    pub error: Option<String>,
}

#[derive(Debug)]
pub(crate) struct ConnGroup {
    pub title: String,
    /// The file to open for this group (the public global file for the global group).
    pub file: PathBuf,
    pub global: bool,
    pub rows: Vec<ConnRow>,
}

fn target_of(c: &Connection) -> String {
    if c.url.is_some() {
        return db_conn::masked(c)["url"].as_str().unwrap_or_default().to_string();
    }
    let host = match (&c.host, c.port) {
        (Some(h), Some(p)) => format!("{h}:{p}"),
        (Some(h), None) => h.clone(),
        (None, Some(p)) => format!(":{p}"),
        (None, None) => String::new(),
    };
    match &c.database {
        Some(d) => format!("{host}/{d}"),
        None if host.is_empty() => "—".into(),
        None => host,
    }
}

/// How `name` keeps its password in `file` (read from the raw file: the loaded connection no longer knows).
pub(crate) fn raw_password_kind(file: &Path, name: &str) -> PasswordKind {
    let raw = std::fs::read_to_string(file).ok().and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok());
    let field = |k: &str| raw.as_ref().and_then(|v| v.get(name)?.get(k)?.as_str());
    password_kind(field("password"), field("url"))
}

/// An explicit `password` wins over one inside the `url` (as when connecting).
fn password_kind(password: Option<&str>, url: Option<&str>) -> PasswordKind {
    let kind = |p: &str, plain| match p.trim() {
        "" => PasswordKind::None,
        p if p.starts_with("${keychain") => PasswordKind::Keychain,
        p if p.contains("${") => PasswordKind::Env,
        _ => plain,
    };
    match kind(password.unwrap_or_default(), PasswordKind::File) {
        PasswordKind::None => url.and_then(|u| db_conn::url_passwords(u).into_iter().map(|r| kind(&u[r], PasswordKind::Url)).find(|k| *k != PasswordKind::None)).unwrap_or(PasswordKind::None),
        k => k,
    }
}

/// The page's rows: the global group first, then one group per workspace file (path relative to `root`);
/// a connection is listed in every file that defines it. `filter` is matched against name, driver and target.
pub(crate) fn build_groups(c: &Connections, global_files: &[PathBuf], root: &Path, current: Option<&str>, filter: &str, kind: &dyn Fn(&Path, &str) -> PasswordKind) -> Vec<ConnGroup> {
    let filter = filter.trim().to_lowercase();
    let mut groups: BTreeMap<(bool, String), ConnGroup> = BTreeMap::new();
    for (name, files) in &c.sources {
        let row = |files_here: &[&PathBuf]| {
            let conn = c.connections.get(name);
            let password = files_here.iter().map(|f| kind(f, name)).rfind(|k| *k != PasswordKind::None).unwrap_or(PasswordKind::None);
            ConnRow {
                name: name.clone(),
                driver: conn.map(|c| c.driver),
                target: conn.map(target_of).unwrap_or_default(),
                color: conn.and_then(|c| c.display_color().map(str::to_string)),
                prod: conn.is_some_and(|c| c.prod),
                read_only: conn.is_some_and(|c| c.read_only),
                password,
                used: current == Some(name.as_str()),
                error: c.errors.get(name).cloned(),
            }
        };
        let matches = |r: &ConnRow| {
            let driver = match r.driver {
                Some(Driver::Postgres) => "postgres",
                Some(Driver::MySql) => "mysql mariadb",
                None => "",
            };
            filter.is_empty() || [r.name.as_str(), driver, r.target.as_str()].iter().any(|s| s.to_lowercase().contains(&filter))
        };
        let global: Vec<&PathBuf> = files.iter().filter(|f| global_files.contains(f)).collect();
        if !global.is_empty() {
            let r = row(&global);
            if matches(&r) {
                let title = format!("Global ({})", global_files[0].display());
                groups.entry((false, String::new())).or_insert_with(|| ConnGroup { title, file: global_files[0].clone(), global: true, rows: vec![] }).rows.push(r);
            }
        }
        for f in files.iter().filter(|f| !global_files.contains(f)) {
            let r = row(&[f]);
            if matches(&r) {
                let rel = f.strip_prefix(root).unwrap_or(f).display().to_string();
                groups.entry((true, rel.clone())).or_insert_with(|| ConnGroup { title: rel, file: f.clone(), global: false, rows: vec![] }).rows.push(r);
            }
        }
    }
    groups.into_values().collect()
}

impl Siesta {
    /// What the page shows: the current file's connections (they include the global ones), else the
    /// workspace's and the global ones alone. Also the current file's connection and the workspace root.
    pub(crate) fn conns_for_page(&self) -> (Arc<Connections>, Option<String>, PathBuf) {
        if let Some(d) = self.doc().filter(|d| d.sql) {
            return (d.conns.clone(), d.connection().map(str::to_string), self.root_of(&d.path));
        }
        let probe = self.root.join("untitled.sql");
        (Arc::new(self.load_connections(&probe)), None, self.root.clone())
    }

    /// Bring the Connections window forward, opening it if it is not open.
    pub(crate) fn open_connections(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(h) = self.connections_window
            && h.update(cx, |_, w, _| w.activate_window()).is_ok()
        {
            return;
        }
        let (app, main) = (cx.entity().downgrade(), window.window_handle());
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(820.), px(640.)), cx)),
            titlebar: Some(TitlebarOptions { title: Some("Connections".into()), ..Default::default() }),
            is_minimizable: false,
            ..Default::default()
        };
        // Deferred, as the settings window: the new window reads `Siesta` while it is being updated.
        cx.defer(move |cx| {
            let weak = app.clone();
            let h = cx.open_window(options, |window, cx| {
                let view = cx.new(|cx| ConnectionsPage::new(weak, main, window, cx));
                cx.new(|cx| Root::new(view, window, cx))
            });
            match h {
                Ok(h) => _ = app.update(cx, |this, _| this.connections_window = Some(h)),
                Err(e) => eprintln!("siesta: connections window: {e}"),
            }
        });
    }
}

enum Test {
    Running,
    Ok(String),
    Failed(String),
}

pub(crate) struct ConnectionsPage {
    app: WeakEntity<Siesta>,
    /// The main window: forms and deletes run there.
    main: AnyWindowHandle,
    focus: FocusHandle,
    filter: Entity<InputState>,
    tests: HashMap<String, Test>,
    _subs: Vec<Subscription>,
}

impl ConnectionsPage {
    pub(crate) fn new(app: WeakEntity<Siesta>, main: AnyWindowHandle, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter connections"));
        let mut _subs = vec![cx.subscribe(&filter, |_, _, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::Change) {
                cx.notify();
            }
        })];
        // The form saved or deleted a connection, or a file changed: rebuild the rows.
        if let Some(entity) = app.upgrade() {
            _subs.push(cx.observe(&entity, |_, _, cx| cx.notify()));
        }
        let focus = cx.focus_handle();
        focus.focus(window, cx);
        Self { app, main, focus, filter, tests: HashMap::new(), _subs }
    }

    /// Runs `f` on `Siesta` in the main window, which comes forward (the form is a dialog there).
    fn with_app(&self, cx: &mut App, f: impl FnOnce(&mut Siesta, &mut Window, &mut Context<Siesta>) + 'static) {
        let app = self.app.clone();
        self.main
            .update(cx, move |_, window, cx| {
                window.activate_window();
                app.update(cx, |a, cx| f(a, window, cx)).ok();
            })
            .ok();
    }

    /// A throwaway session; the result shows in the row.
    fn test(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(conn) = self.app.upgrade().and_then(|a| a.read(cx).conns_for_page().0.connections.get(&name).cloned()) else { return };
        self.tests.insert(name.clone(), Test::Running);
        let handle = RT.spawn(async move {
            let t = Instant::now();
            let conn = conn.resolve_password()?;
            let s = Session::open(&conn).await.map_err(|e| e.to_string())?;
            s.close().await;
            Ok::<_, String>(t.elapsed())
        });
        cx.spawn_in(window, async move |this, cx| {
            let r = handle.await;
            this.update(cx, |this, cx| {
                let t = match r {
                    Ok(Ok(t)) => Test::Ok(format!("Connected in {} ms", t.as_millis())),
                    Ok(Err(e)) => Test::Failed(e),
                    Err(e) => Test::Failed(e.to_string()),
                };
                this.tests.insert(name, t);
                cx.notify();
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn delete(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
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
            this.update(cx, |this, cx| this.with_app(cx, move |app, window, cx| app.delete_global_connection(&name, window, cx))).ok();
        })
        .detach();
    }

    fn render_row(&self, ix: usize, r: &ConnRow, global: bool, file: &Path, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme();
        let (muted, fs) = (t.muted_foreground, t.font_size);
        let dot = r.color.as_deref().and_then(parse_color).unwrap_or(muted.opacity(0.6));
        let badge = |label: &'static str, bg: Hsla, fg: Hsla| div().flex_none().px_1p5().rounded_sm().text_xs().bg(bg).text_color(fg).child(label);
        let driver = match r.driver {
            Some(Driver::Postgres) => "Postgres",
            Some(Driver::MySql) => "MySQL",
            None => "?",
        };
        let name = r.name.clone();
        let button = |id: &'static str, label: &'static str| Button::new((id, ix)).label(label).small();
        let (n_edit, n_test, n_dup, n_del) = (name.clone(), name.clone(), name.clone(), name.clone());
        let file = file.to_path_buf();
        let status = match self.tests.get(&r.name) {
            None => None,
            Some(Test::Running) => Some(div().text_xs().text_color(muted).child("Connecting…")),
            Some(Test::Ok(s)) => Some(div().text_xs().text_color(t.success).child(format!("✓ {s}"))),
            Some(Test::Failed(e)) => Some(div().text_xs().text_color(t.danger).child(format!("✗ {e}"))),
        };
        v_flex()
            .id(("conn-row", ix))
            .px_3()
            .py_2()
            .gap_1()
            .border_b_1()
            .border_color(t.border)
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    .child(div().flex_none().size(fs * 0.7).rounded_full().bg(dot))
                    .child(div().font_weight(FontWeight::SEMIBOLD).child(r.name.clone()))
                    .child(div().text_xs().text_color(muted).child(driver))
                    .when(r.prod, |el| el.child(badge("Production", t.danger, t.danger_foreground)))
                    .when(r.read_only, |el| el.child(badge("Read only", t.secondary, t.secondary_foreground)))
                    .when(r.used, |el| el.child(badge("used by this file", t.accent, t.accent_foreground)))
                    .child(div().flex_1())
                    .child(button("conn-edit", "Edit").on_click(cx.listener(move |this, _, _, cx| {
                        let n = n_edit.clone();
                        this.with_app(cx, move |app, window, cx| app.open_conn_form_for(Some(n), false, false, window, cx));
                    })))
                    .when(r.error.is_none(), |el| {
                        el.child(button("conn-test", "Test").on_click(cx.listener(move |this, _, window, cx| this.test(n_test.clone(), window, cx))))
                    })
                    .when(global, |el| {
                        el.child(button("conn-dup", "Duplicate").on_click(cx.listener(move |this, _, _, cx| {
                            let n = n_dup.clone();
                            this.with_app(cx, move |app, window, cx| app.open_conn_form_for(Some(n), true, false, window, cx));
                        })))
                        .child(button("conn-del", "Delete").danger().on_click(cx.listener(move |this, _, window, cx| this.delete(n_del.clone(), window, cx))))
                    })
                    .when(!global, |el| {
                        el.child(button("conn-open", "Open file").on_click(cx.listener(move |this, _, _, cx| {
                            let p = file.clone();
                            this.with_app(cx, move |app, window, cx| _ = app.open(p, None, window, cx));
                        })))
                    }),
            )
            .child(
                h_flex()
                    .gap_3()
                    .text_xs()
                    .text_color(muted)
                    .child(div().min_w_0().truncate().child(r.target.clone()))
                    .child(div().child(match r.password.label() {
                        Some(k) => format!("password: {k}"),
                        None => "no password".to_string(),
                    })),
            )
            .children(r.error.clone().map(|e| div().text_xs().text_color(t.danger).child(e)))
            .children(status)
            .into_any_element()
    }
}

impl Render for ConnectionsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(app) = self.app.upgrade() else { return div().into_any_element() };
        let (conns, current, root) = app.read(cx).conns_for_page();
        let dir = app.read(cx).config_dir.clone();
        let global_files: Vec<PathBuf> = dir.iter().flat_map(|d| [d.join("connections.json"), d.join("connections.private.json")]).collect();
        let query = self.filter.read(cx).value().to_string();
        let groups = build_groups(&conns, &global_files, &root, current.as_deref(), &query, &raw_password_kind);
        let (muted, warning, danger) = (cx.theme().muted_foreground, cx.theme().warning, cx.theme().danger);

        let notes = conns
            .warnings
            .iter()
            .map(|w| div().text_xs().text_color(warning).child(w.clone()))
            .chain(conns.errors.iter().map(|(n, e)| div().text_xs().text_color(danger).child(format!("'{n}': {e}"))));
        let mut ix = 0;
        let mut body = v_flex();
        for g in &groups {
            body = body.child(
                h_flex()
                    .px_3()
                    .pt_4()
                    .pb_1()
                    .gap_2()
                    .child(div().flex_1().min_w_0().truncate().text_xs().font_weight(FontWeight::BOLD).text_color(muted).child(g.title.clone())),
            );
            for r in &g.rows {
                body = body.child(self.render_row(ix, r, g.global, &g.file, cx));
                ix += 1;
            }
        }
        if groups.is_empty() {
            body = body.child(div().px_3().pt_4().text_sm().text_color(muted).child(if query.trim().is_empty() { "No connections yet" } else { "Nothing matches" }));
        }
        v_flex()
            .id("connections")
            .key_context("Siesta")
            .track_focus(&self.focus)
            .on_action(|_: &CloseTab, window, _| window.remove_window())
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(
                h_flex()
                    .flex_none()
                    .gap_2()
                    .p_3()
                    .child(div().flex_1().child(Input::new(&self.filter).small().cleanable(true)))
                    .child(Button::new("conn-new").label("New connection…").primary().small().on_click(cx.listener(|this, _, _, cx| {
                        this.with_app(cx, |app, window, cx| app.open_conn_form_for(None, false, false, window, cx));
                    }))),
            )
            .child(v_flex().flex_none().px_3().gap_0p5().children(notes))
            .child(div().id("conn-list").flex_1().min_h_0().overflow_y_scroll().child(body))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: it brings `gpui::test`, which shadows `#[test]`.
    use super::{PasswordKind, build_groups, password_kind};
    use db_conn::{Connection, Connections, Driver};
    use std::path::{Path, PathBuf};

    fn conn(name: &str, prod: bool, ro: bool) -> Connection {
        Connection {
            name: name.into(),
            driver: Driver::Postgres,
            url: None,
            host: Some("h".into()),
            port: Some(5432),
            database: Some("d".into()),
            user: None,
            password: None,
            keychain: None,
            ssl: Default::default(),
            read_only: ro,
            prod,
            schema: None,
            init_sql: None,
            color: None,
        }
    }

    #[test]
    fn rows_group_by_file_and_carry_badges() {
        let global = vec![PathBuf::from("/cfg/connections.json"), PathBuf::from("/cfg/connections.private.json")];
        let ws = PathBuf::from("/ws/siesta.json");
        let mut c = Connections::default();
        for (n, prod, ro, files) in [("a", false, false, vec![global[0].clone()]), ("b", true, true, vec![ws.clone()]), ("c", false, false, vec![global[0].clone(), ws.clone()])] {
            c.connections.insert(n.into(), conn(n, prod, ro));
            c.sources.insert(n.into(), files);
        }
        c.errors.insert("broken".into(), "no driver".into());
        c.sources.insert("broken".into(), vec![ws.clone()]);
        let kind = |_: &Path, n: &str| if n == "a" { PasswordKind::Keychain } else { PasswordKind::None };
        let g = build_groups(&c, &global, Path::new("/ws"), Some("b"), "", &kind);
        assert_eq!(g.len(), 2);
        assert!(g[0].global && g[0].title.starts_with("Global (/cfg/connections.json"));
        assert_eq!(g[0].rows.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["a", "c"]);
        assert_eq!((g[0].rows[0].password, g[0].rows[0].target.as_str()), (PasswordKind::Keychain, "h:5432/d"));
        assert_eq!(g[1].title, "siesta.json");
        let names: Vec<_> = g[1].rows.iter().map(|r| (r.name.as_str(), r.prod, r.read_only, r.used, r.error.is_some())).collect();
        assert_eq!(names, [("b", true, true, true, false), ("broken", false, false, false, true), ("c", false, false, false, false)]);
        // The filter matches names.
        let g = build_groups(&c, &global, Path::new("/ws"), None, "BRO", &kind);
        assert_eq!((g.len(), g[0].rows[0].name.as_str()), (1, "broken"));
    }

    #[test]
    fn password_kind_looks_at_the_url() {
        use PasswordKind::{Env, File, Keychain, Url};
        let k = password_kind;
        assert_eq!(k(None, Some("postgres://u:secret@h/d")), Url);
        assert_eq!(k(None, Some("postgres://h/d?sslmode=x&password=secret")), Url);
        assert_eq!(k(None, Some("postgres://u:${env:X}@h/d")), Env);
        assert_eq!(k(None, Some("postgres://h/d?password=${keychain}")), Keychain);
        assert_eq!(k(None, Some("postgres://u@h/d")), PasswordKind::None);
        assert_eq!(k(Some("${keychain}"), Some("postgres://u:secret@h/d")), Keychain);
        assert_eq!(k(Some("pw"), Some("postgres://u:${env:X}@h/d")), File);
        assert_eq!(k(Some(""), Some("postgres://u:secret@h/d")), Url);
        assert_eq!(k(None, None), PasswordKind::None);
    }
}
