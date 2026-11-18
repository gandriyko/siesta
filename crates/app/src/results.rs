//! The result panel: per-statement results (`Store`), the table (gpui-component `DataTable`),
//! header, tabs (Result | Messages | Query), pagination, sorting and the filter row.
//!
//! The panel shows the result of the statement under the cursor (or the pinned one). Page, sort
//! and filter changes run the statement again in the same session while the previous page stays
//! on screen.

use std::{
    collections::HashMap,
    ops::Range,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    time::Instant,
};

use db_conn::Driver;
use db_exec::{DbError, Outcome, Page, Value};
use gpui::{prelude::FluentBuilder as _, *};
use gpui_component::{
    ActiveTheme as _, Disableable as _, Icon, IconName, Selectable as _, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{EditorState, Input, InputEvent, InputState, RopeExt as _},
    menu::{DropdownMenu as _, PopupMenuItem},
    tab::{Tab as TabItem, TabBar},
    table::{Column, DataTable, TableDelegate, TableEvent, TableState},
    v_flex,
};
use sql_split::{Dialect, Kind};

use crate::{Siesta, RUN_KEY, doc::dialect_of, format::*, runner::Spec};

/// Lines a hover tip shows; the rest is one line pointing at the value dialog.
const TIP_LINES: usize = 2000;
/// Characters of one line a hover tip keeps.
const TIP_LINE_CHARS: usize = 4000;

/// Token kinds of a pretty-printed JSON line, named after the theme's syntax styles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tok {
    Key,
    Str,
    Num,
    Bool,
    Null,
}

impl Tok {
    fn style(self) -> &'static str {
        match self {
            Tok::Key => "property",
            Tok::Str => "string",
            Tok::Num => "number",
            Tok::Bool => "boolean",
            Tok::Null => "constant",
        }
    }
}

/// Byte ranges of the tokens of one line of `serde_json::to_string_pretty` output (a cut-off
/// line just ends inside its token). Punctuation stays unstyled.
fn json_tokens(line: &str) -> Vec<(Range<usize>, Tok)> {
    let b = line.as_bytes();
    let (mut out, mut i) = (vec![], 0);
    while i < b.len() {
        let start = i;
        match b[i] {
            b'"' => {
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
                i = (i + 1).min(b.len());
                let key = line[i..].trim_start().starts_with(':');
                out.push((start..i, if key { Tok::Key } else { Tok::Str }));
            }
            b'-' | b'0'..=b'9' => {
                while i < b.len() && matches!(b[i], b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9') {
                    i += 1;
                }
                out.push((start..i, Tok::Num));
            }
            b't' | b'f' | b'n' => {
                while i < b.len() && b[i].is_ascii_alphabetic() {
                    i += 1;
                }
                out.push((start..i, if b[start] == b'n' { Tok::Null } else { Tok::Bool }));
            }
            _ => i += 1,
        }
    }
    out
}

/// The hover tip of a JSON cell: the document pretty-printed and highlighted, scrollable both ways.
struct JsonTip {
    lines: Vec<SharedString>,
}

impl JsonTip {
    fn new(j: &serde_json::Value) -> Self {
        let pretty = serde_json::to_string_pretty(j).unwrap_or_default();
        let total = pretty.lines().count();
        let mut lines: Vec<SharedString> = pretty
            .lines()
            .take(TIP_LINES)
            .map(|l| if l.len() > TIP_LINE_CHARS { format!("{}…", l.chars().take(TIP_LINE_CHARS).collect::<String>()).into() } else { l.to_string().into() })
            .collect();
        if total > TIP_LINES {
            lines.push(format!("… {} more lines: double-click the cell for the whole value", total - TIP_LINES).into());
        }
        Self { lines }
    }
}

impl Render for JsonTip {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // A hoverable tip survives clicks (GPUI only hides plain tooltips on mouse down): once a
        // double click opened the value dialog, draw nothing.
        if gpui_component::WindowExt::has_active_dialog(window, cx) {
            return div().id("json-tip");
        }
        let t = cx.theme();
        let syntax = t.highlight_theme.clone();
        let lines = self.lines.iter().map(|l| {
            let runs: Vec<(Range<usize>, HighlightStyle)> = json_tokens(l).into_iter().filter_map(|(r, k)| Some((r, syntax.style(k.style())?))).collect();
            div().whitespace_nowrap().child(StyledText::new(l.clone()).with_highlights(runs))
        });
        div()
            .id("json-tip")
            .max_w(px(640.))
            .max_h(px(420.))
            .overflow_scroll()
            .p_2()
            .rounded_md()
            .border_1()
            .border_color(t.border)
            .bg(t.popover)
            .text_color(t.popover_foreground)
            .shadow_md()
            .font_family(t.mono_font_family.clone())
            // Smaller than the grid, so more of the document fits.
            .text_size(t.mono_font_size * 0.8)
            .children(lines)
    }
}

/// A statement of a file: where its result lives. Edit the text and it is a new statement.
pub type Key = (PathBuf, String);

#[derive(Clone, Copy, PartialEq, Eq, Default, Debug)]
pub enum Tab {
    #[default]
    Result,
    Messages,
    Query,
}

/// A finished run of one statement.
pub struct Done {
    pub text: String,
    pub kind: Kind,
    /// The page that was asked for (offset, size, sort, filters).
    pub page: Page,
    /// Where the statement was in the editor when it ran.
    pub range: Option<std::ops::Range<usize>>,
    pub conn: String,
    /// The database it ran on (`None` = the server's default).
    pub database: Option<String>,
    pub color: Option<String>,
    pub driver: Driver,
    pub result: Result<Outcome, DbError>,
}

impl Done {
    /// `connection · database` (just the connection when no database is known).
    pub fn label(&self) -> String {
        match &self.database {
            Some(db) => format!("{} · {db}", self.conn),
            None => self.conn.clone(),
        }
    }

    pub fn outcome(&self) -> Option<&Outcome> {
        self.result.as_ref().ok()
    }

    pub fn dialect(&self) -> Dialect {
        dialect_of(self.driver)
    }

    /// The server pages, sorts and filters this result. Not so for statements that cannot be
    /// wrapped, nor for queries that ran unwrapped (repeated column names): those sort locally.
    pub fn paged(&self) -> bool {
        self.kind == Kind::Query && self.outcome().is_none_or(|o| o.wrapped)
    }
}

#[derive(Default)]
pub struct Entry {
    /// Bumped by every new result or local re-sort: the table reloads when it changes.
    pub id: u64,
    pub running: Option<Instant>,
    pub done: Option<Arc<Done>>,
    /// The page the next run asks for; `done.page` is the one on screen.
    pub page: Page,
    /// "Count all": the filters it was counted with, and the count.
    pub count: Option<(Vec<(String, String)>, u64)>,
    pub tab: Tab,
}

#[derive(Default)]
pub struct Store {
    pub entries: HashMap<Key, Entry>,
    /// The statements of the latest multi-statement run per file (empty: a single statement ran).
    pub batches: HashMap<PathBuf, Vec<Key>>,
    pub last: Option<(PathBuf, Vec<Spec>)>,
}

