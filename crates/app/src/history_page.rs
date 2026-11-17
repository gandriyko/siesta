//! History (Cmd+Y): every statement run in this project, newest first, with a filter. Insert puts
//! the statement into the current file after the statement under the cursor; Run runs it again in
//! its own file (where it still is), else inserted into the current file.

use std::{ops::Range, path::Path};

use db_exec::history::{Entry, Store};
use gpui::{prelude::FluentBuilder as _, *};
use gpui_component::{
    ActiveTheme as _, IconName, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    input::Input,
};

use crate::Siesta;

const TIME_W: f32 = 104.;
const CONN_W: f32 = 140.;
const FILE_W: f32 = 140.;
const OUTCOME_W: f32 = 150.;
const MS_W: f32 = 56.;
const ACTIONS_W: f32 = 56.;

impl Siesta {
    fn history_store(&self) -> Option<Store> {
        let mut s = Store::open(self.base.as_deref()?, &self.root).inspect_err(|e| eprintln!("siesta: history disabled: {e}")).ok()?;
        s.total = self.settings.settings.history.total;
        Some(s)
    }

    /// Writes finished runs off the UI thread; an open History page shows them when they land.
    pub(crate) fn record_history(&self, path: &Path, entries: Vec<Entry>, cx: &mut Context<Self>) {
        if entries.is_empty() {
            return;
        }
        let Some(store) = self.history_store() else { return };
        let path = path.to_path_buf();
        let write = cx.background_spawn(async move {
            for e in entries {
                if let Err(err) = store.record(&path, e) {
                    eprintln!("siesta: history: {err}");
                }
            }
        });
        cx.spawn(async move |this, cx| {
            write.await;
            this.update(cx, |this, cx| {
                if this.show_history {
                    this.reload_history();
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn toggle_history(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_history = !self.show_history;
        if self.show_history {
            self.reload_history();
            self.history_filter.update(cx, |s, cx| s.focus(window, cx));
        } else if let Some(d) = self.doc() {
            d.editor.update(cx, |s, cx| s.focus(window, cx));
        }
        cx.notify();
    }

    pub(crate) fn reload_history(&mut self) {
        self.history = self.history_store().map(|s| s.list()).unwrap_or_default();
    }

    /// Indices of entries matching every word of the filter (SQL, connection, database, file, outcome).
    fn history_matches(&self, cx: &App) -> Vec<usize> {
        let query = self.history_filter.read(cx).value().to_lowercase();
        let words: Vec<&str> = query.split_whitespace().collect();
        (0..self.history.len())
            .filter(|&i| {
                let e = &self.history[i];
                let hay = format!("{} {} {} {} {}", e.sql, e.connection, e.database.as_deref().unwrap_or(""), e.file, e.outcome()).to_lowercase();
                words.iter().all(|w| hay.contains(w))
            })
            .collect()
    }

    /// Insert: into the file it was recorded in (opened, after the statement under its cursor), so
    /// it lands under that file's connection. A file that is gone leaves the current file, with a
    /// note when that one is on another connection. The page closes.
    pub(crate) fn history_insert(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let e = self.history[i].clone();
        let path = self.folder().join(&e.file);
        if !(path.is_file() && self.open(path, None, window, cx).is_some()) {
            let now = self.doc().filter(|d| d.sql).map(|d| d.connection().map(str::to_string));
            if let Some(now) = now.filter(|n| n.as_deref() != Some(e.connection.as_str())) {
                window.push_notification(format!("{} is gone; inserted here. Recorded on `{}`, this file uses `{}`", e.file, e.connection, now.as_deref().unwrap_or("no connection")), cx);
            }
        }
        if self.insert_statement(&e.sql, window, cx) {
            self.show_history = false;
            cx.notify();
            return true;
        }
        false
    }

    /// Run again: in its own file at the statement when the file still has it (that file's
    /// connection decides, as for any run), else inserted as Insert does and run there.
    fn history_run(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        let e = self.history[i].clone();
        let path = self.folder().join(&e.file);
        // The recorded file becomes the current one (when it exists), so a missing statement is inserted there.
        let line = path.is_file().then(|| self.open(path.clone(), None, window, cx)).flatten().and_then(|ix| {
            let d = &self.docs[ix];
            let src = d.editor.read(cx).value();
            d.file.statements.iter().find(|s| s.text(&src).trim() == e.sql.trim()).map(|s| s.line)
        });
        match line {
            Some(line) => self.go_to(path.clone(), line, false, window, cx),
            None if !self.history_insert(i, window, cx) => return,
            None => {}
        }
        self.show_history = false;
        // (A file that is gone was already noted by Insert.)
        let now = self.doc().and_then(|d| d.connection().map(str::to_string));
        if path.is_file() && now.as_deref() != Some(e.connection.as_str()) {
            window.push_notification(format!("Recorded on `{}`; runs on this file's connection `{}`", e.connection, now.as_deref().unwrap_or("none")), cx);
        }
        self.run_current(window, cx);
        cx.notify();
    }

    pub(crate) fn history_page(&mut self, cx: &mut Context<Self>) -> AnyElement {
        self.history_rows = self.history_matches(cx);
        let t = cx.theme();
        let muted = t.muted_foreground;
        let count = match (self.history_rows.len(), self.history.len()) {
            (shown, all) if shown == all => format!("{all} runs"),
            (shown, all) => format!("{shown} of {all}"),
        };
        let header = div()
            .flex_none()
            .flex()
            .items_center()
            .gap_3()
            .px_4()
            .py_2()
            .border_b_1()
            .border_color(t.border)
            .child(div().text_lg().font_weight(FontWeight::BOLD).child("History"))
            .child(div().text_xs().text_color(muted).child(count))
            .child(div().flex_1())
            .child(Input::new(&self.history_filter).small().w(px(280.)))
            .child(Button::new("history-refresh").icon(IconName::RefreshCw).ghost().small().tooltip("Reload").on_click(cx.listener(|this, _, _, cx| {
                this.reload_history();
                cx.notify();
            })))
            .child(Button::new("history-close").label("Close").small().on_click(cx.listener(|this, _, window, cx| this.toggle_history(window, cx))));
        let body = if self.history_rows.is_empty() {
            let text = if self.history.is_empty() { "No runs in this project yet. Cmd+Enter a statement to start." } else { "Nothing matches the filter." };
            div().flex_1().flex().items_center().justify_center().text_sm().text_color(muted).child(text).into_any_element()
        } else {
            let list = uniform_list("history", self.history_rows.len(), cx.processor(|this, range: Range<usize>, _, cx| range.map(|ix| this.history_row(ix, cx)).collect::<Vec<_>>()))
                .track_scroll(&self.history_scroll);
            div().flex_1().min_h_0().flex().flex_col().pt_2().child(self.history_head(cx)).child(div().flex_1().min_h_0().child(list.size_full())).into_any_element()
        };
        div().size_full().flex().flex_col().child(header).child(body).into_any_element()
    }

    fn history_head(&self, cx: &App) -> AnyElement {
        let t = cx.theme();
        let col = |w: f32, label: &'static str| div().w(px(w)).flex_none().child(label);
        div()
            .flex_none()
            .mx_2()
            .px_3()
            .pb_2()
            .mb_1()
            .flex()
            .items_center()
            .gap_3()
            .border_b_1()
            .border_color(t.border)
            .text_xs()
            .font_weight(FontWeight::BOLD)
            .text_color(t.muted_foreground)
            .child(col(TIME_W, "TIME"))
            .child(col(CONN_W, "CONNECTION"))
            .child(div().flex_1().min_w_0().child("STATEMENT"))
            .child(col(FILE_W, "FILE"))
            .child(col(OUTCOME_W, "RESULT"))
            .child(div().w(px(MS_W)).flex_none().text_right().child("MS"))
            .child(div().w(px(ACTIONS_W)).flex_none())
            .into_any_element()
    }

    fn history_row(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let i = self.history_rows[ix];
        let e = &self.history[i];
        let t = cx.theme();
        let (muted, hover, zebra) = (t.muted_foreground, t.list_hover, t.secondary.opacity(0.5));
        let mono = t.mono_font_family.clone();
        let outcome_color = if e.error.is_some() { t.danger } else { muted };
        let slow = if e.duration_ms >= 1000 { t.warning } else { muted };
        let conn = match &e.database {
            Some(db) => format!("{} · {db}", e.connection),
            None => e.connection.clone(),
        };
        let sql: String = e.sql.split_whitespace().collect::<Vec<_>>().join(" ");
        let insert = Button::new(("history-insert", ix)).icon(IconName::Plus).ghost().xsmall().tooltip("Insert into the editor").on_click(cx.listener(move |this, _, window, cx| {
            cx.stop_propagation();
            this.history_insert(i, window, cx);
        }));
        let run = Button::new(("history-run", ix)).icon(IconName::Play).ghost().xsmall().tooltip("Run again").on_click(cx.listener(move |this, _, window, cx| {
            cx.stop_propagation();
            this.history_run(i, window, cx);
        }));
        let row = div()
            .id(("history-row", ix))
            .px_3()
            .h(px(32.))
            .flex()
            .items_center()
            .gap_3()
            .rounded_md()
            .cursor_pointer()
            .when(ix % 2 == 1, |r| r.bg(zebra))
            .hover(move |s| s.bg(hover))
            .child(div().w(px(TIME_W)).flex_none().text_xs().font_family(mono.clone()).text_color(muted).child(e.time_label()))
            .child(div().w(px(CONN_W)).flex_none().truncate().text_xs().child(conn))
            .child(div().flex_1().min_w_0().truncate().text_sm().font_family(mono).child(sql))
            .child(div().w(px(FILE_W)).flex_none().truncate().text_xs().text_color(muted).child(format!("{}:{}", e.file, e.line)))
            .child(div().w(px(OUTCOME_W)).flex_none().truncate().text_xs().text_color(outcome_color).child(e.outcome()))
            .child(div().w(px(MS_W)).flex_none().text_right().text_xs().text_color(slow).child(e.duration_ms.to_string()))
            .child(div().w(px(ACTIONS_W)).flex_none().flex().child(insert).child(run))
            // Double click inserts the statement.
            .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| {
                if ev.click_count() >= 2 {
                    this.history_insert(i, window, cx);
                }
            }));
        div().w_full().px_2().child(row).into_any_element()
    }
}
