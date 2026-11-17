//! The full value of a cell (or a whole row) in a dialog: JSON formatted with highlighting,
//! folding and a JSONPath filter (the read-only viewer of Respite's response body), other text
//! wrapped, bytes as a hex dump with "Save to file".

use std::{cell::Cell, sync::Arc};

use db_exec::Value;
use gpui::{prelude::FluentBuilder as _, *};
use gpui_component::{
    ActiveTheme as _, Disableable as _, IconName, Sizable as _, WindowExt as _,
    button::{Button, ButtonGroup},
    h_flex,
    input::{Editor, EditorState, Input, InputEvent, InputState},
    table::TableState,
    v_flex,
};

use crate::{
    Siesta,
    format::{hexdump, row_json},
    results::{Done, Grid},
};

/// What the popup shows for a value.
#[derive(Debug, PartialEq)]
pub enum Body {
    /// Pretty-printed JSON (a JSON value, or text that parses as an object / array).
    Json(String),
    Text(String),
    /// Hex dump of the first 4 KB.
    Hex(String),
}

pub const HEX_LIMIT: usize = 4096;

pub fn body_of(v: &Value, null_text: &str) -> Body {
    match v {
        Value::Json(j) => Body::Json(serde_json::to_string_pretty(j).unwrap_or_default()),
        Value::Text(s) => match serde_json::from_str::<serde_json::Value>(s) {
            Ok(j @ (serde_json::Value::Object(_) | serde_json::Value::Array(_))) => Body::Json(serde_json::to_string_pretty(&j).unwrap_or_default()),
            _ => Body::Text(s.clone()),
        },
        Value::Bytes(b) => Body::Hex(hexdump(b, HEX_LIMIT)),
        Value::Null => Body::Text(null_text.to_string()),
        other => Body::Text(other.display()),
    }
}

/// Matches of the JSONPath `expr` in `text` as a pretty JSON array.
pub fn jsonpath(text: &str, expr: &str) -> Result<String, String> {
    let path = serde_json_path::JsonPath::parse(expr).map_err(|e| format!("JSONPath: {e}"))?;
    let json: serde_json::Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let found: Vec<_> = path.query(&json).all().into_iter().cloned().collect();
    serde_json::to_string_pretty(&found).map_err(|e| e.to_string())
}

pub struct Popup {
    done: Arc<Done>,
    /// Display order of the table's rows, and the position in it.
    order: Vec<usize>,
    pos: usize,
    /// `None`: the whole row as a JSON object.
    col: Option<usize>,
    /// Rows before this page, for "row N".
    offset: u64,
    null_text: String,
    table: Entity<TableState<Grid>>,
    editor: Entity<EditorState>,
    filter: Entity<InputState>,
    focus: FocusHandle,
    body: Body,
    /// What the editor shows now (the filtered JSON, or the body).
    shown: String,
    _subs: Vec<Subscription>,
}

impl Popup {
    #[allow(clippy::too_many_arguments)]
    fn new(done: Arc<Done>, order: Vec<usize>, pos: usize, col: Option<usize>, null_text: String, table: Entity<TableState<Grid>>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let editor = cx.new(|cx| {
            let mut s = EditorState::new(window, cx).line_number(false).soft_wrap(true).folding(true).searchable(true).replaceable(false);
            s.set_readonly(true, cx);
            s
        });
        let filter = cx.new(|cx| InputState::new(window, cx).placeholder("JSONPath filter, e.g. $.items[*].id"));
        let subs = vec![cx.subscribe_in(&filter, window, |this, _, ev: &InputEvent, window, cx| {
            if matches!(ev, InputEvent::Change) {
                this.apply_filter(window, cx);
            }
        })];
        let offset = done.page.offset;
        let mut p = Popup { done, order, pos, col, offset, null_text, table, editor, filter, focus: cx.focus_handle(), body: Body::Text(String::new()), shown: String::new(), _subs: subs };
        p.load(window, cx);
        p
    }

    fn row(&self) -> Option<&[Value]> {
        self.done.outcome()?.rows.get(*self.order.get(self.pos)?).map(Vec::as_slice)
    }