impl Store {
    /// Put a finished result in place (used by the run pipeline, and by tests).
    #[cfg(test)]
    pub fn put(&mut self, key: Key, done: Done) {
        let e = self.entries.entry(key).or_default();
        e.id += 1;
        e.page = done.page.clone();
        e.done = Some(Arc::new(done));
    }
}

/// The table's data and look.
pub struct Grid {
    view: WeakEntity<Siesta>,
    pub done: Option<Arc<Done>>,
    /// Display order of the rows (identity, or the local sort).
    pub order: Vec<usize>,
    numeric: Vec<bool>,
    widths: Vec<Pixels>,
    sort: Vec<(String, bool)>,
    null_text: String,
    preview_bytes: usize,
    pub filters: Vec<Entity<InputState>>,
    show_filters: bool,
    /// The result has a JSON column (or text cells that are JSON objects / arrays): the toggle shows.
    pub has_json: bool,
    /// JSON preview on: lines per cell (0 = off) and the row height that fits them.
    json_lines: usize,
    line_h: Pixels,
    row_h: Pixels,
    /// First lines of the pretty-printed JSON per (row in `done`, column); only with the preview on.
    previews: HashMap<(usize, usize), Vec<String>>,
    /// Edit mode: which columns can be edited (empty: not editing), and the pending values per
    /// (row in `done`, column).
    pub editable: Vec<bool>,
    pub changes: crate::edit::Changes,
    /// The cell edited in place: (display row, column, its field).
    pub editing: Option<(usize, usize, Entity<InputState>)>,
    /// What a cell draws, per (row in `done`, column), made on its first render: the table renders
    /// all visible cells on every frame (a caret blink in the edited cell is one), so formatting
    /// and JSON parsing happen once per result, not per frame.
    cells: HashMap<(usize, usize), Rc<Cell>>,
}

/// A cell ready to draw.
struct Cell {
    text: SharedString,
    /// The document of a JSON cell (its hover tip).
    json: Option<Arc<serde_json::Value>>,
    null: bool,
    bytes: bool,
}

impl Grid {
    fn new(view: WeakEntity<Siesta>) -> Self {
        Grid {
            view,
            done: None,
            order: vec![],
            numeric: vec![],
            widths: vec![],
            sort: vec![],
            null_text: "NULL".into(),
            preview_bytes: 8 * 1024,
            filters: vec![],
            show_filters: false,
            has_json: false,
            json_lines: 0,
            line_h: px(18.),
            row_h: px(30.),
            previews: HashMap::new(),
            editable: vec![],
            changes: Default::default(),
            editing: None,
            cells: HashMap::new(),
        }
    }

    /// The drawable cell at (row in `done`, column), made once.
    fn cell(&mut self, ri: usize, col: usize) -> Option<Rc<Cell>> {
        if let Some(c) = self.cells.get(&(ri, col)) {
            return Some(c.clone());
        }
        let v = self.outcome()?.rows.get(ri)?.get(col)?;
        let c = Rc::new(Cell {
            text: cell_display(v, &self.null_text, self.preview_bytes).into(),
            json: cell_json(v).map(|j| Arc::new(j.into_owned())),
            null: matches!(v, Value::Null),
            bytes: matches!(v, Value::Bytes(_)),
        });
        self.cells.insert((ri, col), c.clone());
        Some(c)
    }

    /// Whether the rows are tall (the JSON preview is on and there is JSON to show).
    pub fn tall(&self) -> bool {
        self.json_lines > 0 && self.has_json
    }

    pub fn outcome(&self) -> Option<&Outcome> {
        self.done.as_ref().and_then(|d| d.outcome())
    }

    /// The value at display position (`row`, `col`).
    pub fn value(&self, row: usize, col: usize) -> Option<&Value> {
        self.outcome()?.rows.get(*self.order.get(row)?)?.get(col)
    }

    /// `json_lines`: lines of JSON per cell, 0 = no preview; `mono`: the monospace font size they use.
    #[allow(clippy::too_many_arguments)]
    fn load(&mut self, done: Arc<Done>, sort: Vec<(String, bool)>, null_text: &str, preview_bytes: usize, fs: Pixels, json_lines: usize, mono: Pixels) {
        let mut order = vec![];
        let (mut numeric, mut widths) = (vec![], vec![]);
        let mut previews = HashMap::new();
        self.has_json = false;
        if let Some(o) = done.outcome() {
            // ponytail: JSON is looked for in the first 200 rows; a column that only has JSON later gets no preview.
            self.has_json = o.rows.iter().take(200).any(|r| r.iter().any(|v| cell_json(v).is_some()));
            if json_lines > 0 && self.has_json {
                for (ri, r) in o.rows.iter().enumerate() {
                    for (ci, v) in r.iter().enumerate() {
                        if let Some(j) = cell_json(v) {
                            previews.insert((ri, ci), json_preview(&j, json_lines));
                        }
                    }
                }
            }
            // The server sorted wrappable statements; the others are sorted here ("local").
            order = if done.paged() { (0..o.rows.len()).collect() } else { sort_order(&o.rows, &o.columns, &sort) };
            for (i, c) in o.columns.iter().enumerate() {
                let sample = o.rows.iter().take(50).map(|r| cell_display(&r[i], null_text, 64).chars().count());
                let chars = sample.chain([c.name.chars().count() + 3, c.type_name.chars().count()]).max().unwrap_or(4);
                widths.push((fs * 0.62 * chars as f32 + px(28.)).clamp(px(64.), px(360.)));
                numeric.push(is_numeric_column(&o.rows, i));
            }
        }
        (self.done, self.order, self.numeric, self.widths, self.sort) = (Some(done), order, numeric, widths, sort);
        (self.null_text, self.preview_bytes) = (null_text.to_string(), preview_bytes);
        self.json_lines = if previews.is_empty() { 0 } else { json_lines };
        self.line_h = mono * 1.4;
        self.row_h = self.line_h * json_lines as f32 + px(10.);
        self.previews = previews;
        self.cells.clear();
    }
}

impl TableDelegate for Grid {
    fn columns_count(&self, _: &App) -> usize {
        self.outcome().map_or(0, |o| o.columns.len())
    }

    fn rows_count(&self, _: &App) -> usize {
        self.order.len()
    }

    fn column(&self, ix: usize, _: &App) -> Column {
        let name = self.outcome().map_or_else(String::new, |o| o.columns[ix].name.clone());
        let c = Column::new(name.clone(), name).width(self.widths.get(ix).copied().unwrap_or(px(120.))).resizable(true).movable(false).selectable(true);
        if self.numeric.get(ix) == Some(&true) { c.text_right() } else { c }
    }

