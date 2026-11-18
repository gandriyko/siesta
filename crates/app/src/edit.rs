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
    input::{Editor, EditorState, Input, InputEvent, InputState},
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

/// Binary values are not edited as text.
fn binary(type_name: &str) -> bool {
    let t = type_name.to_ascii_uppercase();
    t == "BYTEA" || t.contains("BLOB") || t.contains("BINARY")
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
        if !keys[*ti].is_empty() && !repeated && !binary(&columns[i].type_name) {
            cols[i] = Some((*ti, name.clone()));
        }
    }
    if cols.iter().all(Option::is_none) {
        if src.tables.is_empty() {
            return Err("the columns don't come from a table (expressions, functions or a subquery)".into());
        }
        if reasons.is_empty() {
            reasons.push("only binary or repeated columns come from the table".into());
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
pub type Changes = BTreeMap<(usize, usize), Option<String>>;

/// One `UPDATE … SET … WHERE <key> = <value as it was>;` per changed row and table. The new values
/// are string literals (the server converts them to the column type); keys keep their own type.
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
                let value = changes[&(row, *c)].as_ref().map_or_else(|| "NULL".to_string(), |s| literal(&Value::Text(s.clone()), d));
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

/// The text a cell is edited as (in the cell, so JSON is compact), NULL as `None`.
pub fn edit_text(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        other => Some(other.display()),
    }
}

/// Where a key in the in-place field goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Move {
    /// Enter: editing ends, the selection stays on the cell.
    Stay,
    Next,
    Prev,
    Up,
    Down,
}

/// What a key in the in-place field does: `Some(None)` cancels (Esc), `Some(Some(m))` moves,
/// `None` is the field's own. ← / → move the caret, and leave the cell only from the edge of the
/// text or when it is all selected (as it is when the field opens).
pub fn move_key(key: &str, shift: bool, plain: bool, selected: std::ops::Range<usize>, len: usize) -> Option<Option<Move>> {
    let all = selected.start == 0 && selected.end == len;
    Some(Some(match key {
        "escape" => return Some(None),
        "tab" if shift => Move::Prev,
        "tab" => Move::Next,
        // Enter toggles: it opened the field on this cell, it closes it here.
        "enter" => Move::Stay,
        "up" if plain => Move::Up,
        "down" if plain => Move::Down,
        "left" if plain && (all || selected.end == 0) => Move::Prev,
        "right" if plain && (all || selected.start == len) => Move::Next,
        _ => return None,
    }))
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
    Line(Entity<InputState>),
    Text(Entity<EditorState>),
}

type Apply = Rc<dyn Fn(Option<String>, &mut Window, &mut App)>;

/// The dialog of one cell: the value (one line, or an editor for multi-line text and JSON) and NULL.
pub struct CellEdit {
    field: Field,
    null: bool,
    _subs: Vec<Subscription>,
}

impl CellEdit {
    fn new(value: Option<String>, json: bool, apply: Apply, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let text = value.clone().unwrap_or_default();
        let mut subs = vec![];
        let field = if json || text.contains('\n') {
            let ed = cx.new(|cx| {
                let mut s = EditorState::new(window, cx).line_number(false).soft_wrap(true).folding(false);
                s.set_highlighter(SharedString::from(if json { "json" } else { "text" }), cx);
                s.set_value(text, window, cx);
                s
            });
            Field::Text(ed)
        } else {
            let input = cx.new(|cx| InputState::new(window, cx).default_value(text));
            subs.push(cx.subscribe_in(&input, window, move |this: &mut Self, _, ev: &InputEvent, window, cx| {
                if matches!(ev, InputEvent::PressEnter { .. }) {
                    apply(this.value(cx), window, cx);
                }
            }));
            Field::Line(input)
        };
        CellEdit { field, null: value.is_none(), _subs: subs }
    }

    fn value(&self, cx: &App) -> Option<String> {
        if self.null {
            return None;
        }
        Some(match &self.field {
            Field::Line(i) => i.read(cx).value().to_string(),
            Field::Text(e) => e.read(cx).value().to_string(),
        })
    }