    /// `column · type · row N`, or `Row N`.
    pub fn title(&self) -> String {
        let n = self.offset as usize + self.pos + 1;
        match (self.col, self.done.outcome()) {
            (Some(c), Some(o)) => format!("{} · {} · row {n}", o.columns[c].name, o.columns[c].type_name),
            _ => format!("Row {n}"),
        }
    }

    /// The text shown, without a filter.
    pub fn source(&self) -> &str {
        match &self.body {
            Body::Json(s) | Body::Text(s) | Body::Hex(s) => s,
        }
    }

    #[cfg(test)]
    pub fn shown_text(&self) -> &str {
        &self.shown
    }

    fn is_json(&self) -> bool {
        matches!(self.body, Body::Json(_))
    }

    fn load(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(row), Some(o)) = (self.row(), self.done.outcome()) else { return };
        self.body = match self.col {
            Some(c) => body_of(&row[c], &self.null_text),
            None => Body::Json(row_json(&o.columns, row)),
        };
        let lang = if self.is_json() { "json" } else { "text" };
        self.shown = self.source().to_string();
        let text = self.shown.clone();
        self.editor.update(cx, |s, cx| {
            s.set_highlighter(SharedString::from(lang), cx);
            s.set_value(text, window, cx);
        });
        self.filter.update(cx, |s, cx| s.set_value("", window, cx));
        cx.notify();
    }

    fn apply_filter(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_json() {
            return;
        }
        let expr = self.filter.read(cx).value().to_string();
        self.shown = if expr.trim().is_empty() { self.source().to_string() } else { jsonpath(self.source(), expr.trim()).unwrap_or_else(|e| format!("// {e}")) };
        let text = self.shown.clone();
        self.editor.update(cx, |s, cx| s.set_value(text, window, cx));
        cx.notify();
    }

    /// ↑ / ↓: the same column of the previous / next row; the table's selection follows.
    fn step(&mut self, delta: isize, window: &mut Window, cx: &mut Context<Self>) {
        let pos = self.pos as isize + delta;
        if pos < 0 || pos as usize >= self.order.len() {
            return;
        }
        self.pos = pos as usize;
        let (pos, col) = (self.pos, self.col.unwrap_or(0));
        self.table.update(cx, |t, cx| {
            t.set_selected_cell(pos, col, cx);
            t.scroll_to_row(pos, cx);
        });
        self.load(window, cx);
    }

    fn copy(&self, window: &mut Window, cx: &mut Context<Self>) {
        cx.write_to_clipboard(ClipboardItem::new_string(self.shown.clone()));
        window.push_notification("Copied", cx);
    }

    fn save(&self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(row), Some(c)) = (self.row(), self.col) else { return };
        let Value::Bytes(bytes) = row[c].clone() else { return };
        let dir = std::env::home_dir().unwrap_or_default();
        let picked = cx.prompt_for_new_path(&dir, Some("value.bin"));
        cx.spawn_in(window, async move |_, cx| {
            let Ok(Ok(Some(path))) = picked.await else { return };
            let msg = match std::fs::write(&path, &bytes) {
                Ok(()) => format!("Saved {}", path.display()),
                Err(e) => format!("{}: {e}", path.display()),
            };
            cx.update(|window, cx| window.push_notification(msg, cx)).ok();
        })
        .detach();
    }
}