    /// Name and type, the sort mark, and (with Cmd+Alt+F) the filter field. A click sorts.
    fn render_th(&mut self, ix: usize, _: &mut Window, cx: &mut Context<TableState<Self>>) -> impl IntoElement {
        let Some(col) = self.outcome().map(|o| o.columns[ix].clone()) else { return div().into_any_element() };
        let t = cx.theme();
        let mark =self.sort.iter().position(|(n, _)| *n == col.name).map(|i| {
            let arrow = if self.sort[i].1 { "↓" } else { "↑" };
            if self.sort.len() > 1 { format!("{arrow}{}", i + 1) } else { arrow.to_string() }
        });
        let view = self.view.clone();
        let name = col.name.clone();
        let local = self.done.as_ref().is_some_and(|d| !d.paged());
        // Edit mode: read-only columns are muted.
        let read_only = !self.editable.is_empty() && self.editable.get(ix) != Some(&true);
        let head = div()
            .id(("th", ix))
            .w_full()
            .cursor_pointer()
            .when(read_only, |d| d.text_color(t.muted_foreground))
            .on_click(move |ev: &ClickEvent, window, cx| {
                let shift = ev.modifiers().shift;
                view.update(cx, |this, cx| this.sort_by(name.clone(), shift, window, cx)).ok();
            })
            .child(
                h_flex()
                    .gap_1()
                    .items_baseline()
                    .child(div().truncate().font_weight(FontWeight::SEMIBOLD).child(col.name.clone()))
                    .children(mark.map(|m| div().flex_none().text_color(t.primary).child(if local { format!("{m} local") } else { m }))),
            )
            // Only names in the header; the type is a hover away.
            .tooltip(move |window, cx| gpui_component::tooltip::Tooltip::new(col.type_name.clone()).build(window, cx));
        let filter = self.show_filters.then(|| self.filters.get(ix).cloned()).flatten().map(|input| {
            // A click in the field must not sort.
            div().id(("th-filter", ix)).w_full().pt_1().on_click(|_, _, cx| cx.stop_propagation()).child(Input::new(&input).small().cleanable(true))
        });
        v_flex().w_full().child(head).children(filter).into_any_element()
    }

    /// With the JSON preview on, every row is as tall as N lines: gpui-component's table takes the
    /// row height from the row element itself (`render_tr`) and its list measures the first row.
    fn render_tr(&mut self, row_ix: usize, _: &mut Window, _: &mut Context<TableState<Self>>) -> Stateful<Div> {
        let tr = div().id(("row", row_ix));
        if self.tall() { tr.h(self.row_h) } else { tr }
    }

    fn render_td(&mut self, row: usize, col: usize, _: &mut Window, cx: &mut Context<TableState<Self>>) -> impl IntoElement {
        // Edited in place: the field, which keeps its clicks (they must not reselect the table)
        // and turns Esc into cancel.
        if let Some((_, _, input)) = self.editing.as_ref().filter(|(r, c, _)| (*r, *c) == (row, col)) {
            let view = self.view.clone();
            return div()
                .id(("cell-edit", col))
                .w_full()
                // A light tint marks the cell being edited (pending values are tinted with `warning`).
                .px_1()
                .rounded_sm()
                .bg(cx.theme().primary.opacity(0.14))
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                // Only Enter (keep) and Esc (cancel) leave the field; no other key reaches another
                // cell. Enter arrives as the table's `OpenCell` (see `open_selected`).
                .map(|d| edit_keys(d, view))
                // Bare text: the selected cell's own frame is the only border.
                .child(Input::new(input).appearance(false).xsmall().p_0())
                .into_any_element();
        }
        let Some(&ri) = self.order.get(row) else { return div().into_any_element() };
        let Some(cell) = self.cell(ri, col) else { return div().into_any_element() };
        let t = cx.theme();
        // A pending edit: the new value, highlighted, until it is saved or discarded.
        if let Some(new) = self.order.get(row).and_then(|r| self.changes.get(&(*r, col))) {
            return div()
                .w_full()
                .truncate()
                .px_1()
                .rounded_sm()
                .bg(t.warning.opacity(0.22))
                .when(new.is_none(), |d| d.italic())
                .child(new.as_ref().map_or_else(|| self.null_text.clone(), |v| cell_display(&v.as_value(), &self.null_text, self.preview_bytes)))
                .into_any_element();
        }
        // JSON cells: hovering shows the whole document, formatted; the tip stays while the
        // pointer is on it, so it can be scrolled.
        let json_tip = |el: Div| -> AnyElement {
            let Some(j) = cell.json.clone() else { return el.into_any_element() };
            el.id(("json-cell", col)).hoverable_tooltip(move |_, cx| cx.new(|_| JsonTip::new(&j)).into()).into_any_element()
        };
        if let Some(lines) = self.previews.get(&(ri, col)) {
            let (mono, size, line_h) = (t.mono_font_family.clone(), t.mono_font_size, self.line_h);
            return json_tip(div().w_full().font_family(mono).text_size(size).children(lines.iter().map(|l| div().w_full().h(line_h).truncate().child(l.clone()))));
        }
        json_tip(
            div()
                .w_full()
                .truncate()
                .when(cell.null, |d| d.italic().text_color(t.muted_foreground))
                .when(cell.bytes, |d| d.text_color(t.muted_foreground))
                .when(self.numeric.get(col) == Some(&true), |d| d.text_right())
                .child(cell.text.clone()),
        )
    }

    fn context_menu(&mut self, row: usize, menu: gpui_component::menu::PopupMenu, _: &mut Window, _: &mut Context<TableState<Self>>) -> gpui_component::menu::PopupMenu {
        let item = |label: &'static str, what: fn(&mut Siesta, usize, &mut Window, &mut Context<Siesta>)| {
            let view = self.view.clone();
            PopupMenuItem::new(label).on_click(move |_, window, cx| {
                view.update(cx, |this, cx| what(this, row, window, cx)).ok();
            })
        };
        let menu = menu.item(item("Copy row", Siesta::copy_row_tsv)).item(item("Copy as JSON", Siesta::copy_row_json)).item(item("Copy as INSERT", Siesta::copy_row_insert));
        if self.editable.is_empty() {
            return menu;
        }
        menu.separator().item(item("Set cell to NULL", |this, row, window, cx| this.set_selected(true, row, window, cx))).item(item("Revert cell", |this, row, window, cx| this.set_selected(false, row, window, cx)))
    }

    fn cell_text(&self, row: usize, col: usize, _: &App) -> String {
        self.value(row, col).map(tsv_cell).unwrap_or_default()
    }
}

