//! One open file: its editor, the latest statement split, connection and the active statement.

use std::{ops::Range, path::PathBuf, sync::Arc};

use db_conn::{Connections, Driver};
use gpui::*;
use gpui_component::{
    ActiveTheme as _,
    input::{EditorState, GutterDecoration, GutterDecorationCollection, Rope, RopeExt as _},
};
use sql_split::{Dialect, SqlFile};

pub struct Doc {
    pub path: Arc<PathBuf>,
    pub editor: Entity<EditorState>,
    /// `false` for other files (connection JSON, anything shown with "all files"): plain editing.
    pub sql: bool,
    /// The split of the current text, refreshed after every edit.
    pub file: Arc<SqlFile>,
    /// [`outline`] of the current text, made with the split: the tree reads it every frame.
    pub outline: Vec<(usize, String)>,
    pub dialect: Dialect,
    /// The connections of the config dir.
    pub conns: Arc<Connections>,
    /// Index into `file.statements` of the statement under the cursor.
    pub active: Option<usize>,
    /// Byte range currently decorated as the active statement.
    applied: Option<Range<usize>>,
    /// 0-based first / last line of the active statement, for its background tint.
    stmt_lines: Option<(usize, usize)>,
    stmt_bar: GutterDecorationCollection,
    /// Edited since load / last save.
    pub dirty: bool,
    /// The file's mtime when it was last read or written: a newer one means it changed on disk.
    pub mtime: Option<std::time::SystemTime>,
    /// What the completion provider reads: connection, dialect, schema cache.
    pub lang: std::rc::Rc<std::cell::RefCell<crate::lang::Ctx>>,
    pub _subs: Vec<Subscription>,
}

pub fn dialect_of(driver: Driver) -> Dialect {
    db_exec::dialect(driver)
}

/// The schema whose tables are written without a prefix: Postgres' first `search_path` entry
/// (`public` by default); MySQL has none (the connection's database is its only schema).
pub fn default_schema(c: &db_conn::Connection) -> Option<String> {
    match c.driver {
        Driver::Postgres => Some(c.schema.as_deref().and_then(|s| s.split(',').next()).map_or("public", str::trim).to_string()),
        Driver::MySql => c.database.clone(),
        Driver::Elastic => None,
    }
}

impl Doc {
    pub fn new(path: PathBuf, editor: Entity<EditorState>, conns: Arc<Connections>, cache: crate::schema_cache::Shared, subs: Vec<Subscription>, cx: &mut App) -> Self {
        let stmt_bar = editor.update(cx, |s, cx| s.create_gutter_decorations_collection(vec![], cx));
        let sql = siesta_workspace::is_sql(&path);
        let dialect = siesta_workspace::file_dialect(&path);
        let lang = std::rc::Rc::new(std::cell::RefCell::new(crate::lang::Ctx { dialect, connection: None, default_schema: None, cache }));
        // ponytail: no completion in `.es` files yet; index / field names from the schema would be next.
        if sql && dialect != Dialect::Elastic {
            let provider = std::rc::Rc::new(crate::lang::Lang(lang.clone()));
            editor.update(cx, |s, _| s.lsp_mut().completion_provider = Some(provider));
        }
        let mut doc = Self {
            path: Arc::new(path),
            editor,
            sql,
            file: Arc::default(),
            outline: vec![],
            dialect,
            conns,
            active: None,
            applied: None,
            stmt_lines: None,
            stmt_bar,
            dirty: false,
            mtime: None,
            lang,
            _subs: subs,
        };
        doc.mtime = mtime(&doc.path);
        doc.reparse(cx);
        doc
    }

    pub fn name(&self) -> String {
        self.path.file_name().map_or_else(|| "untitled".into(), |n| n.to_string_lossy().into_owned())
    }

    /// The file's `-- @connection` (the picker writes it, as Respite's `# @env`); `None` = no connection.
    pub fn connection(&self) -> Option<&str> {
        self.file.connection.as_deref()
    }

    /// The file's `-- @database`; `None` = the connection's default.
    pub fn database(&self) -> Option<&str> {
        self.file.database.as_deref()
    }

    /// The database shown for the file: its choice, else the connection's own, else `None`
    /// (the server's default).
    pub fn shown_database(&self) -> Option<String> {
        self.effective_connection().and_then(|c| c.database)
    }

    /// The file's connection pointed at its database (as the CLI without flags: the directives, else the default).
    pub fn effective_connection(&self) -> Option<db_conn::Connection> {
        self.conns.connections.get(self.connection()?).map(|c| c.with_database(self.database()))
    }

    /// Schema cache key of the file's connection and database (`None`: no connection, or an unusable one).
    pub fn schema_key(&self) -> Option<String> {
        self.effective_connection().as_ref().map(crate::schema_cache::key)
    }

