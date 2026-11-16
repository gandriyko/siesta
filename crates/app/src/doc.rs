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
    pub dialect: Dialect,
    /// Connections visible from this file's `siesta*.json` chain.
    pub conns: Arc<Connections>,
    /// The picker's choice for this file; `-- @connection` in the text wins over it.
    pub picked: Option<String>,
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
    match driver {
        Driver::Postgres => Dialect::Postgres,
        Driver::MySql => Dialect::MySql,
    }
}

/// The schema whose tables are written without a prefix: Postgres' first `search_path` entry
/// (`public` by default); MySQL has none (the connection's database is its only schema).
pub fn default_schema(c: &db_conn::Connection) -> Option<String> {
    match c.driver {
        Driver::Postgres => Some(c.schema.as_deref().and_then(|s| s.split(',').next()).map_or("public", str::trim).to_string()),
        Driver::MySql => c.database.clone(),
    }
}

impl Doc {
    pub fn new(
        path: PathBuf,
        editor: Entity<EditorState>,
        conns: Arc<Connections>,
        picked: Option<String>,
        cache: crate::schema_cache::Shared,
        subs: Vec<Subscription>,
        cx: &mut App,
    ) -> Self {
        let stmt_bar = editor.update(cx, |s, cx| s.create_gutter_decorations_collection(vec![], cx));
        let sql = siesta_workspace::is_sql(&path);
        let lang = std::rc::Rc::new(std::cell::RefCell::new(crate::lang::Ctx { dialect: Dialect::default(), connection: None, default_schema: None, cache }));
        if sql {
            let provider = std::rc::Rc::new(crate::lang::Lang(lang.clone()));
            editor.update(cx, |s, _| s.lsp_mut().completion_provider = Some(provider));
        }
        let mut doc = Self {
            path: Arc::new(path),
            editor,
            sql,
            file: Arc::default(),
            dialect: Dialect::default(),
            conns,
            picked,
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

    /// `-- @connection` wins over the picker; `None` = no connection.
    pub fn connection(&self) -> Option<&str> {
        self.file.connection.as_deref().or(self.picked.as_deref())
    }

    /// The connection a `-- @connection` directive fixes; while it is there the picker is locked.
    pub fn locked(&self) -> Option<&str> {
        self.file.connection.as_deref()
    }

    /// Schema cache key of the file's connection (`None`: no connection, or an unusable one).
    pub fn schema_key(&self) -> Option<String> {
        self.conns.connections.get(self.connection()?).map(crate::schema_cache::key)
    }

    /// Dialect of a named connection; Postgres without one (or when it is unknown).
    fn dialect_for(&self, name: Option<&str>) -> Dialect {
        name.and_then(|n| self.conns.connections.get(n)).map_or_else(Dialect::default, |c| dialect_of(c.driver))
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
        let wanted = self.dialect_for(file.connection.as_deref().or(self.picked.as_deref()));
        if wanted != self.dialect {
            self.dialect = wanted;
            file = sql_split::split(&text, wanted);
        }
        self.file = Arc::new(file);
        let connection = self.connection().map(str::to_string);
        let default_schema = connection.as_deref().and_then(|c| self.conns.connections.get(c)).and_then(default_schema);
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
