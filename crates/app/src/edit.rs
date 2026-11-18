//! Editing a result: the "Edit" button of the result header finds where the columns come from
//! (`Session::source`: the server's row description, not a parser) and the primary key of those
//! tables. Columns of a table whose whole key is in the result can be changed (double-click,
//! Enter); the changes wait, highlighted, until "Save…" shows the `UPDATE … WHERE <key>`
//! statements for confirmation and runs them in one transaction. When no column can be edited,
//! the bar says why (no table, no primary key, key not in the result).

use std::{collections::BTreeMap, rc::Rc, sync::Arc};

use db_exec::{Column, Source, SourceTable, Value, quote_ident};
use gpui::{prelude::FluentBuilder as _, *};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    checkbox::Checkbox,
    h_flex,
    input::{Editor, EditorState, InputEvent, InputState},
    v_flex,
};
use sql_split::{Dialect, Kind};

use crate::{Siesta, format::literal, results::Key};

/// Which columns of a result can be edited, and how to find their rows.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub tables: Vec<SourceTable>,
    /// Per result column: `(table, the table's column)` when it can be edited.
    pub cols: Vec<Option<(usize, String)>>,
    /// Per table: the result columns holding its primary key (empty: the table is read-only).
    pub keys: Vec<Vec<usize>>,
    /// Why some columns stay read-only (shown muted in the bar).
    pub note: Option<String>,
}

fn binary(type_name: &str) -> bool {
    let t = type_name.to_ascii_uppercase();
    t == "BYTEA" || t.contains("BLOB") || t.contains("BINARY")
}

/// Text longer than this is a memo: it is edited in a window, not in the cell.
pub const MEMO_CHARS: usize = 200;

/// A value too big for the cell, edited in its own window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Big {
    /// Pretty-printed, with JSON highlighting.
    Json,
    /// Multi-line or long text.
    Memo,
    /// Bytes: a hex dump, loaded from and saved to a file.
    Blob,
}

/// Whether the value of a cell (its pending text, else the result's value) needs a window.
pub fn big(v: &Value, type_name: &str, pending: Option<&NewValue>) -> Option<Big> {
    let ty = type_name.to_ascii_lowercase();
    if matches!(v, Value::Bytes(_)) || binary(type_name) || matches!(pending, Some(NewValue::Bytes(_))) {
        return Some(Big::Blob);
    }
    if matches!(v, Value::Json(_)) || ty.contains("json") {
        return Some(Big::Json);
    }
    let text = match pending {
        Some(NewValue::Text(s)) => s.clone(),
        _ => v.display(),
    };
    (text.contains('\n') || text.chars().count() > MEMO_CHARS).then_some(Big::Memo)
}

/// A pending value of a cell.
#[derive(Debug, Clone, PartialEq)]
pub enum NewValue {
    /// Sent as a string literal; the server converts it to the column type.
    Text(String),
    Bytes(Vec<u8>),
}

impl NewValue {
    /// The result's value as a pending one (NULL is `None`).
    pub fn of(v: &Value) -> Option<NewValue> {
        match v {
            Value::Null => None,
            Value::Bytes(b) => Some(NewValue::Bytes(b.clone())),
            other => Some(NewValue::Text(other.display())),
        }
    }

    #[cfg(test)]
    pub fn text(s: &str) -> Option<NewValue> {
        Some(NewValue::Text(s.into()))
    }

    pub fn as_value(&self) -> Value {
        match self {
            NewValue::Text(s) => Value::Text(s.clone()),
            NewValue::Bytes(b) => Value::Bytes(b.clone()),
        }
    }
}

fn qualified(d: Dialect, t: &SourceTable) -> String {
    if t.schema.is_empty() { quote_ident(d, &t.name) } else { format!("{}.{}", quote_ident(d, &t.schema), quote_ident(d, &t.name)) }
}

/// `schema.table` as the bar shows it.
pub fn table_label(t: &SourceTable) -> String {
    if t.schema.is_empty() { t.name.clone() } else { format!("{}.{}", t.schema, t.name) }
}