    /// Dialect of the file with a named connection: `.es` files are always Elastic; `.sql` files
    /// follow their SQL connection, Postgres without one (an Elasticsearch connection on a `.sql`
    /// file is a mismatch the picker shows, not a reason to split SQL as requests).
    fn dialect_for(&self, name: Option<&str>) -> Dialect {
        if self.dialect == Dialect::Elastic {
            return Dialect::Elastic;
        }
        match name.and_then(|n| self.conns.connections.get(n)).map(|c| dialect_of(c.driver)) {
            Some(Dialect::Elastic) | None => Dialect::default(),
            Some(d) => d,
        }
    }

    /// The file's connection exists but speaks another language (`.es` on SQL, `.sql` on Elasticsearch).
    pub fn connection_mismatch(&self) -> Option<&'static str> {
        let c = self.conns.connections.get(self.connection()?)?;
        match (self.dialect == Dialect::Elastic, c.driver == Driver::Elastic) {
            (true, false) => Some(".es files run on Elasticsearch connections"),
            (false, true) => Some("Elasticsearch connections run .es files"),
            _ => None,
        }
    }

    /// Split the text again (after an edit or a connection change) and refresh the underlines and
    /// the active statement. The dialect follows the connection, which `@connection` may name, so
    /// a file that changes dialect is split a second time.
    pub fn reparse(&mut self, cx: &mut App) {
        if !self.sql {
            return;
        }
        let text = self.editor.read(cx).value();
        let mut file = sql_split::split(&text, self.dialect);
        let wanted = self.dialect_for(file.connection.as_deref());
        if wanted != self.dialect {
            self.dialect = wanted;
            file = sql_split::split(&text, wanted);
        }
        self.outline = outline(&text, &file);
        self.file = Arc::new(file);
        let default_schema = self.effective_connection().as_ref().and_then(default_schema);
        let mut lang = self.lang.borrow_mut();
        (lang.dialect, lang.connection, lang.default_schema) = (self.dialect, self.schema_key(), default_schema);
        drop(lang);
        self.publish_diagnostics(cx);
        self.sync_active(cx);
    }

    /// Splitter findings (unclosed string / comment / `$tag$`, directive warnings) as underlines.
    /// The editor drops them on every edit, so this runs after each reparse.
    fn publish_diagnostics(&self, cx: &mut App) {
        let found = self.file.diagnostics.clone();
        self.editor.update(cx, |s, cx| {
            let rope = s.text().clone();
            let list: Vec<_> = found.iter().map(|d| diagnostic(&rope, d)).collect();
            if let Some(set) = s.diagnostics_mut() {
                set.clear();
                set.extend(list);
                cx.notify();
            }
        });
    }

    pub fn cursor_offset(&self, cx: &App) -> usize {
        self.editor.read(cx).cursor()
    }

    pub fn active_statement(&self) -> Option<&sql_split::Statement> {
        self.active.and_then(|i| self.file.statements.get(i))
    }

    pub fn sync_active(&mut self, cx: &mut App) {
        let offset = self.cursor_offset(cx);
        let stmt = self.file.statement_at(offset);
        self.active = stmt.and_then(|st| self.file.statements.iter().position(|s| std::ptr::eq(s, st)));
        let range = stmt.map(|st| st.range.clone());
        let rope = self.editor.read(cx).text().clone();
        self.stmt_lines = range.as_ref().map(|r| (rope.offset_to_position(r.start).line as usize, rope.offset_to_position(r.end).line as usize));
        let range = self.stmt_lines.map(|(first, last)| rope.line_start_offset(first)..rope.line_end_offset(last));
        // Setting decorations notifies the editor, which re-enters the observer:
        // touch them only when the statement actually changed, or it loops forever.
        if range == self.applied {
            return;
        }
        self.applied = range.clone();
        let bar = cx.theme().primary;
        self.stmt_bar.set(range.iter().map(|r| GutterDecoration::new(r.clone()).with_color(bar)).collect(), cx);
    }

    /// A faint full-width tint over the active statement, laid over the editor (whose own range
    /// fill is ragged: it follows each line's text). Geometry is read at paint time, after the
    /// editor's prepaint, so it tracks scrolling and wrapping.
    pub fn statement_tint(&self) -> impl IntoElement {
        let (editor, lines) = (self.editor.clone(), self.stmt_lines);
        canvas(
            |_, _, _| {},
            move |bounds, _, window, cx| {
                let Some((first, last)) = lines else { return };
                let s = editor.read(cx);
                let Some(vis) = s.visible_row_range().filter(|v| first < v.end && last >= v.start) else { return };
                let text = s.text();
                // An offset above the viewport resolves to the first visible line, so `top` clamps.
                let Some(top) = s.range_to_bounds(&(text.line_start_offset(first)..text.line_start_offset(first))) else { return };
                let end = text.line_end_offset(last);
                let bottom = (last < vis.end).then(|| s.range_to_bounds(&(end..end))).flatten().map_or(bounds.bottom(), |b| b.bottom());
                let left = top.left().max(bounds.left());
                let rect = Bounds::from_corners(point(left, top.top().max(bounds.top())), point(bounds.right(), bottom.min(bounds.bottom())));
                window.paint_quad(fill(rect, cx.theme().primary.opacity(0.07)));
            },
        )
        .absolute()
        .inset_0()
    }

    /// A green ▶ in the gutter at each visible statement's first line, right of its number (a red ■
    /// while it runs; setting `editor.run_button`). Painted over the editor at paint time from the
    /// layout just made, so it follows edits, wrapping, folding and scrolling. A click runs (or
    /// stops) that statement. Copied from Respite's `Doc::send_buttons`.
    pub fn run_buttons(&self, running: Vec<usize>, view: WeakEntity<crate::Siesta>) -> impl IntoElement {
        let (editor, path) = (self.editor.clone(), self.path.clone());
        let starts: Vec<usize> = self.file.statements.iter().map(|s| s.range.start).collect();
        canvas(
            move |bounds, window, cx| {
                let s = editor.read(cx);
                let Some(vis) = s.visible_row_range() else { return vec![] };
                let (text, map) = (s.text(), s.display_map());
                let mut out = vec![];
                for &start in &starts {
                    let row = text.offset_to_position(start).line as usize;
                    if map.is_buffer_line_hidden(row) || !vis.contains(&map.buffer_line_to_display_row(row)) {
                        continue;
                    }
                    let line_start = text.line_start_offset(row);
                    let Some(at) = s.range_to_bounds(&(line_start..line_start)) else { continue };
                    // A square as tall as the line, ending just before the text: right of the number.
                    let side = at.size.height;
                    let rect = Bounds::new(point((at.left() - side - px(2.)).max(bounds.left()), at.top()), size(side, side));
                    if rect.top() < bounds.top() || rect.bottom() > bounds.bottom() {
                        continue;
                    }
                    let hitbox = window.insert_hitbox(rect, HitboxBehavior::BlockMouseExceptScroll);
                    out.push((rect, hitbox, row + 1, running.contains(&start)));
                }
                out
            },
            move |_, buttons, window, cx| {
                for (rect, hitbox, line, busy) in buttons {
                    paint_arrow(busy, hitbox.is_hovered(window), rect, window, cx);
                    window.set_cursor_style(CursorStyle::PointingHand, &hitbox);
                    let (view, path) = (view.clone(), path.clone());
                    window.on_mouse_event(move |e: &MouseDownEvent, phase, window, cx| {
                        if phase == DispatchPhase::Bubble && e.button == MouseButton::Left && hitbox.is_hovered(window) {
                            cx.stop_propagation();
                            view.update(cx, |this, cx| this.run_or_stop(&path, line, busy, window, cx)).ok();
                        }
                    });
                }
            },
        )
        .absolute()
        .inset_0()
    }
}