    fn focus(this: &Entity<Self>, window: &mut Window, cx: &mut App) {
        let field = match &this.read(cx).field {
            Field::Line(i) => Field::Line(i.clone()),
            Field::Text(e) => Field::Text(e.clone()),
        };
        match &field {
            Field::Line(i) => i.update(cx, |s, cx| {
                s.focus(window, cx);
                s.select_all(window, cx);
            }),
            Field::Text(e) => e.update(cx, |s, cx| s.focus(window, cx)),
        }
    }
}

impl Render for CellEdit {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let field = match &self.field {
            Field::Line(i) => div().child(Input::new(i)).into_any_element(),
            Field::Text(e) => div()
                .h(window.viewport_size().height * 0.45)
                .border_1()
                .border_color(cx.theme().border)
                .rounded_md()
                .child(Editor::new(e).size_full().bordered(false))
                .into_any_element(),
        };
        v_flex()
            .gap_2()
            .child(div().when(self.null, |d| d.opacity(0.4)).child(field))
            .child(Checkbox::new("cell-null").label("NULL").checked(self.null).on_click(cx.listener(|this, v: &bool, _, cx| {
                this.null = *v;
                cx.notify();
            })))
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
    /// (Enter or leaving the field keeps the value, Esc cancels); multi-line text opens a dialog.
    /// `false`: not in edit mode or the column is read-only (the viewer opens instead).
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
        let original = edit_text(&o.rows[ri][col]);
        let offset = g.done.as_ref().map_or(0, |d| d.page.offset);
        let current = self.shown_edit(cx).and_then(|e| e.changes.get(&(ri, col)).cloned()).unwrap_or_else(|| original.clone());
        if current.as_deref().is_some_and(|s| s.contains('\n')) {
            self.edit_in_dialog(ri, col, &column, offset as usize + row + 1, current, original, window, cx);
            return true;
        }
        let input = cx.new(|cx| InputState::new(window, cx).default_value(current.clone().unwrap_or_default()).placeholder(if current.is_none() { "NULL" } else { "" }));
        // Enter, Tab and the arrows are taken by the cell before the field sees them (`move_key`).
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
        let value = if text.is_empty() && i.current.is_none() { None } else { Some(text) };
        self.set_change(i.ri, i.col, value, i.original, cx);
        self.focus_table(window, cx);
    }

    /// Enter / Tab / an arrow in the field: keep the value and end editing. Enter stays on the
    /// cell (it toggles editing); Tab walks the editable columns, wrapping to the next row; ↑ / ↓
    /// move in the column. Off the edge of the page the selection stays.
    pub(crate) fn move_inline(&mut self, m: Move, window: &mut Window, cx: &mut Context<Self>) {
        let Some((row, col)) = self.shown_edit(cx).and_then(|e| e.inline.as_ref()).map(|i| (i.row, i.col)) else { return };
        let Some(t) = self.shown_edit(cx).and_then(|e| e.target().cloned()) else { return };
        self.commit_inline(window, cx);
        let rows = self.ui.table.read(cx).delegate().order.len();
        let cols: Vec<usize> = t.cols.iter().enumerate().filter(|(_, c)| c.is_some()).map(|(i, _)| i).collect();
        let at = cols.iter().position(|&c| c == col).unwrap_or(0);
        let next = match m {
            Move::Stay => None,
            Move::Up => row.checked_sub(1).map(|r| (r, col)),
            Move::Down => (row + 1 < rows).then_some((row + 1, col)),
            Move::Next if at + 1 < cols.len() => Some((row, cols[at + 1])),
            Move::Next => (row + 1 < rows).then(|| (row + 1, cols[0])),
            Move::Prev if at > 0 => Some((row, cols[at - 1])),
            Move::Prev => row.checked_sub(1).map(|r| (r, cols[cols.len() - 1])),
        };
        let Some((r, c)) = next else { return };
        self.ui.table.update(cx, |t, cx| {
            t.set_selected_cell(r, c, cx);
            t.scroll_to_row(r, cx);
            t.scroll_to_col(c, cx);
        });
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
        let original = edit_text(&o.rows[ri][col]);
        let value = if null { None } else { original.clone() };
        self.set_change(ri, col, value, original, cx);
    }

    #[allow(clippy::too_many_arguments)]
    fn edit_in_dialog(&mut self, ri: usize, col: usize, column: &Column, n: usize, current: Option<String>, original: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let json = column.type_name.to_ascii_lowercase().contains("json");
        let this = cx.entity().downgrade();
        let apply: Apply = Rc::new(move |value, window, cx| {
            window.close_dialog(cx);
            let (this, original) = (this.clone(), original.clone());
            // Enter arrives through the dialog's subscription, mid-update: apply after it.
            window.defer(cx, move |_, cx| {
                this.update(cx, |this, cx| this.set_change(ri, col, value, original, cx)).ok();
            });
        });
        let form = cx.new(|cx| CellEdit::new(current, json, apply.clone(), window, cx));
        let title = format!("{} · {} · row {n}", column.name, column.type_name);
        let field = form.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            let (apply, read) = (apply.clone(), field.clone());
            dialog.title(title.clone()).w(px(640.)).child(field.clone()).footer(
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

    fn set_change(&mut self, row: usize, col: usize, value: Option<String>, original: Option<String>, cx: &mut Context<Self>) {
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
    use super::{Changes, edit_text, plan, update_sql};
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
        assert_eq!(t.cols, [Some((0, "id".to_string())), Some((0, "name".to_string())), None, None]);
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
        changes.insert((0, 2), Some("it's".into()));
        changes.insert((0, 3), None);
        changes.insert((1, 0), Some("30".into()));
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
        // A NULL key (outer join): no row to update.
        let rows = vec![vec![Value::Null, Value::Int(2), Value::Null, Value::Null]];
        let mut changes = Changes::new();
        changes.insert((0, 2), Some("x".into()));
        assert!(update_sql(Dialect::Postgres, &t, &rows, &changes).unwrap_err().contains("is NULL"));
    }

    #[test]
    fn keys_in_the_cell_field() {
        use super::{Move, move_key};
        // "abc": all selected (just opened), caret at start / middle / end.
        let k = |key, shift, sel: std::ops::Range<usize>| move_key(key, shift, !shift, sel, 3);
        assert_eq!(k("tab", false, 1..1), Some(Some(Move::Next)));
        assert_eq!(k("tab", true, 1..1), Some(Some(Move::Prev)));
        assert_eq!(k("enter", false, 1..1), Some(Some(Move::Stay)));
        assert_eq!(k("enter", true, 1..1), Some(Some(Move::Stay)));
        assert_eq!(k("up", false, 1..1), Some(Some(Move::Up)));
        assert_eq!(k("escape", false, 1..1), Some(None));
        assert_eq!(k("left", false, 0..3), Some(Some(Move::Prev)));
        assert_eq!(k("right", false, 0..3), Some(Some(Move::Next)));
        assert_eq!(k("left", false, 1..1), None); // the caret moves
        assert_eq!(k("left", false, 0..0), Some(Some(Move::Prev)));
        assert_eq!(k("right", false, 3..3), Some(Some(Move::Next)));
        assert_eq!(k("right", false, 2..2), None);
        assert_eq!(move_key("left", true, false, 0..0, 3), None); // Shift+← selects
        assert_eq!(k("a", false, 0..0), None);
    }

    #[test]
    fn cells_edit_as_text() {
        assert_eq!(edit_text(&Value::Null), None);
        assert_eq!(edit_text(&Value::Int(5)).as_deref(), Some("5"));
        assert_eq!(edit_text(&Value::Json(serde_json::json!({"a": 1}))).as_deref(), Some("{\"a\":1}"));
    }
}