/// The keys of the in-place field. A key may arrive as a binding of the field (Input context), of
/// the table (`SelectUp`, `SelectNextColumn`… run when the field lets an arrow through), or, when
/// nothing binds it, as a plain key (Tab would type a tab). The wrapper sits between the table and
/// the field, so its capture listeners see all of them first: the table's navigation is swallowed,
/// ↑ / ↓ / Tab do nothing, Esc cancels. ← / → stay the field's (the caret).
fn edit_keys(d: Stateful<Div>, view: WeakEntity<Siesta>) -> Stateful<Div> {
    use gpui_base::actions as a;
    use gpui_component::input as i;
    fn swallow<A: Action>(d: Stateful<Div>) -> Stateful<Div> {
        d.capture_action(|_: &A, _, cx| cx.stop_propagation())
    }
    let commit = {
        let view = view.clone();
        move |_: &i::Enter, window: &mut Window, cx: &mut App| {
            cx.stop_propagation();
            view.update(cx, |this, cx| this.commit_inline(window, cx)).ok();
        }
    };
    let d = d.capture_action(commit);
    let cancel = move |window: &mut Window, cx: &mut App| {
        cx.stop_propagation();
        view.update(cx, |this, cx| this.cancel_inline(window, cx)).ok();
    };
    let d = [swallow::<a::SelectUp>, swallow::<a::SelectDown>, swallow::<a::SelectPrevColumn>, swallow::<a::SelectNextColumn>, swallow::<a::SelectFirst>, swallow::<a::SelectLast>, swallow::<a::SelectPageUp>, swallow::<a::SelectPageDown>]
        .into_iter()
        .chain([swallow::<i::MoveUp>, swallow::<i::MoveDown>, swallow::<i::IndentInline>, swallow::<i::OutdentInline>])
        .fold(d, |d, f| f(d));
    let (c1, c2) = (cancel.clone(), cancel.clone());
    d.capture_action(move |_: &i::Escape, window, cx| c1(window, cx))
        .capture_action(move |_: &a::Cancel, window, cx| c2(window, cx))
        .capture_key_down(move |ev: &KeyDownEvent, window, cx| match ev.keystroke.key.as_str() {
            "tab" | "up" | "down" => cx.stop_propagation(),
            "escape" => cancel(window, cx),
            _ => {}
        })
}

/// Everything the panel keeps besides the results themselves.
pub struct Ui {
    pub table: Entity<TableState<Grid>>,
    /// Messages and Query tabs: a read-only editor, loaded when what it shows changes.
    meta: Entity<EditorState>,
    meta_shown: Option<(Key, u64, Tab)>,
    filter_cols: Vec<String>,
    filter_subs: Vec<Subscription>,
    pub filters_open: bool,
    /// (statement, result id, filter row visible, JSON preview lines): what the table currently holds.
    shown: Option<(Key, u64, bool, usize)>,
    /// The header's JSON preview toggle for this session; `None` follows `results.json_preview_lines`.
    pub json_on: Option<bool>,
    pub pinned: Option<Key>,
    pub popup: Option<Entity<crate::value_popup::Popup>>,
    /// Edit mode of one result (the header's Edit button).
    pub edit: Option<crate::edit::Edit>,
    _subs: Vec<Subscription>,
}

impl Ui {
    pub fn new(view: WeakEntity<Siesta>, window: &mut Window, cx: &mut Context<Siesta>) -> Self {
        let table = cx.new(|cx| {
            TableState::new(Grid::new(view), window, cx).cell_selectable(true).row_header(false).col_selectable(false).row_selectable(false).sortable(false).col_movable(false)
        });
        let meta = cx.new(|cx| {
            let mut s = EditorState::new(window, cx).line_number(false).soft_wrap(true).folding(false).searchable(true).replaceable(false);
            s.set_readonly(true, cx);
            s
        });
        let subs = vec![cx.subscribe_in(&table, window, |this, _, ev: &TableEvent, window, cx| {
            // A single click only selects (it must be possible to select and copy);
            // a double click, Enter or Space opens the value.
            match ev {
                TableEvent::DoubleClickedCell(r, c) => this.open_cell(*r, *c, window, cx),
                // The selection left the cell being edited (any way at all): editing ends there.
                TableEvent::SelectCell(r, c) if this.ui.edit.as_ref().and_then(|e| e.inline.as_ref()).is_some_and(|i| (i.row, i.col) != (*r, *c)) => this.commit_inline(window, cx),
                _ => {}
            }
        })];
        Ui { table, meta, meta_shown: None, filter_cols: vec![], filter_subs: vec![], filters_open: false, shown: None, json_on: None, pinned: None, popup: None, edit: None, _subs: subs }
    }
}

/// What the tree shows beside a statement: `…` while it runs, its row count, or ✕.
fn entry_status(e: &Entry) -> Option<(String, bool)> {
    if e.running.is_some() && e.done.is_none() {
        return Some(("…".into(), false));
    }
    match &e.done.as_ref()?.result {
        Ok(o) => Some((o.rows_affected.filter(|_| o.columns.is_empty()).map_or_else(|| o.rows.len().to_string(), |n| n.to_string()), false)),
        Err(_) => Some(("✕".into(), true)),
    }
}

fn plain_error(e: &DbError) -> String {
    let mut s = match &e.code {
        Some(c) => format!("ERROR {c}: {}", e.message),
        None => format!("ERROR: {}", e.message),
    };
    if let Some(d) = &e.detail {
        s += &format!("\nDETAIL: {d}");
    }
    if let Some(h) = &e.hint {
        s += &format!("\nHINT: {h}");
    }
    if let Some(p) = e.position {
        s += &format!("\nPOSITION: {p}");
    }
    s
}

/// The Messages tab.
pub fn messages_text(done: &Done) -> String {
    let mut parts = vec![];
    match &done.result {
        Err(e) => parts.push(plain_error(e)),
        Ok(o) => parts.extend(o.notices.iter().cloned()),
    }
    if parts.is_empty() { "No messages".into() } else { parts.join("\n\n") }
}

/// The Query tab: what was sent (with the page wrapper), and its parameters.
pub fn query_text(done: &Done) -> String {
    match &done.result {
        Ok(o) => {
            let params: String = o.params.iter().enumerate().map(|(i, p)| format!("\n-- ${} = {p}", i + 1)).collect();
            format!("{}{params}", o.executed_sql)
        }
        Err(_) => done.text.clone(),
    }
}

impl Siesta {
    /// The statement whose result is shown: the pinned one, else the one under the cursor.
    pub(crate) fn shown_key(&self, cx: &App) -> Option<Key> {
        if let Some(k) = &self.ui.pinned {
            return Some(k.clone());
        }
        self.active_key(cx)
    }

    /// The statement under the cursor of the current file.
    pub(crate) fn active_key(&self, cx: &App) -> Option<Key> {
        let d = self.doc().filter(|d| d.sql)?;
        let st = d.active_statement()?;
        let text = d.editor.read(cx).text().slice(st.range.clone()).to_string();
        Some(((*d.path).clone(), text))
    }

    /// Row count or ✕ beside a statement of the tree.
    #[cfg(test)]
    pub(crate) fn statement_status(&self, path: &Path, line: usize, cx: &App) -> Option<(String, bool)> {
        self.statement_statuses(path, cx).remove(&line)
    }

    /// Row count or ✕ per statement line (1-based) of an open file, for the tree. One pass over
    /// the file's results, not a text copy per statement: the tree is drawn every frame.
    pub(crate) fn statement_statuses(&self, path: &Path, cx: &App) -> HashMap<usize, (String, bool)> {
        let mut out = HashMap::new();
        let Some(d) = self.docs.iter().find(|d| *d.path == path) else { return out };
        let rope = d.editor.read(cx).text();
        for ((p, text), e) in &self.store.entries {
            let Some(status) = (p == path).then(|| entry_status(e)).flatten() else { continue };
            // Only a statement of the same length can have this text: compare those alone.
            for st in d.file.statements.iter().filter(|s| s.range.len() == text.len()) {
                if rope.slice(st.range.clone()) == text.as_str() {
                    out.insert(st.line, status.clone());
                }
            }
        }
        out
    }