impl Render for Popup {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let h = window.viewport_size().height * 0.6;
        let last = self.order.len().saturating_sub(1);
        let bytes = matches!(self.body, Body::Hex(_));
        let wide = self.is_json();
        v_flex()
            .key_context("ValuePopup")
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &crate::PopupPrev, window, cx| this.step(-1, window, cx)))
            .on_action(cx.listener(|this, _: &crate::PopupNext, window, cx| this.step(1, window, cx)))
            .gap_2()
            .child(
                h_flex()
                    .gap_2()
                    .items_center()
                    // Room for the dialog's own close button in the top-right corner.
                    .pr_8()
                    .child(div().flex_1().min_w_0().truncate().font_weight(FontWeight::SEMIBOLD).child(self.title()))
                    .child(
                        ButtonGroup::new("popup-actions")
                            .outline()
                            .small()
                            .when(bytes, |g| g.child(Button::new("popup-save").label("Save to file").on_click(cx.listener(|this, _, window, cx| this.save(window, cx)))))
                            .child(Button::new("popup-copy").icon(IconName::Copy).label("Copy").on_click(cx.listener(|this, _, window, cx| this.copy(window, cx))))
                            .child(Button::new("popup-prev").icon(IconName::ArrowUp).disabled(self.pos == 0).tooltip("Previous row (↑)").on_click(cx.listener(|this, _, window, cx| this.step(-1, window, cx))))
                            .child(Button::new("popup-next").icon(IconName::ArrowDown).disabled(self.pos >= last).tooltip("Next row (↓)").on_click(cx.listener(|this, _, window, cx| this.step(1, window, cx)))),
                    ),
            )
            .child(div().h(h).border_1().border_color(cx.theme().border).rounded_md().child(Editor::new(&self.editor).size_full().bordered(false)))
            .when(wide, |c| c.child(div().flex_none().child(Input::new(&self.filter).small().cleanable(true))))
            .child(div().text_xs().text_color(muted).child("↑ / ↓ walk the column, Esc closes"))
    }
}

impl Siesta {
    /// Open the value at display position (`row`, `col`) of the shown result; `col == None` shows
    /// the whole row as a JSON object.
    pub(crate) fn open_popup(&mut self, row: usize, col: Option<usize>, window: &mut Window, cx: &mut Context<Self>) {
        let table = self.ui.table.clone();
        let g = table.read(cx).delegate();
        let (Some(done), true) = (g.done.clone(), row < g.order.len()) else { return };
        if done.outcome().is_none_or(|o| col.is_some_and(|c| c >= o.columns.len())) {
            return;
        }
        let (order, null_text) = (g.order.clone(), self.settings.settings.results.null_text.clone());
        let popup = cx.new(|cx| Popup::new(done, order, row, col, null_text, table.clone(), window, cx));
        self.ui.popup = Some(popup.clone());
        let first = Cell::new(true);
        window.open_dialog(cx, move |dialog, window, cx| {
            if first.replace(false) {
                let focus = popup.read(cx).focus.clone();
                window.focus(&focus, cx);
            }
            dialog.w(px(900.)).child(popup.clone())
        });
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: it brings `gpui::test`, which shadows `#[test]`.
    use super::{Body, body_of, jsonpath};
    use db_exec::Value;

    #[test]
    fn bodies() {
        let json = serde_json::json!({"a": [1, 2], "b": {"c": null}});
        let pretty = serde_json::to_string_pretty(&json).unwrap();
        assert_eq!(body_of(&Value::Json(json), "NULL"), Body::Json(pretty.clone()));
        assert_eq!(pretty, "{\n  \"a\": [\n    1,\n    2\n  ],\n  \"b\": {\n    \"c\": null\n  }\n}");
        // Text that is JSON gets the JSON view; a number or a word does not.
        assert_eq!(body_of(&Value::Text("{\"a\":1}".into()), "NULL"), Body::Json("{\n  \"a\": 1\n}".into()));
        assert_eq!(body_of(&Value::Text("[1]".into()), "NULL"), Body::Json("[\n  1\n]".into()));
        assert_eq!(body_of(&Value::Text("123".into()), "NULL"), Body::Text("123".into()));
        assert_eq!(body_of(&Value::Text("plain\ntext".into()), "NULL"), Body::Text("plain\ntext".into()));
        assert_eq!(body_of(&Value::Null, "∅"), Body::Text("∅".into()));
        assert!(matches!(body_of(&Value::Bytes(vec![1, 2, 3]), "NULL"), Body::Hex(h) if h.starts_with("00000000  01 02 03")));
    }

    #[test]
    fn json_path() {
        let text = "{\"items\": [{\"id\": 1}, {\"id\": 2}]}";
        assert_eq!(jsonpath(text, "$.items[*].id").unwrap(), "[\n  1,\n  2\n]");
        assert!(jsonpath(text, "$[").unwrap_err().starts_with("JSONPath"));
    }
}