/// What can be edited, or why nothing can.
pub fn plan(columns: &[Column], src: &Source) -> Result<Target, String> {
    if src.columns.len() != columns.len() {
        return Err("the statement's columns changed since it ran: run it again".into());
    }
    let mut reasons = vec![];
    let mut keys = vec![];
    for (ti, t) in src.tables.iter().enumerate() {
        if t.primary_key.is_empty() {
            reasons.push(format!("{} has no primary key", table_label(t)));
            keys.push(vec![]);
            continue;
        }
        let found: Vec<Option<usize>> = t.primary_key.iter().map(|k| src.columns.iter().position(|c| c.as_ref().is_some_and(|(i, n)| *i == ti && n == k))).collect();
        match t.primary_key.iter().zip(&found).find(|(_, f)| f.is_none()) {
            Some((k, _)) => {
                reasons.push(format!("the primary key column `{k}` of {} is not in the result", table_label(t)));
                keys.push(vec![]);
            }
            None => keys.push(found.into_iter().flatten().collect()),
        }
    }
    let mut cols: Vec<Option<(usize, String)>> = vec![None; columns.len()];
    for (i, c) in src.columns.iter().enumerate() {
        let Some((ti, name)) = c else { continue };
        // The same table column twice (`SELECT id, id`): only the first one is edited.
        let repeated = src.columns[..i].contains(c);
        if !keys[*ti].is_empty() && !repeated {
            cols[i] = Some((*ti, name.clone()));
        }
    }
    if cols.iter().all(Option::is_none) {
        if src.tables.is_empty() {
            return Err("the columns don't come from a table (expressions, functions or a subquery)".into());
        }
        if reasons.is_empty() {
            reasons.push("only repeated columns come from the table".into());
        }
        return Err(reasons.join("; "));
    }
    let expressions = src.columns.iter().any(Option::is_none);
    if expressions {
        reasons.push("computed columns are read-only".into());
    }
    let note = (!reasons.is_empty()).then(|| reasons.join("; "));
    Ok(Target { tables: src.tables.clone(), cols, keys, note })
}

/// Pending new values per (row of the result, column); `None` is SQL NULL.
pub type Changes = BTreeMap<(usize, usize), Option<NewValue>>;

/// One `UPDATE … SET … WHERE <key> = <value as it was>;` per changed row and table. New text is a
/// string literal (the server converts it to the column type), new bytes a hex literal; keys keep
/// their own type.
pub fn update_sql(d: Dialect, t: &Target, rows: &[Vec<Value>], changes: &Changes) -> Result<Vec<String>, String> {
    // (row, table) → its changed columns
    let mut groups: BTreeMap<(usize, usize), Vec<usize>> = BTreeMap::new();
    for &(row, col) in changes.keys() {
        if let Some(Some((ti, _))) = t.cols.get(col) {
            groups.entry((row, *ti)).or_default().push(col);
        }
    }
    let mut out = vec![];
    for ((row, ti), sets) in groups {
        let r = rows.get(row).ok_or("the result changed: run it again")?;
        let assignments: Vec<String> = sets
            .iter()
            .map(|c| {
                let name = &t.cols[*c].as_ref().expect("editable").1;
                let value = changes[&(row, *c)].as_ref().map_or_else(|| "NULL".to_string(), |v| literal(&v.as_value(), d));
                format!("{} = {value}", quote_ident(d, name))
            })
            .collect();
        let table = &t.tables[ti];
        let mut cond = vec![];
        for (&k, name) in t.keys[ti].iter().zip(&table.primary_key) {
            if matches!(r[k], Value::Null) {
                return Err(format!("row {}: the key `{name}` of {} is NULL, so there is no such row", row + 1, table_label(table)));
            }
            cond.push(format!("{} = {}", quote_ident(d, name), literal(&r[k], d)));
        }
        out.push(format!("UPDATE {}\nSET {}\nWHERE {};", qualified(d, table), assignments.join(",\n    "), cond.join("\n  AND ")));
    }
    Ok(out)
}

/// The cell being edited in place.
pub struct Inline {
    /// Display row (the table's) and row of the result.
    pub row: usize,
    pub ri: usize,
    pub col: usize,
    pub input: Entity<InputState>,
    /// What the cell held before this edit (a pending change, or the result's value).
    current: Option<String>,
    /// The result's own value: editing back to it removes the change.
    original: Option<String>,
    _subs: Vec<Subscription>,
}

pub enum State {
    /// Asking the server where the columns come from.
    Checking,
    /// Nothing can be edited: why.
    Failed(String),
    Ready(Arc<Target>),
}

/// Edit mode of one statement's result.
pub struct Edit {
    pub key: Key,
    pub state: State,
    /// The result (entry id) the changes belong to: a new page or run drops them.
    pub id: u64,
    pub changes: Changes,
    /// The last save failed: why.
    pub error: Option<String>,
    pub saving: bool,
    pub inline: Option<Inline>,
}

impl Edit {
    pub fn target(&self) -> Option<&Arc<Target>> {
        match &self.state {
            State::Ready(t) => Some(t),
            _ => None,
        }
    }
}

enum Field {
    /// JSON (pretty-printed) and memo text.
    Text(Entity<EditorState>),
    /// Bytes, shown as a read-only hex dump; replaced from a file.
    Blob { bytes: Vec<u8>, dump: Entity<EditorState> },
}

type Apply = Rc<dyn Fn(Option<NewValue>, &mut Window, &mut App)>;

/// The window of a big value (JSON, memo, blob): the value and NULL.
pub struct CellEdit {
    field: Field,
    null: bool,
}

fn dump_text(b: &[u8]) -> String {
    format!("{} bytes\n\n{}", b.len(), crate::format::hexdump(b, crate::value_popup::HEX_LIMIT))
}