    fn entry(&self, key: &Key) -> Option<&Entry> {
        self.store.entries.get(key)
    }

    fn done_of(&self, key: &Key) -> Option<Arc<Done>> {
        self.entry(key)?.done.clone()
    }

    /// Lines of JSON per cell while the preview is on, else 0: the header toggle (from the setting
    /// at first); a setting of 0 gives 4 when the toggle is switched on.
    pub(crate) fn json_lines(&self) -> usize {
        let n = self.settings.settings.results.json_preview_lines;
        if self.ui.json_on.unwrap_or(n > 0) { if n == 0 { 4 } else { n } } else { 0 }
    }

    pub(crate) fn toggle_json_preview(&mut self, cx: &mut Context<Self>) {
        self.ui.json_on = Some(self.json_lines() == 0);
        cx.notify();
    }

    /// Load the table (and the filter fields, and the Messages / Query text) when what is shown changed.
    pub(crate) fn sync_results(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let key = self.shown_key(cx);
        let json_lines = self.json_lines();
        let e = key.as_ref().and_then(|k| self.entry(k));
        let sig = key.clone().zip(e).map(|(k, e)| (k, e.id, self.ui.filters_open, json_lines));
        let (done, page) = e.map(|e| (e.done.clone(), e.page.clone())).unwrap_or_default();
        if sig != self.ui.shown {
            self.ui.shown = sig;
            let st = &self.settings.settings.results;
            let (null_text, preview, fs) = (st.null_text.clone(), st.cell_preview_kb * 1024, cx.theme().font_size);
            let mono = cx.theme().mono_font_size;
            let cols: Vec<String> = done.as_ref().and_then(|d| d.outcome()).map(|o| o.columns.iter().map(|c| c.name.clone()).collect()).unwrap_or_default();
            if cols != self.ui.filter_cols {
                self.rebuild_filter_inputs(&cols, window, cx);
            }
            // The fields show this statement's filters (the page that was asked for).
            for (name, input) in cols.iter().zip(&self.grid_filters(cx)) {
                let text = page.filters.iter().find(|(c, _)| c == name).map(|(_, t)| t.clone()).unwrap_or_default();
                input.update(cx, |s, cx| {
                    if s.value() != text.as_str() {
                        s.set_value(text, window, cx);
                    }
                });
            }
            let open = self.ui.filters_open;
            let local_sort = page.sort.clone();
            self.ui.table.update(cx, |t, cx| {
                let g = t.delegate_mut();
                g.show_filters = open;
                match done {
                    Some(d) => g.load(d, local_sort, &null_text, preview, fs, json_lines, mono),
                    None => *g = Grid { filters: std::mem::take(&mut g.filters), ..Grid::new(g.view.clone()) },
                }
                t.clear_selection(cx);
                t.refresh(cx);
            });
        }
        self.sync_meta(key, window, cx);
        self.sync_edit(cx);
    }

    fn grid_filters(&self, cx: &App) -> Vec<Entity<InputState>> {
        self.ui.table.read(cx).delegate().filters.clone()
    }

    fn rebuild_filter_inputs(&mut self, cols: &[String], window: &mut Window, cx: &mut Context<Self>) {
        self.ui.filter_cols = cols.to_vec();
        self.ui.filter_subs.clear();
        let inputs: Vec<Entity<InputState>> = cols.iter().map(|c| cx.new(|cx| InputState::new(window, cx).placeholder(format!("filter {c}")))).collect();
        for i in &inputs {
            // Enter applies all the fields at once (a run per keystroke would be a request per letter).
            let sub = cx.subscribe_in(i, window, |this, _, ev: &InputEvent, window, cx| {
                if matches!(ev, InputEvent::PressEnter { .. }) {
                    this.apply_filters(window, cx);
                }
            });
            self.ui.filter_subs.push(sub);
        }
        self.ui.table.update(cx, |t, _| t.delegate_mut().filters = inputs);
    }

    fn sync_meta(&mut self, key: Option<Key>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(k) = key else { return };
        let Some(e) = self.entry(&k) else { return };
        let Some(done) = e.done.clone().filter(|_| e.tab != Tab::Result) else { return };
        let sig = Some((k, e.id, e.tab));
        if sig == self.ui.meta_shown {
            return;
        }
        let (tab, text) = (e.tab, if e.tab == Tab::Messages { messages_text(&done) } else { query_text(&done) });
        self.ui.meta_shown = sig;
        self.ui.meta.update(cx, |s, cx| {
            s.set_highlighter(SharedString::from(if tab == Tab::Query { "sql" } else { "text" }), cx);
            s.set_value(text, window, cx);
        });
    }

    // ---- page, sort, filter ----

    fn edit_page(&mut self, window: &mut Window, cx: &mut Context<Self>, change: impl FnOnce(&mut Entry)) {
        let Some(key) = self.shown_key(cx) else { return };
        let Some(e) = self.store.entries.get_mut(&key) else { return };
        if e.done.is_none() || e.running.is_some() {
            return;
        }
        // Pending edits belong to the rows on screen: another page would drop them.
        let n = self.ui.edit.as_ref().filter(|ed| ed.key == key).map_or(0, |ed| ed.changes.len());
        if n > 0 {
            return window.push_notification(format!("Save or discard the {n} pending change{} first", if n == 1 { "" } else { "s" }), cx);
        }
        let Some(e) = self.store.entries.get_mut(&key) else { return };
        change(e);
        let local = e.done.as_ref().is_some_and(|d| !d.paged());
        if local {
            // Not wrappable: the sort happens in memory.
            e.id += 1;
            cx.notify();
        } else {
            self.rerun(&key, window, cx);
        }
    }

    /// Header click (Shift adds the column to the sort).
    pub(crate) fn sort_by(&mut self, col: String, shift: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.edit_page(window, cx, |e| {
            e.page.sort = next_sort(&e.page.sort, &col, shift);
            e.page.offset = 0;
        });
    }

    /// Cmd+] / Cmd+[
    pub(crate) fn step_page(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(key) = self.shown_key(cx) else { return };
        let Some(e) = self.entry(&key) else { return };
        let Some(done) = e.done.as_ref().filter(|d| d.paged()) else { return };
        // The page on screen, not one that is still being fetched.
        let (offset, size, more) = (done.page.offset, done.page.size, done.outcome().is_some_and(|o| o.has_more));
        if (forward && !more) || (!forward && offset == 0) {
            return;
        }
        self.edit_page(window, cx, |e| e.page.offset = if forward { offset + size } else { offset.saturating_sub(size) });
    }

