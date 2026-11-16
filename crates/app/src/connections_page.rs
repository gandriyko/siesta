//! The Connections page (a window, like Settings): a flat list of every connection, with
//! Edit / Test / Duplicate / Delete. Edit, Duplicate and Delete run in the main window (the form is
//! a dialog there) and never change a file's pick.

use std::{collections::HashMap, sync::Arc, time::Instant};

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

#[derive(Debug)]
pub(crate) struct ConnRow {
    pub name: String,
    pub driver: Option<Driver>,
    /// `host:port/database`, or the URL with its password masked.
    pub target: String,
    pub color: Option<String>,
    pub prod: bool,
    pub read_only: bool,
    /// A password is in the keychain (or in the URL).
    pub password: bool,
    /// The current file's connection.
    pub used: bool,
    /// The connection could not be built.
    pub error: Option<String>,
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

/// The page's rows, sorted by name. `filter` is matched against name, driver and target;
/// `has_password` says whether the keychain holds a password for a name.
pub(crate) fn build_rows(c: &Connections, current: Option<&str>, filter: &str, has_password: &dyn Fn(&str) -> bool) -> Vec<ConnRow> {
    let filter = filter.trim().to_lowercase();
    let names: std::collections::BTreeSet<&String> = c.connections.keys().chain(c.errors.keys()).collect();
    names
        .into_iter()
        .map(|name| {
            let conn = c.connections.get(name);
            ConnRow {
                name: name.clone(),
                driver: conn.map(|c| c.driver),
                target: conn.map(target_of).unwrap_or_default(),
                color: conn.and_then(|c| c.display_color().map(str::to_string)),
                prod: conn.is_some_and(|c| c.prod),
                read_only: conn.is_some_and(|c| c.read_only),
                password: conn.is_some_and(|c| c.has_password()) || has_password(name),
                used: current == Some(name.as_str()),
                error: c.errors.get(name).cloned(),
            }
        })
        .filter(|r| {
            let driver = match r.driver {
                Some(Driver::Postgres) => "postgres",
                Some(Driver::MySql) => "mysql mariadb",
                None => "",
            };
            filter.is_empty() || [r.name.as_str(), driver, r.target.as_str()].iter().any(|s| s.to_lowercase().contains(&filter))
        })
        .collect()
}

impl Siesta {
    /// What the page shows: the connections, and the current file's connection.
    pub(crate) fn conns_for_page(&self) -> (Arc<Connections>, Option<String>) {
        (self.conns.clone(), self.doc().filter(|d| d.sql).and_then(|d| d.connection().map(str::to_string)))
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
    /// Which names have a keychain password, read once per loaded `Connections` (a read can be slow).
    passwords: (usize, HashMap<String, bool>),
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
        Self { app, main, focus, filter, tests: HashMap::new(), passwords: (0, HashMap::new()), _subs }
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
            Some("The connection file is removed and its password is deleted from the system keychain."),
            &["Delete", "Cancel"],
            cx,
        );
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            this.update(cx, |this, cx| this.with_app(cx, move |app, window, cx| app.delete_connection(&name, window, cx))).ok();
        })
        .detach();
    }

    fn render_row(&self, ix: usize, r: &ConnRow, cx: &mut Context<Self>) -> AnyElement {
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
                            .child(button("conn-dup", "Duplicate").on_click(cx.listener(move |this, _, _, cx| {
                                let n = n_dup.clone();
                                this.with_app(cx, move |app, window, cx| app.open_conn_form_for(Some(n), true, false, window, cx));
                            })))
                    })
                    .child(button("conn-del", "Delete").danger().on_click(cx.listener(move |this, _, window, cx| this.delete(n_del.clone(), window, cx)))),
            )
            .child(
                h_flex()
                    .gap_3()
                    .text_xs()
                    .text_color(muted)
                    .child(div().min_w_0().truncate().child(r.target.clone()))
                    .child(div().child(if r.password { "password set" } else { "no password" })),
            )
            .children(r.error.clone().map(|e| div().text_xs().text_color(t.danger).child(e)))
            .children(status)
            .into_any_element()
    }
}

impl Render for ConnectionsPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(app) = self.app.upgrade() else { return div().into_any_element() };
        let (conns, current) = app.read(cx).conns_for_page();
        let query = self.filter.read(cx).value().to_string();
        let id = Arc::as_ptr(&conns) as usize;
        if self.passwords.0 != id {
            self.passwords = (id, conns.connections.keys().map(|n| (n.clone(), db_conn::secrets::get(n).is_ok())).collect());
        }
        let stored = &self.passwords.1;
        let rows = build_rows(&conns, current.as_deref(), &query, &|n| stored.get(n).copied().unwrap_or(false));
        let (muted, warning) = (cx.theme().muted_foreground, cx.theme().warning);

        let notes = conns.warnings.iter().map(|w| div().text_xs().text_color(warning).child(w.clone()));
        let mut body = v_flex();
        for (ix, r) in rows.iter().enumerate() {
            body = body.child(self.render_row(ix, r, cx));
        }
        if rows.is_empty() {
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
    use super::build_rows;
    use db_conn::{Connection, Connections, Driver};

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
            ssl: Default::default(),
            read_only: ro,
            prod,
            schema: None,
            init_sql: None,
            color: None,
        }
    }

    #[test]
    fn rows_are_flat_and_carry_badges() {
        let mut c = Connections::default();
        for (n, prod, ro) in [("b", true, true), ("a", false, false)] {
            c.connections.insert(n.into(), conn(n, prod, ro));
        }
        c.connections.get_mut("a").unwrap().url = Some("postgres://u:secret@h/d".into());
        c.errors.insert("broken".into(), "no driver".into());
        let stored = |n: &str| n == "b";
        let rows = build_rows(&c, Some("b"), "", &stored);
        let seen: Vec<_> = rows.iter().map(|r| (r.name.as_str(), r.prod, r.read_only, r.used, r.error.is_some(), r.password)).collect();
        assert_eq!(seen, [("a", false, false, false, false, true), ("b", true, true, true, false, true), ("broken", false, false, false, true, false)]);
        assert_eq!((rows[0].target.as_str(), rows[1].target.as_str()), ("postgres://u:***@h/d", "h:5432/d"));
        // The filter matches names, drivers and targets.
        assert_eq!(build_rows(&c, None, "BRO", &stored).iter().map(|r| r.name.as_str()).collect::<Vec<_>>(), ["broken"]);
        assert_eq!(build_rows(&c, None, "postgres", &stored).len(), 2);
    }
}