impl CellEdit {
    fn new(value: Option<NewValue>, kind: Big, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let editor = |text: String, lang: &'static str, readonly: bool, window: &mut Window, cx: &mut Context<Self>| {
            cx.new(|cx| {
                let mut s = EditorState::new(window, cx).line_number(false).soft_wrap(!readonly).folding(false);
                s.set_highlighter(SharedString::from(lang), cx);
                s.set_value(text, window, cx);
                s.set_readonly(readonly, cx);
                s
            })
        };
        let field = match (kind, &value) {
            (Big::Blob, v) => {
                let bytes = match v {
                    Some(NewValue::Bytes(b)) => b.clone(),
                    Some(NewValue::Text(s)) => s.clone().into_bytes(),
                    None => vec![],
                };
                let dump = editor(dump_text(&bytes), "text", true, window, cx);
                Field::Blob { bytes, dump }
            }
            (_, v) => {
                let text = match v {
                    Some(NewValue::Text(s)) => s.clone(),
                    _ => String::new(),
                };
                // JSON is edited pretty-printed (text that does not parse stays as it is).
                let (text, lang) = match kind {
                    Big::Json => (serde_json::from_str::<serde_json::Value>(&text).ok().and_then(|j| serde_json::to_string_pretty(&j).ok()).unwrap_or(text), "json"),
                    _ => (text, "text"),
                };
                Field::Text(editor(text, lang, false, window, cx))
            }
        };
        CellEdit { field, null: value.is_none() }
    }

    fn value(&self, cx: &App) -> Option<NewValue> {
        if self.null {
            return None;
        }
        Some(match &self.field {
            Field::Text(e) => NewValue::Text(e.read(cx).value().to_string()),
            Field::Blob { bytes, .. } => NewValue::Bytes(bytes.clone()),
        })
    }

    fn focus(this: &Entity<Self>, window: &mut Window, cx: &mut App) {
        if let Field::Text(e) = &this.read(cx).field {
            let e = e.clone();
            e.update(cx, |s, cx| s.focus(window, cx));
        }
    }

    /// Blob: the bytes of a file replace the value.
    fn load_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: false, prompt: None });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else { return };
            let Some(path) = paths.into_iter().next() else { return };
            let read = std::fs::read(&path);
            this.update_in(cx, |this, window, cx| match read {
                Ok(b) => {
                    if let Field::Blob { bytes, dump } = &mut this.field {
                        dump.update(cx, |s, cx| s.set_value(dump_text(&b), window, cx));
                        *bytes = b;
                    }
                    this.null = false;
                    cx.notify();
                }
                Err(e) => window.push_notification(format!("{}: {e}", path.display()), cx),
            })
            .ok();
        })
        .detach();
    }

    /// Blob: the bytes to a file.
    fn save_file(&self, window: &mut Window, cx: &mut Context<Self>) {
        let Field::Blob { bytes, .. } = &self.field else { return };
        let bytes = bytes.clone();
        let picked = cx.prompt_for_new_path(&std::env::home_dir().unwrap_or_default(), Some("value.bin"));
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

impl Render for CellEdit {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (editor, readonly, blob) = match &self.field {
            Field::Text(e) => (e.clone(), false, false),
            Field::Blob { dump, .. } => (dump.clone(), true, true),
        };
        let field = div()
            .h(window.viewport_size().height * 0.55)
            .border_1()
            .border_color(cx.theme().border)
            .rounded_md()
            // The element's own `readonly` overrides the state's on every render.
            .child(Editor::new(&editor).readonly(readonly).size_full().bordered(false));
        let actions = h_flex()
            .gap_3()
            .items_center()
            .child(Checkbox::new("cell-null").label("NULL").checked(self.null).on_click(cx.listener(|this, v: &bool, _, cx| {
                this.null = *v;
                cx.notify();
            })))
            .when(blob, |row| {
                row.child(Button::new("blob-load").label("Load from file…").small().on_click(cx.listener(|this, _, window, cx| this.load_file(window, cx))))
                    .child(Button::new("blob-save").label("Save to file…").small().on_click(cx.listener(|this, _, window, cx| this.save_file(window, cx))))
            });
        v_flex().gap_2().child(div().when(self.null, |d| d.opacity(0.4)).child(field)).child(actions)
    }
}

impl Siesta {
    /// The edit mode of the shown result, if it is on.
    pub(crate) fn shown_edit(&self, cx: &App) -> Option<&Edit> {
        let key = self.shown_key(cx)?;
        self.ui.edit.as_ref().filter(|e| e.key == key)
    }

    /// Pending changes of the shown result (page, sort and filter changes would drop them).
    pub(crate) fn pending_changes(&self, cx: &App) -> usize {
        self.shown_edit(cx).map_or(0, |e| e.changes.len())
    }