    pub(crate) fn set_page_size(&mut self, size: u64, window: &mut Window, cx: &mut Context<Self>) {
        self.edit_page(window, cx, |e| {
            e.page.size = size.max(1);
            e.page.offset = 0;
        });
    }

    /// Enter in a filter field: all the fields become the page's filters.
    pub(crate) fn apply_filters(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let cols = self.ui.filter_cols.clone();
        let texts: Vec<String> = self.grid_filters(cx).iter().map(|i| i.read(cx).value().trim().to_string()).collect();
        let filters: Vec<(String, String)> = cols.into_iter().zip(texts).filter(|(_, t)| !t.is_empty()).collect();
        self.edit_page(window, cx, |e| {
            e.page.filters = filters;
            e.page.offset = 0;
        });
    }

    pub(crate) fn clear_filters(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.edit_page(window, cx, |e| {
            e.page.filters.clear();
            e.page.offset = 0;
        });
    }

    pub(crate) fn toggle_filters(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.ui.filters_open = !self.ui.filters_open;
        cx.notify();
    }

    fn pin(&mut self, cx: &mut Context<Self>) {
        self.ui.pinned = match self.ui.pinned {
            Some(_) => None,
            None => self.active_key(cx),
        };
        cx.notify();
    }

    // ---- selection and copying ----

    fn selected(&self, cx: &App) -> Option<(usize, usize)> {
        self.ui.table.read(cx).selected_cell()
    }

    /// Cmd+C: the selected cell (or row) for pasting into a spreadsheet.
    pub(crate) fn copy_selection(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((r, c)) = self.selected(cx) else { return };
        let text = self.ui.table.read(cx).delegate().value(r, c).map(tsv_cell);
        if let Some(t) = text {
            cx.write_to_clipboard(ClipboardItem::new_string(t));
            window.push_notification("Copied the cell", cx);
        }
    }

    fn with_row<R>(&self, row: usize, cx: &App, f: impl FnOnce(&Done, &Outcome, &[Value]) -> R) -> Option<R> {
        let g = self.ui.table.read(cx).delegate();
        let (done, o) = (g.done.as_ref()?, g.outcome()?);
        Some(f(done, o, o.rows.get(*g.order.get(row)?)?))
    }

    fn copy_text(&self, text: Option<String>, what: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(t) = text {
            cx.write_to_clipboard(ClipboardItem::new_string(t));
            window.push_notification(format!("Copied {what}"), cx);
        }
    }

    pub(crate) fn copy_row_tsv(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.with_row(row, cx, |_, _, r| tsv([r]));
        self.copy_text(text, "the row", window, cx);
    }

    pub(crate) fn copy_row_json(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.with_row(row, cx, |_, o, r| row_json(&o.columns, r));
        self.copy_text(text, "the row as JSON", window, cx);
    }

    pub(crate) fn copy_row_insert(&mut self, row: usize, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.with_row(row, cx, |d, o, r| insert_sql("table_name", &o.columns, [r], d.dialect()));
        self.copy_text(text, "the row as INSERT", window, cx);
    }

    /// Copy the whole shown result (the page) as TSV with a header line.
    fn copy_result(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let g = self.ui.table.read(cx).delegate();
        let Some(o) = g.outcome() else { return };
        let head = o.columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>().join("\t");
        let text = format!("{head}\n{}", tsv(g.order.iter().map(|&i| o.rows[i].as_slice())));
        let n = o.rows.len();
        self.copy_text(Some(text), &format!("{n} rows"), window, cx);
    }

    /// Enter / Space: the selected cell in the popup.
    pub(crate) fn open_selected(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // Enter while a cell is edited in place: keep the value and close the field (Enter toggles).
        if self.ui.edit.as_ref().is_some_and(|e| e.inline.is_some()) {
            return self.commit_inline(window, cx);
        }
        if let Some((r, c)) = self.selected(cx) {
            self.open_cell(r, c, window, cx);
        }
    }

    /// Double-click / Enter / Space: in edit mode an editable cell opens for editing, any other
    /// cell in the value viewer.
    fn open_cell(&mut self, row: usize, col: usize, window: &mut Window, cx: &mut Context<Self>) {
        if !self.edit_cell(row, col, window, cx) {
            self.open_popup(row, Some(col), window, cx);
        }
    }

    /// Cmd+Shift+Enter in the table: the whole row as a JSON object.
    pub(crate) fn open_selected_row(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((r, _)) = self.selected(cx) {
            self.open_popup(r, None, window, cx);
        }
    }

    // ---- the panel ----

    /// The right panel: header (Run / Stop, statement, Pin; status and actions), batch strip,
    /// tabs, then the tab's content and the page footer.
    pub(crate) fn result_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        self.sync_results(window, cx);
        let t = cx.theme();
        let (muted, border, fs) = (t.muted_foreground, t.border, t.font_size);
        let (danger, warning, primary, fg) = (t.danger, t.warning, t.primary, t.foreground);
        let key = self.shown_key(cx);
        let e = key.as_ref().and_then(|k| self.entry(k));
        let done = e.and_then(|e| e.done.clone());
        let running = e.and_then(|e| e.running);
        let tab = e.map_or(Tab::Result, |e| e.tab);
        let title = key.as_ref().map(|k| k.1.lines().next().unwrap_or("").trim().to_string());
        let icon = |svg: &'static [u8]| Icon::default().data(svg);
        let can_run = self.doc().is_some_and(|d| d.sql && d.active_statement().is_some());

        let run = Button::new("run")
            .label(if running.is_some() { "◼ Stop" } else { "▶ Run" })
            .primary()
            .small()
            .disabled(running.is_none() && !can_run)
            .tooltip(if running.is_some() { "Stop (Esc)" } else { "Run the statement (Cmd+Enter)" })
            .on_click(cx.listener(move |this, _, window, cx| if running.is_some() { this.cancel_run(window, cx) } else { this.run_current(window, cx) }));
        let pin = Button::new("pin")
            .icon(icon(include_bytes!("../assets/pin.svg")))
            .ghost()
            .small()
            .selected(self.ui.pinned.is_some())
            .tooltip(if self.ui.pinned.is_some() { "Pinned: click to follow the cursor again" } else { "Pin: keep this result while the cursor moves" })
            .on_click(cx.listener(|this, _, _, cx| this.pin(cx)));
        let line1 = h_flex()
            .gap_3()
            .items_center()
            .child(run)
            .child(div().min_w_0().flex_1().truncate().text_sm().text_color(muted).children(title))
            .child(pin);