/// A green ▶ (red ■ while running) in the gutter, at the statement's first line.
fn paint_arrow(busy: bool, hover: bool, rect: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
    let t = cx.theme();
    let color = if busy { t.danger } else { t.success };
    if hover {
        window.paint_quad(fill(rect, t.list_hover).corner_radii(px(4.)));
    }
    let font = window.text_style().font();
    let label = if busy { "■" } else { "▶" };
    let run = TextRun { len: label.len(), font, color, background_color: None, underline: None, strikethrough: None };
    let s = window.text_system().shape_line(SharedString::from(label), t.font_size * 1.15, &[run], None);
    let at = point(rect.left() + (rect.size.width - s.width) / 2., rect.top());
    let _ = s.paint(at, rect.size.height, TextAlign::Left, None, window, cx);
}

/// Editor underline for a splitter finding.
fn diagnostic(rope: &Rope, d: &sql_split::Diagnostic) -> lsp_types::Diagnostic {
    lsp_types::Diagnostic {
        range: lsp_types::Range::new(rope.offset_to_position(d.range.start), rope.offset_to_position(d.range.end)),
        severity: Some(match d.severity {
            sql_split::Severity::Error => lsp_types::DiagnosticSeverity::ERROR,
            sql_split::Severity::Warning => lsp_types::DiagnosticSeverity::WARNING,
        }),
        source: Some("siesta".into()),
        message: d.message.clone(),
        ..Default::default()
    }
}

pub fn mtime(path: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// The smallest byte range of `old` whose replacement gives `new` (one undo step, cursor kept).
pub fn changed(old: &str, new: &str) -> (Range<usize>, String) {
    let pre = old.bytes().zip(new.bytes()).take_while(|(a, b)| a == b).count();
    let pre = (0..=pre).rev().find(|&i| old.is_char_boundary(i) && new.is_char_boundary(i)).unwrap_or(0);
    let max = old.len().min(new.len()) - pre;
    let suf = old.bytes().rev().zip(new.bytes().rev()).take(max).take_while(|(a, b)| a == b).count();
    let suf = (0..=suf).rev().find(|&i| old.is_char_boundary(old.len() - i) && new.is_char_boundary(new.len() - i)).unwrap_or(0);
    (pre..old.len() - suf, new[pre..new.len() - suf].to_string())
}

/// `(line, label)` per statement: `@name` or the first line; `line` is 1-based.
pub fn outline(src: &str, file: &SqlFile) -> Vec<(usize, String)> {
    file.statements.iter().map(|s| (s.line, s.label(src).to_string())).collect()
}