    /// Changes belong to one result: a new page or run of the statement drops them. The table
    /// gets the editable columns and the pending values to draw.
    pub(crate) fn sync_edit(&mut self, cx: &mut Context<Self>) {
        let key = self.shown_key(cx);
        let id = key.as_ref().and_then(|k| self.store.entries.get(k)).map(|e| e.id);
        let (mut editable, mut changes, mut editing) = (vec![], Changes::new(), None);
        if let Some(e) = self.ui.edit.as_mut().filter(|e| Some(&e.key) == key.as_ref()) {
            if Some(e.id) != id {
                e.id = id.unwrap_or(0);
                e.changes.clear();
                e.error = None;
                e.inline = None;
            }
            if let State::Ready(t) = &e.state {
                editable = t.cols.iter().map(Option::is_some).collect();
            }
            changes = e.changes.clone();
            editing = e.inline.as_ref().map(|i| (i.row, i.col, i.input.clone()));
        }
        self.ui.table.update(cx, |t, cx| {
            let g = t.delegate_mut();
            if g.editable != editable || g.changes != changes || g.editing != editing {
                (g.editable, g.changes, g.editing) = (editable, changes, editing);
                cx.notify();
            }
        });
    }

    /// The header's Edit button: find the table and key of the shown result, or leave edit mode.
    pub(crate) fn toggle_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = self.shown_key(cx) else { return };
        if self.ui.edit.as_ref().is_some_and(|e| e.key == key) {
            let n = self.pending_changes(cx);
            if n == 0 {
                self.ui.edit = None;
                return cx.notify();
            }
            let answer = window.prompt(PromptLevel::Warning, &format!("Discard {n} unsaved change{}?", if n == 1 { "" } else { "s" }), None, &["Discard", "Cancel"], cx);
            cx.spawn_in(window, async move |this, cx| {
                if answer.await == Ok(0) {
                    this.update(cx, |this, cx| {
                        this.ui.edit = None;
                        cx.notify();
                    })
                    .ok();
                }
            })
            .detach();
            return;
        }
        let Some(e) = self.store.entries.get(&key) else { return };
        let Some(done) = e.done.clone() else { return };
        let id = e.id;
        let mut edit = Edit { key: key.clone(), state: State::Checking, id, changes: Changes::new(), error: None, saving: false, inline: None };
        let conn = self.docs.iter().find(|d| *d.path == key.0).and_then(|d| d.conns.connections.get(&done.conn).map(|c| c.with_database(done.database.as_deref())));
        let columns = done.outcome().map(|o| o.columns.clone()).unwrap_or_default();
        edit.state = match conn {
            _ if done.kind != Kind::Query || columns.is_empty() => State::Failed("only the rows of a SELECT can be edited".into()),
            None => State::Failed(format!("the connection `{}` is gone", done.conn)),
            Some(c) if c.read_only => State::Failed(format!("the connection `{}` is read-only", done.conn)),
            Some(c) => {
                let handle = self.source_job(&key.0, &done.conn, c, done.text.clone());
                cx.spawn_in(window, async move |this, cx| {
                    let r = handle.await.unwrap_or_else(|e| Err(db_exec::DbError { message: e.to_string(), ..Default::default() }));
                    this.update(cx, |this, cx| {
                        if let Some(e) = this.ui.edit.as_mut().filter(|e| e.key == key) {
                            e.state = match r {
                                Ok(src) => plan(&columns, &src).map_or_else(State::Failed, |t| State::Ready(Arc::new(t))),
                                Err(err) => State::Failed(format!("the server could not describe the statement: {err}")),
                            };
                        }
                        cx.notify();
                    })
                    .ok();
                })
                .detach();
                State::Checking
            }
        };
        self.ui.edit = Some(edit);
        cx.notify();
    }

    /// Double-click / Enter on a cell in edit mode: an editable cell turns into a field in place
    /// (Enter or leaving the field keeps the value, Esc cancels); a big value (JSON, memo, blob)
    /// opens in its own window. `false`: not in edit mode or the column is read-only (the viewer
    /// opens instead).
    pub(crate) fn edit_cell(&mut self, row: usize, col: usize, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(target) = self.shown_edit(cx).and_then(|e| e.target().cloned()) else { return false };
        if target.cols.get(col).is_none_or(Option::is_none) {
            return false;
        }
        // Another cell is open: it keeps its value first.
        self.commit_inline(window, cx);
        let g = self.ui.table.read(cx).delegate();
        let (Some(&ri), Some(o)) = (g.order.get(row), g.outcome()) else { return true };
        let column = o.columns[col].clone();
        let value = o.rows[ri][col].clone();
        let offset = g.done.as_ref().map_or(0, |d| d.page.offset);
        let original = NewValue::of(&value);
        let pending = self.shown_edit(cx).and_then(|e| e.changes.get(&(ri, col)).cloned());
        if let Some(kind) = big(&value, &column.type_name, pending.as_ref().and_then(Option::as_ref)) {
            let current = pending.unwrap_or_else(|| original.clone());
            self.edit_in_window(ri, col, &column, kind, offset as usize + row + 1, current, original, window, cx);
            return true;
        }
        let text = |v: Option<NewValue>| match v {
            Some(NewValue::Text(s)) => Some(s),
            Some(NewValue::Bytes(_)) | None => None,
        };
        let original = text(original);
        let current = pending.map_or_else(|| original.clone(), text);
        let input = cx.new(|cx| InputState::new(window, cx).default_value(current.clone().unwrap_or_default()).placeholder(if current.is_none() { "NULL" } else { "" }));
        // Enter, Esc, Tab and ↑ / ↓ are taken by the cell before the field sees them (`results::edit_keys`).
        let subs = vec![cx.subscribe_in(&input, window, |_, _, ev: &InputEvent, window, cx| {
            if matches!(ev, InputEvent::Blur) {
                // Deferred: the field's own event is still being delivered.
                cx.defer_in(window, |this, window, cx| this.commit_inline(window, cx));
            }
        })];
        input.update(cx, |s, cx| {
            s.focus(window, cx);
            s.select_all(window, cx);
        });
        if let Some(e) = self.ui.edit.as_mut() {
            e.inline = Some(Inline { row, ri, col, input, current, original, _subs: subs });
        }
        cx.notify();
        true
    }

    /// Keep the in-place field's value (an untouched NULL stays NULL) and give the table its focus back.
    pub(crate) fn commit_inline(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(i) = self.ui.edit.as_mut().and_then(|e| e.inline.take()) else { return };
        let text = i.input.read(cx).value().to_string();
        let value = if text.is_empty() && i.current.is_none() { None } else { Some(NewValue::Text(text)) };
        self.set_change(i.ri, i.col, value, i.original.map(NewValue::Text), cx);
        self.focus_table(window, cx);
    }

    /// Esc in the field: the cell keeps what it had.
    pub(crate) fn cancel_inline(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.ui.edit.as_mut().and_then(|e| e.inline.take()).is_some() {
            self.focus_table(window, cx);
            cx.notify();
        }
    }

    fn focus_table(&self, window: &mut Window, cx: &mut Context<Self>) {
        let focus = self.ui.table.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
    }

    /// Context menu in edit mode: the selected cell becomes NULL (`null`) or gets its value back.
    pub(crate) fn set_selected(&mut self, null: bool, _: usize, _: &mut Window, cx: &mut Context<Self>) {
        let Some((row, col)) = self.ui.table.read(cx).selected_cell() else { return };
        if self.shown_edit(cx).and_then(|e| e.target()).is_none_or(|t| t.cols.get(col).is_none_or(Option::is_none)) {
            return;
        }
        let g = self.ui.table.read(cx).delegate();
        let (Some(&ri), Some(o)) = (g.order.get(row), g.outcome()) else { return };
        let original = NewValue::of(&o.rows[ri][col]);
        let value = if null { None } else { original.clone() };
        self.set_change(ri, col, value, original, cx);
    }

    /// The window of a big value: JSON (pretty-printed, compared by content, so reformatting alone
    /// is no change), memo text, or a blob (hex dump, Load from file… / Save to file…).
    #[allow(clippy::too_many_arguments)]
    fn edit_in_window(&mut self, ri: usize, col: usize, column: &Column, kind: Big, n: usize, current: Option<NewValue>, original: Option<NewValue>, window: &mut Window, cx: &mut Context<Self>) {
        let this = cx.entity().downgrade();
        let apply: Apply = Rc::new(move |value, window, cx| {
            window.close_dialog(cx);
            let parse = |v: &Option<NewValue>| match v {
                Some(NewValue::Text(s)) => serde_json::from_str::<serde_json::Value>(s).ok(),
                _ => None,
            };
            let same_json = kind == Big::Json && parse(&value).is_some() && parse(&value) == parse(&original);
            let value = if same_json { original.clone() } else { value };
            let (this, original) = (this.clone(), original.clone());
            window.defer(cx, move |_, cx| {
                this.update(cx, |this, cx| this.set_change(ri, col, value, original, cx)).ok();
            });
        });
        let form = cx.new(|cx| CellEdit::new(current, kind, window, cx));
        let title = format!("{} · {} · row {n}", column.name, column.type_name);
        let field = form.clone();
        let width = (window.viewport_size().width * 0.8).min(px(1200.));
        window.open_dialog(cx, move |dialog, _, _| {
            let (apply, read) = (apply.clone(), field.clone());
            dialog.title(title.clone()).w(width).child(field.clone()).footer(
                h_flex()
                    .w_full()
                    .justify_end()
                    .gap_2()
                    .child(Button::new("cell-cancel").label("Cancel").on_click(|_, window, cx| window.close_dialog(cx)))
                    .child(Button::new("cell-set").label("Set").primary().on_click(move |_, window, cx| {
                        let v = read.read(cx).value(cx);
                        apply(v, window, cx)
                    })),
            )
        });
        CellEdit::focus(&form, window, cx);
    }

    fn set_change(&mut self, row: usize, col: usize, value: Option<NewValue>, original: Option<NewValue>, cx: &mut Context<Self>) {
        let Some(key) = self.shown_key(cx) else { return };
        let Some(e) = self.ui.edit.as_mut().filter(|e| e.key == key) else { return };
        if value == original {
            e.changes.remove(&(row, col));
        } else {
            e.changes.insert((row, col), value);
        }
        e.error = None;
        cx.notify();
    }

    fn discard_changes(&mut self, cx: &mut Context<Self>) {
        if let Some(e) = self.ui.edit.as_mut() {
            e.changes.clear();
            e.error = None;
        }
        cx.notify();
    }

    /// "Save…": the UPDATE statements in a confirmation, then run them in one transaction.
    fn review_changes(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = self.shown_key(cx) else { return };
        let Some(done) = self.store.entries.get(&key).and_then(|e| e.done.clone()) else { return };
        let Some(e) = self.ui.edit.as_mut().filter(|e| e.key == key) else { return };
        let (Some(t), Some(o)) = (e.target().cloned(), done.outcome()) else { return };
        let sql = match update_sql(done.dialect(), &t, &o.rows, &e.changes) {
            Ok(s) => s,
            Err(err) => {
                e.error = Some(err);
                return cx.notify();
            }
        };
        let Some(conn) = self.docs.iter().find(|d| *d.path == key.0).and_then(|d| d.conns.connections.get(&done.conn).map(|c| c.with_database(done.database.as_deref()))) else { return };
        let n = sql.len();
        let rows = if n == 1 { "1 row".to_string() } else { format!("{n} rows") };
        let message = if conn.prod { format!("Update {rows} on PRODUCTION {}?", done.label()) } else { format!("Update {rows} on {}?", done.label()) };
        let tx = if self.in_tx(&key.0) {
            "They run in the open transaction: COMMIT to keep them."
        } else {
            "They run in one transaction: if any of them does not change exactly one row, nothing is saved."
        };
        // The statements as they will run, highlighted, in a read-only editor.
        let viewer = cx.new(|cx| {
            let mut s = EditorState::new(window, cx).line_number(false).soft_wrap(false).folding(false).searchable(true).replaceable(false);
            s.set_highlighter(SharedString::from("sql"), cx);
            s.set_value(sql.join("\n\n"), window, cx);
            s.set_readonly(true, cx);
            s
        });
        let (prod, this) = (conn.prod, cx.entity().downgrade());
        let run = Rc::new(move |window: &mut Window, cx: &mut App| {
            window.close_dialog(cx);
            let (key, name, conn, sql) = (key.clone(), done.conn.clone(), conn.clone(), sql.clone());
            this.update(cx, |this, cx| this.save_changes(key, name, conn, sql, window, cx)).ok();
        });
        let viewport = window.viewport_size();
        window.open_dialog(cx, move |dialog, _, cx| {
            let t = cx.theme();
            let run = run.clone();
            dialog
                .title(message.clone())
                .w((viewport.width * 0.8).min(px(1200.)))
                .child(
                    v_flex()
                        .gap_2()
                        .child(div().h(viewport.height * 0.55).border_1().border_color(t.border).rounded_md().child(Editor::new(&viewer).readonly(true).size_full().bordered(false)))
                        .child(div().text_sm().text_color(if prod { t.danger } else { t.muted_foreground }).child(tx)),
                )
                .footer(
                    h_flex()
                        .w_full()
                        .justify_end()
                        .gap_2()
                        .child(Button::new("update-cancel").label("Cancel").on_click(|_, window, cx| window.close_dialog(cx)))
                        .child(Button::new("update-run").label("Update").map(|b| if prod { b.danger() } else { b.primary() }).on_click(move |_, window, cx| run(window, cx))),
                )
        });
    }

    fn save_changes(&mut self, key: Key, name: String, conn: db_conn::Connection, sql: Vec<String>, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(e) = self.ui.edit.as_mut().filter(|e| e.key == key) {
            e.saving = true;
        }
        let database = conn.database.clone();
        let handle = self.apply_job(&key.0, &name, conn, sql);
        cx.spawn_in(window, async move |this, cx| {
            let report = handle.await.unwrap_or_else(|e| crate::runner::Applied { result: Err(e.to_string()), in_tx: false, ran: vec![] });
            this.update_in(cx, |this, window, cx| {
                this.applied(&key, &name, database, report, window, cx);
            })
            .ok();
        })
        .detach();
        cx.notify();
    }

    fn applied(&mut self, key: &Key, name: &str, database: Option<String>, report: crate::runner::Applied, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(slot) = self.slots.get_mut(&(key.0.clone(), name.to_string(), database.clone())) {
            slot.in_tx = report.in_tx;
        }
        let history = report
            .ran
            .iter()
            .map(|(sql, r)| db_exec::history::Entry {
                source: "gui".into(),
                connection: name.to_string(),
                database: database.clone(),
                sql: sql.clone(),
                duration_ms: r.as_ref().map_or(0, |o| o.duration.as_millis() as u64),
                affected: r.as_ref().ok().and_then(|o| o.rows_affected),
                error: r.as_ref().err().map(|e| e.to_string()),
                ..Default::default()
            })
            .collect();
        self.record_history(&key.0, history, cx);
        let Some(e) = self.ui.edit.as_mut().filter(|e| e.key == *key) else { return };
        e.saving = false;
        match report.result {
            Ok(n) => {
                e.changes.clear();
                e.error = None;
                window.push_notification(format!("Updated {n} row{}", if n == 1 { "" } else { "s" }), cx);
                // The page again, with the saved values.
                self.rerun(key, window, cx);
            }
            Err(err) => e.error = Some(err),
        }
        cx.notify();
    }

    /// The bar under the header in edit mode: what is edited (or why nothing can be), pending
    /// changes with Save… / Discard.
    pub(crate) fn edit_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let e = self.shown_edit(cx)?;
        let t = cx.theme();
        let (muted, warning, danger, fg) = (t.muted_foreground, t.warning, t.danger, t.foreground);
        let mut bar = h_flex().flex_none().gap_2().items_center().flex_wrap().px_3().py_1().border_b_1().border_color(t.border).bg(warning.opacity(0.08)).text_xs();
        let close = Button::new("edit-close").label("Done").ghost().xsmall().on_click(cx.listener(|this, _, window, cx| this.toggle_edit(window, cx)));
        bar = match &e.state {
            State::Checking => bar.child(div().text_color(muted).child("Finding the table and its primary key…")),
            State::Failed(why) => bar.child(div().flex_1().text_color(warning).child(format!("Can't edit: {why}."))).child(close),
            State::Ready(target) => {
                let tables: Vec<String> = target
                    .tables
                    .iter()
                    .zip(&target.keys)
                    .filter(|(_, k)| !k.is_empty())
                    .map(|(t, _)| format!("{} (key {})", table_label(t), t.primary_key.join(", ")))
                    .collect();
                let n = e.changes.len();
                let mut left = v_flex().flex_1().min_w_0().gap_0p5().child(div().text_color(fg).child(format!("Editing {} · double-click a cell to change it", tables.join(", "))));
                if let Some(note) = &target.note {
                    left = left.child(div().text_color(muted).child(format!("Read-only: {note}.")));
                }
                if let Some(err) = &e.error {
                    left = left.child(div().text_color(danger).child(err.clone()));
                }
                let mut row = bar.child(left);
                if n > 0 {
                    row = row
                        .child(div().text_color(warning).font_weight(FontWeight::SEMIBOLD).child(format!("{n} change{}", if n == 1 { "" } else { "s" })))
                        .child(Button::new("edit-discard").label("Discard").ghost().xsmall().disabled(e.saving).on_click(cx.listener(|this, _, _, cx| this.discard_changes(cx))))
                        .child(
                            Button::new("edit-save")
                                .label(if e.saving { "Saving…" } else { "Save…" })
                                .primary()
                                .xsmall()
                                .disabled(e.saving)
                                .tooltip("Review the UPDATE statements, then run them")
                                .on_click(cx.listener(|this, _, window, cx| this.review_changes(window, cx))),
                        );
                }
                row.child(close)
            }
        };
        Some(bar.into_any_element())
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: it brings `gpui::test`, which shadows `#[test]`.
    use super::{Big, Changes, NewValue, big, plan, update_sql};
    use db_exec::{Column, Source, SourceTable, Value};
    use sql_split::Dialect;

    fn col(name: &str, ty: &str) -> Column {
        Column { name: name.into(), type_name: ty.into() }
    }

    fn table(name: &str, key: &[&str]) -> SourceTable {
        SourceTable { schema: "public".into(), name: name.into(), primary_key: key.iter().map(|k| k.to_string()).collect() }
    }

    #[test]
    fn plan_finds_editable_columns_or_says_why() {
        let cols = [col("id", "int4"), col("n", "text"), col("upper", "text"), col("blob", "bytea")];
        let src = Source { columns: vec![Some((0, "id".into())), Some((0, "name".into())), None, Some((0, "data".into()))], tables: vec![table("users", &["id"])] };
        let t = plan(&cols, &src).unwrap();
        // Binary columns too (they are edited in the blob window).
        assert_eq!(t.cols, [Some((0, "id".to_string())), Some((0, "name".to_string())), None, Some((0, "data".to_string()))]);
        assert_eq!(t.keys, [vec![0]]);
        assert_eq!(t.note.as_deref(), Some("computed columns are read-only"));

        // No key, key not selected, no table at all.
        let src = Source { columns: vec![Some((0, "name".into()))], tables: vec![table("v", &[])] };
        assert_eq!(plan(&cols[1..2], &src).unwrap_err(), "public.v has no primary key");
        let src = Source { columns: vec![Some((0, "name".into()))], tables: vec![table("users", &["id"])] };
        assert_eq!(plan(&cols[1..2], &src).unwrap_err(), "the primary key column `id` of public.users is not in the result");
        let src = Source { columns: vec![None], tables: vec![] };
        assert!(plan(&cols[2..3], &src).unwrap_err().starts_with("the columns don't come from a table"));
        // A join: the table with its key in the result is editable, the other is explained.
        let cols = [col("id", "int4"), col("name", "text"), col("total", "numeric")];
        let src = Source { columns: vec![Some((0, "id".into())), Some((0, "name".into())), Some((1, "total".into()))], tables: vec![table("users", &["id"]), table("orders", &["id"])] };
        let t = plan(&cols, &src).unwrap();
        assert_eq!(t.cols[2], None);
        assert_eq!(t.note.as_deref(), Some("the primary key column `id` of public.orders is not in the result"));
    }

    #[test]
    fn updates_per_row_with_the_original_key() {
        let cols = [col("a", "int4"), col("b", "int4"), col("note", "text"), col("j", "jsonb")];
        let src = Source { columns: vec![Some((0, "a".into())), Some((0, "b".into())), Some((0, "note".into())), Some((0, "j".into()))], tables: vec![table("t", &["a", "b"])] };
        let t = plan(&cols, &src).unwrap();
        let rows = vec![vec![Value::Int(1), Value::Int(2), Value::Text("x".into()), Value::Null], vec![Value::Int(3), Value::Int(4), Value::Null, Value::Null]];
        let mut changes = Changes::new();
        changes.insert((0, 2), NewValue::text("it's"));
        changes.insert((0, 3), None);
        changes.insert((1, 0), NewValue::text("30"));
        let sql = update_sql(Dialect::Postgres, &t, &rows, &changes).unwrap();
        assert_eq!(
            sql,
            [
                "UPDATE \"public\".\"t\"\nSET \"note\" = 'it''s',\n    \"j\" = NULL\nWHERE \"a\" = 1\n  AND \"b\" = 2;",
                // A changed key column: the row is found by its old value.
                "UPDATE \"public\".\"t\"\nSET \"a\" = '30'\nWHERE \"a\" = 3\n  AND \"b\" = 4;",
            ]
        );
        let sql = update_sql(Dialect::MySql, &t, &rows, &changes).unwrap();
        assert_eq!(sql[0], "UPDATE `public`.`t`\nSET `note` = 'it''s',\n    `j` = NULL\nWHERE `a` = 1\n  AND `b` = 2;");
        // New bytes are a hex literal.
        let mut bytes = Changes::new();
        bytes.insert((0, 2), Some(NewValue::Bytes(vec![0xde, 0xad])));
        assert_eq!(update_sql(Dialect::Postgres, &t, &rows, &bytes).unwrap()[0], "UPDATE \"public\".\"t\"\nSET \"note\" = '\\xdead'\nWHERE \"a\" = 1\n  AND \"b\" = 2;");
        assert!(update_sql(Dialect::MySql, &t, &rows, &bytes).unwrap()[0].contains("`note` = X'dead'"));
        // A NULL key (outer join): no row to update.
        let rows = vec![vec![Value::Null, Value::Int(2), Value::Null, Value::Null]];
        let mut changes = Changes::new();
        changes.insert((0, 2), NewValue::text("x"));
        assert!(update_sql(Dialect::Postgres, &t, &rows, &changes).unwrap_err().contains("is NULL"));
    }

    #[test]
    fn cells_edit_as_text() {
        assert_eq!(NewValue::of(&Value::Null), None);
        assert_eq!(NewValue::of(&Value::Int(5)), Some(NewValue::Text("5".into())));
        assert_eq!(NewValue::of(&Value::Bytes(vec![1])), Some(NewValue::Bytes(vec![1])));
        // Big values open in a window: JSON, blobs, multi-line or long text.
        let text = |s: &str| Value::Text(s.into());
        assert_eq!(big(&Value::Json(serde_json::json!({"a": 1})), "jsonb", None), Some(Big::Json));
        assert_eq!(big(&Value::Null, "json", None), Some(Big::Json));
        assert_eq!(big(&Value::Bytes(vec![1]), "bytea", None), Some(Big::Blob));
        assert_eq!(big(&Value::Null, "LONGBLOB", None), Some(Big::Blob));
        assert_eq!(big(&text("a\nb"), "text", None), Some(Big::Memo));
        assert_eq!(big(&text(&"x".repeat(201)), "text", None), Some(Big::Memo));
        assert_eq!(big(&text("short"), "text", None), None);
        // A pending multi-line value keeps the window even for a short original.
        assert_eq!(big(&text("short"), "text", Some(&NewValue::Text("a\nb".into()))), Some(Big::Memo));
    }
}