        // Status: what ran, how long, where.
        let pill = |text: String, color: Hsla| div().flex_none().px_1p5().rounded_sm().bg(color.opacity(0.15)).text_color(color).text_xs().font_weight(FontWeight::BOLD).child(text);
        let mut status = h_flex().min_w_0().gap_2().items_center().text_xs().text_color(muted);
        if let Some(started) = running {
            status = status.child(div().text_color(warning).child(format!("Running… {:.1} s", started.elapsed().as_secs_f32())));
        }
        if let Some(d) = &done {
            status = match &d.result {
                Ok(o) => status.child(div().text_color(fg).font_weight(FontWeight::SEMIBOLD).child(rows_label(o))).child(duration_label(o.duration)),
                Err(_) => status.child(pill("Error".into(), danger)),
            };
            let dark = cx.theme().is_dark();
            let strip = d.color.as_deref().and_then(|c| parse_color(c, dark)).unwrap_or(muted.opacity(0.5));
            status = status.child(h_flex().gap_1().items_center().child(div().w(px(3.)).h(fs).rounded_full().bg(strip)).child(d.label()));
            if d.outcome().is_some_and(|o| o.in_transaction) || self.in_tx(&key.as_ref().expect("key").0) {
                status = status.child(div().text_color(warning).font_weight(FontWeight::BOLD).child("TX ●"));
            }
            let filters = d.page.filters.len();
            if filters > 0 {
                status = status.child(
                    Button::new("clear-filters")
                        .label(format!("{filters} filter{} ✕", if filters == 1 { "" } else { "s" }))
                        .ghost()
                        .xsmall()
                        .tooltip("Clear the filters")
                        .on_click(cx.listener(|this, _, window, cx| this.clear_filters(window, cx))),
                );
            }
            if running.is_some() {
                status = status.child("Loading…");
            }
        } else if running.is_none() {
            status = status.child(if key.is_some() { format!("{RUN_KEY} to run") } else { "No statement under the cursor".to_string() });
        }
        let mut actions = h_flex().flex_none().gap_1();
        if self.ui.table.read(cx).delegate().has_json {
            let on = self.json_lines() > 0;
            actions = actions.child(
                Button::new("json-preview")
                    .label("{ }")
                    .ghost()
                    .xsmall()
                    .selected(on)
                    .tooltip(if on { "JSON preview on: click for one line per row" } else { "Preview JSON cells over several lines" })
                    .on_click(cx.listener(|this, _, _, cx| this.toggle_json_preview(cx))),
            );
        }
        if done.as_ref().is_some_and(|d| d.outcome().is_some_and(|o| !o.columns.is_empty())) {
            actions = actions
                .child(
                    Button::new("filters")
                        .icon(IconName::Search)
                        .ghost()
                        .xsmall()
                        .selected(self.ui.filters_open)
                        .tooltip("Column filters (Cmd+Alt+F)")
                        .on_click(cx.listener(|this, _, window, cx| this.toggle_filters(window, cx))),
                )
                .child(Button::new("copy-result").icon(IconName::Copy).ghost().xsmall().tooltip("Copy the page as TSV").on_click(cx.listener(|this, _, window, cx| this.copy_result(window, cx))));
            if done.as_ref().is_some_and(|d| d.kind == Kind::Query) {
                let editing = self.shown_edit(cx).is_some();
                actions = actions.child(
                    Button::new("edit-result")
                        .icon(icon(include_bytes!("../assets/pencil.svg")))
                        .label("Edit")
                        .ghost()
                        .xsmall()
                        .selected(editing)
                        .tooltip(if editing { "Leave edit mode" } else { "Edit cells: finds the table and its primary key, then UPDATEs by key after you confirm" })
                        .on_click(cx.listener(|this, _, window, cx| this.toggle_edit(window, cx))),
                );
            }
        }
        if done.is_some() {
            actions = actions.child(Button::new("page-sql").label("Show page SQL").ghost().xsmall().on_click(cx.listener(|this, _, _, cx| {
                if let Some(e) = this.shown_key(cx).and_then(|k| this.store.entries.get_mut(&k)) {
                    e.tab = Tab::Query;
                }
                cx.notify();
            })));
        }
        let line2 = h_flex().gap_2().items_center().justify_between().child(status).child(actions);
        let batch = self.batch_strip(key.as_ref(), cx);
        let header = v_flex().flex_none().gap_1().px_3().py_1p5().border_b_1().border_color(border).child(line1).child(line2).children(batch);

        let tabs = done.as_ref().map(|d| {
            let msgs = match &d.result {
                Err(_) => Some(danger),
                Ok(o) if !o.notices.is_empty() => Some(warning),
                _ => None,
            };
            TabBar::new("result-tabs").pill().small().selected_index(tab as usize).on_click(cx.listener(|this, ix: &usize, _, cx| {
                if let Some(e) = this.shown_key(cx).and_then(|k| this.store.entries.get_mut(&k)) {
                    e.tab = [Tab::Result, Tab::Messages, Tab::Query][(*ix).min(2)];
                }
                cx.notify();
            }))
            .child(TabItem::new().label("Result"))
            .child(match msgs {
                Some(c) => TabItem::new().label("Messages").suffix(div().size(fs * 0.7).rounded_full().bg(c).mr(fs * 0.4)),
                None => TabItem::new().label("Messages"),
            })
            .child(TabItem::new().label("Query"))
        });

        let body = match (&done, tab) {
            (None, _) => v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .text_color(muted)
                .child(if running.is_some() { "Running…".to_string() } else { format!("{RUN_KEY} to run") })
                .into_any_element(),
            (Some(_), Tab::Messages | Tab::Query) => div().flex_1().min_h_0().child(gpui_component::input::Editor::new(&self.ui.meta).readonly(true).size_full().bordered(false)).into_any_element(),
            (Some(d), Tab::Result) => match &d.result {
                Err(err) => v_flex()
                    .flex_1()
                    .min_h_0()
                    .p_3()
                    .gap_1()
                    .text_sm()
                    .child(div().text_color(danger).font_weight(FontWeight::SEMIBOLD).child(match &err.code {
                        Some(c) => format!("{c}: {}", err.message),
                        None => err.message.clone(),
                    }))
                    .children(err.detail.clone().map(|x| div().child(format!("DETAIL: {x}"))))
                    .children(err.hint.clone().map(|x| div().text_color(muted).child(format!("HINT: {x}"))))
                    .into_any_element(),
                Ok(o) if o.columns.is_empty() => v_flex()
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .gap_1()
                    .child(div().text_color(fg).child(rows_label(o)))
                    .children(o.notices.first().map(|n| div().text_xs().text_color(muted).child(n.clone())))
                    .into_any_element(),
                Ok(_) => self.table_view(running.is_some(), primary, window, cx),
            },
        };
        let footer = (tab == Tab::Result).then(|| self.footer(key.as_ref(), cx)).flatten();
        let edit_bar = self.edit_bar(cx);
        let panel = v_flex().size_full().child(header).children(edit_bar).children(tabs.map(|t| div().flex_none().px_2().py_1().child(t))).child(body).children(footer);
        panel.into_any_element()
    }

    /// `Result 1 | Result 2 …` and `N statements · M rows affected` for a multi-statement run.
    fn batch_strip(&self, key: Option<&Key>, cx: &mut Context<Self>) -> Option<AnyElement> {
        let key = key?;
        let keys = self.store.batches.get(&key.0).filter(|b| b.len() > 1 && b.contains(key))?;
        let affected: u64 = keys.iter().filter_map(|k| self.done_of(k)).filter_map(|d| d.outcome().and_then(|o| o.rows_affected)).sum();
        let ran = keys.iter().filter(|k| self.entry(k).is_some_and(|e| e.done.is_some())).count();
        let mut row = h_flex().gap_1().items_center().flex_wrap();
        for (i, k) in keys.iter().enumerate() {
            let failed = self.done_of(k).is_some_and(|d| d.result.is_err());
            let (path, text) = k.clone();
            let b = Button::new(("batch", i))
                .label(format!("Result {}{}", i + 1, if failed { " ✕" } else { "" }))
                .ghost()
                .xsmall()
                .selected(k == key)
                .on_click(cx.listener(move |this, _, window, cx| this.go_to_statement_text(&path, &text, window, cx)));
            row = row.child(b);
        }
        let summary = format!("{} statements · {affected} rows affected{}", keys.len(), if ran < keys.len() { " (stopped)" } else { "" });
        Some(row.child(div().ml_2().text_xs().text_color(cx.theme().muted_foreground).child(summary)).into_any_element())
    }

    /// A batch tab: move the cursor to that statement, which brings its result up.
    fn go_to_statement_text(&mut self, path: &Path, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(d) = self.docs.iter().find(|d| *d.path == path) else { return };
        let editor = d.editor.clone();
        editor.update(cx, |s, cx| {
            if let Some(at) = s.value().find(text) {
                let pos = s.text().offset_to_position(at);
                s.set_cursor_position(pos, window, cx);
            }
        });
    }

    fn table_view(&self, loading: bool, _primary: Hsla, _: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme();
        let open = self.ui.filters_open;
        // The names alone: a row's own height (the table's default). The filter row adds a field under them.
        let head = t.font_size * 3.4;
        div()
            .id("result-table")
            .key_context("ResultTable")
            .flex_1()
            .min_h_0()
            .when(loading, |d| d.opacity(0.55))
            .on_action(cx.listener(|this, _: &crate::CopyCells, window, cx| this.copy_selection(window, cx)))
            .on_action(cx.listener(|this, _: &crate::OpenCell, window, cx| this.open_selected(window, cx)))
            .on_action(cx.listener(|this, _: &crate::OpenRow, window, cx| this.open_selected_row(window, cx)))
            .on_action(cx.listener(|this, _: &crate::NextPage, window, cx| this.step_page(true, window, cx)))
            .on_action(cx.listener(|this, _: &crate::PrevPage, window, cx| this.step_page(false, window, cx)))
            .child(DataTable::new(&self.ui.table).small().bordered(false).stripe(true).when(open, |d| d.header_row_height(head)))
            .into_any_element()
    }

    /// `‹ 1 / ? ›`, the page size, "Count all"; Query results only.
    fn footer(&self, key: Option<&Key>, cx: &mut Context<Self>) -> Option<AnyElement> {
        let e = self.entry(key?)?;
        let done = e.done.as_ref()?;
        let o = done.outcome().filter(|o| !o.columns.is_empty())?;
        let t = cx.theme();
        let muted = t.muted_foreground;
        let mut row = h_flex().flex_none().gap_2().items_center().px_3().py_1().border_t_1().border_color(t.border).text_xs().text_color(muted);
        if !done.paged() {
            let sorted = !e.page.sort.is_empty();
            row = row.child(format!("{}{}", rows_label(o), if sorted { " · sorted locally" } else { "" }));
            if o.truncated {
                row = row.child(div().text_color(t.warning).child("The result is cut at results.max_rows"));
            }
            return Some(row.into_any_element());
        }
        let (offset, size) = (done.page.offset, done.page.size);
        let total = e.count.as_ref().filter(|(f, _)| *f == done.page.filters).map(|(_, n)| *n);
        let sizes = [25u64, 50, 100, 200, 500, 1000];
        let view = cx.entity().downgrade();
        row = row
            .child(Button::new("page-prev").icon(IconName::ChevronLeft).ghost().xsmall().disabled(offset == 0).tooltip("Previous page (Cmd+[)").on_click(cx.listener(|this, _, window, cx| this.step_page(false, window, cx))))
            .child(page_label(offset, size, total))
            .child(Button::new("page-next").icon(IconName::ChevronRight).ghost().xsmall().disabled(!o.has_more).tooltip("Next page (Cmd+])").on_click(cx.listener(|this, _, window, cx| this.step_page(true, window, cx))))
            .child(
                Button::new("page-size")
                    .label(format!("{size} / page"))
                    .ghost()
                    .xsmall()
                    .dropdown_caret(true)
                    .dropdown_menu(move |menu, _, _| {
                        sizes.iter().fold(menu, |menu, &n| {
                            let view = view.clone();
                            menu.item(PopupMenuItem::new(format!("{n} rows")).checked(n == size).on_click(move |_, window, cx| {
                                view.update(cx, |this, cx| this.set_page_size(n, window, cx)).ok();
                            }))
                        })
                    }),
            );
        row = match total {
            Some(n) => row.child(format!("{n} rows in total")),
            None => row.child(Button::new("count-all").label("Count all").ghost().xsmall().on_click(cx.listener(|this, _, window, cx| this.count_all(window, cx)))),
        };
        Some(row.into_any_element())
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: it brings `gpui::test`, which shadows `#[test]`.
    use super::{JsonTip, TIP_LINE_CHARS, TIP_LINES, Tok, json_tokens};

    #[test]
    fn json_line_tokens() {
        let toks = |l: &str| json_tokens(l).into_iter().map(|(r, k)| (l[r].to_string(), k)).collect::<Vec<_>>();
        assert_eq!(toks(r#"  "a\"b": "x:y","#), [(r#""a\"b""#.into(), Tok::Key), (r#""x:y""#.into(), Tok::Str)]);
        assert_eq!(toks(r#"  "n": -1.5e3,"#), [(r#""n""#.into(), Tok::Key), ("-1.5e3".into(), Tok::Num)]);
        assert_eq!(toks("  true, false, null"), [("true".into(), Tok::Bool), ("false".into(), Tok::Bool), ("null".into(), Tok::Null)]);
        // A cut-off line ends inside its string.
        assert_eq!(toks(r#"  "ключ…"#), [(r#""ключ…"#.into(), Tok::Str)]);
        assert!(toks("  },").is_empty());
    }

    #[test]
    fn json_tip_is_pretty_and_capped() {
        let small = JsonTip::new(&serde_json::json!({"a": [1, 2]}));
        assert_eq!(small.lines, ["{", "  \"a\": [", "    1,", "    2", "  ]", "}"]);
        let big = JsonTip::new(&serde_json::Value::Array((0..TIP_LINES).map(|i| i.into()).collect()));
        assert_eq!(big.lines.len(), TIP_LINES + 1);
        assert_eq!(big.lines.last().unwrap().as_ref(), "… 2 more lines: double-click the cell for the whole value");
        let wide = JsonTip::new(&serde_json::json!(["x".repeat(TIP_LINE_CHARS + 10)]));
        assert_eq!(wide.lines[1].chars().count(), TIP_LINE_CHARS + 1);
    }
}
