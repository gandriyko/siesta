//! Siesta GUI: sidebar (Files | DB), tabs of open files, a plain-text SQL editor (no run buttons or
//! statuses in its lines), connection picker in the top-right corner, result panel on the right.
//!
//! Running statements lives in `runner`, the result panel and table in `results`, the value
//! dialog in `value_popup`, pure formatting in `format`; everything else of the shell is here.

mod complete;
mod conn_form;
mod history_page;
mod connections_page;
mod dbtab;
mod doc;
mod format;
mod highlight;
mod keymap;
mod lang;
mod palette;
mod results;
mod runner;
mod schema_cache;
mod settings_page;
mod sidebar;
mod themes;
mod value_popup;

use std::{
    cell::RefCell,
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
};

use db_conn::Connections;
use doc::Doc;
use gpui::{prelude::FluentBuilder as _, *};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Root, Sizable as _, Theme, ThemeMode, WindowExt as _, h_flex, h_resizable, v_resizable,
    button::{Button, ButtonVariants as _},
    input::{Editor, EditorState, InputEvent, InputState, Position, RopeExt as _},
    menu::{DropdownMenu as _, PopupMenu, PopupMenuItem},
    resizable_panel,
};
use siesta_workspace::{OpenFile, WorkspaceState};

actions!(
    siesta,
    [
        RunStatement, RunAll, RepeatLast, CancelRun, SaveFile, CloseTab, ToggleSidebar, NextTab, OpenSettings, OpenConnections, OpenFolder, OpenSingleFile, Quit,
        PickConnection, ToggleResult, ToggleSplit, GoToFile, GoToStatement, GoToWorkspaceStatement, CommandPalette, PrevStatement,
        NextStatement, FocusEditor, ToggleFilters, RefreshSchema, ToggleHistory,
        // In the result table ("ResultTable" context) and the value dialog ("ValuePopup").
        CopyCells, OpenCell, OpenRow, NextPage, PrevPage, PopupPrev, PopupNext
    ]
);

/// Shown in an empty workspace (not written until saved).
const SAMPLE: &str = "-- @connection local\nSELECT 1;\n";
#[cfg(target_os = "macos")]
const RUN_KEY: &str = "Cmd+Enter";
#[cfg(not(target_os = "macos"))]
const RUN_KEY: &str = "Ctrl+Enter";

/// A notification that is safe while the window is still being built (no `Root` yet: an
/// immediate one panics), so startup paths use this.
fn notify_later(window: &mut Window, cx: &mut App, msg: impl Into<SharedString>) {
    let msg = msg.into();
    window.defer(cx, move |window, cx| window.push_notification(msg, cx));
}

/// The user's `keymap.json`, next to the global `settings.json`.
fn keymap_path(s: &siesta_settings::Loaded) -> Option<PathBuf> {
    Some(s.global_path.as_ref()?.parent()?.join("keymap.json"))
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SideTab {
    Files,
    Db,
}

/// Files and their mtimes, to notice changes on disk.
type Stamp = Vec<(PathBuf, Option<std::time::SystemTime>)>;

pub(crate) struct Siesta {
    /// The workspace folder: root of the file tree and of saved state.
    root: PathBuf,
    /// The connections of the config dir, shared by every open file.
    conns: Arc<Connections>,
    /// Where `state.json`, history and the schema cache live (`~/.siesta`); `None` = nothing is saved.
    base: Option<PathBuf>,
    docs: Vec<Doc>,
    current: Option<usize>,
    /// The workspace's `.sql` files (nothing else is shown).
    files: Vec<PathBuf>,
    /// Workspaces opened before (newest first), offered when this one has no `.sql` files.
    recent: Vec<PathBuf>,
    side_tab: SideTab,
    /// Expanded files in the sidebar.
    expanded: HashSet<PathBuf>,
    /// `(line, label)` of statements of files that are not open, split when expanded.
    outlines: HashMap<PathBuf, Vec<(usize, String)>>,
    rows: Vec<sidebar::Row>,
    sidebar_filter: Entity<InputState>,
    /// Schemas per connection name, in memory and in `~/.siesta/cache`; shared with the editors' completion.
    schemas: schema_cache::Shared,
    /// DB tab: the tree's rows, the filter, and the toggled nodes (see `dbtab::rows`).
    db_rows: Vec<dbtab::DbRow>,
    db_filter: Entity<InputState>,
    db_toggled: HashSet<String>,
    /// Folders are open by default; this holds the collapsed ones.
    collapsed_dirs: HashSet<PathBuf>,
    sidebar: bool,
    /// Cmd+J hides the result panel.
    result_open: bool,
    /// The connection picked per file (`None` = "No connection" chosen); persisted.
    picks: BTreeMap<PathBuf, Option<String>>,
    /// The database picked per file (absent = the connection's default); persisted.
    db_picks: BTreeMap<PathBuf, String>,
    /// The databases each connection's server offers (`schema_cache::key` of the connection as
    /// saved), read in the background when a picker opens; kept until "Refresh".
    db_lists: HashMap<String, runner::DbList>,
    /// The database picker's menu while it exists (rebuilt when a list arrives with it open).
    db_menu: Rc<RefCell<Option<WeakEntity<PopupMenu>>>>,
    /// The connection files at the last check; a change reloads connections.
    conns_seen: Option<Stamp>,
    /// A dirty tab whose close was requested once; the next close discards it.
    close_armed: Option<PathBuf>,
    /// Re-renders the open search dialog (Cmd+P …) as its query changes.
    picker_sub: Option<Subscription>,
    /// Tabs, most recently used first (Ctrl+Tab).
    mru: Vec<PathBuf>,
    /// Ctrl+Tab held down: the MRU order when it started and how far it has gone.
    tab_cycle: Option<(Vec<PathBuf>, usize)>,
    /// Sessions per (file, connection, database), opened by the first run and closed with the file.
    slots: HashMap<runner::SlotKey, runner::Slot>,
    /// Results per (file, statement text).
    store: results::Store,
    /// The result panel's table, filter fields and pin.
    ui: results::Ui,
    /// The "Running…" clock is ticking.
    ticking: bool,
    settings: siesta_settings::Loaded,
    settings_window: Option<WindowHandle<Root>>,
    connections_window: Option<WindowHandle<Root>>,
    /// `~/.config/siesta`: `connections/<name>.json` live here; `None` = no connections.
    config_dir: Option<PathBuf>,
    /// The open New / Edit connection form. Weak: the dialog owns it, so closing the dialog any way
    /// (Esc, ×, Close) drops the form and with it a running Test.
    conn_form: Option<WeakEntity<conn_form::ConnForm>>,
    /// History page (Cmd+Y) in place of the editor.
    show_history: bool,
    /// This project's history, newest first; read when the page opens and after runs.
    history: Vec<db_exec::history::Entry>,
    /// Indices into `history` matching the filter (the list's rows).
    history_rows: Vec<usize>,
    history_filter: Entity<InputState>,
    history_scroll: UniformListScrollHandle,
    /// Holds focus when nothing else does (no tabs), so shortcuts still reach the "Siesta" context.
    focus: FocusHandle,
    /// Theme's own monospace font, used when `font_family` is empty.
    default_mono: SharedString,
    _subs: Vec<Subscription>,
}

/// `path` or `path:line` (1-based). A real file named `a:3` wins over the suffix.
/// Fills the database picker's menu; re-run on the open menu when the list changes.
pub(crate) type DbMenuBuild = Rc<dyn Fn(PopupMenu, &mut Window, &mut Context<PopupMenu>) -> PopupMenu>;

/// One row of the database picker below "Default (...)".
#[derive(Debug, PartialEq)]
enum DbRow {
    Loading,
    Error(String),
    Database(String),
    Retry,
    Refresh,
}

fn db_rows(list: Option<&runner::DbList>) -> Vec<DbRow> {
    match list {
        None | Some(runner::DbList::Loading) => vec![DbRow::Loading, DbRow::Refresh],
        Some(runner::DbList::Failed(e)) => vec![DbRow::Error(db_error_text(e)), DbRow::Retry],
        Some(runner::DbList::Ready(names)) => names.iter().cloned().map(DbRow::Database).chain([DbRow::Refresh]).collect(),
    }
}

/// "Can't list databases: <first line, at most 80 chars>".
fn db_error_text(e: &str) -> String {
    let line = e.lines().next().unwrap_or("").trim();
    let short = if line.chars().count() > 80 { format!("{}…", line.chars().take(80).collect::<String>()) } else { line.to_string() };
    format!("Can't list databases: {short}")
}

fn parse_target(arg: &str) -> (PathBuf, Option<u32>) {
    if let Some((p, l)) = arg.rsplit_once(':')
        && let Ok(n) = l.parse::<u32>()
        && !Path::new(arg).exists()
    {
        return (p.into(), Some(n));
    }
    (arg.into(), None)
}

impl Siesta {
    /// `open`: a file (and 1-based line) that is brought to the front after the saved tabs.
    fn new(
        root: PathBuf,
        open: Option<(PathBuf, Option<u32>)>,
        base: Option<PathBuf>,
        settings: siesta_settings::Loaded,
        config_dir: Option<PathBuf>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        for e in &settings.errors {
            notify_later(window, cx, format!("Settings: {e}"));
        }
        let sidebar_filter = cx.new(|cx| InputState::new(window, cx).placeholder("Search statements"));
        let db_filter = cx.new(|cx| InputState::new(window, cx).placeholder("Search tables and columns"));
        let history_filter = cx.new(|cx| InputState::new(window, cx).placeholder("Filter: SQL, connection, file, error"));
        // The close button: with unsaved files, ask (as Quit does) instead of closing.
        let view = cx.entity().downgrade();
        window.on_window_should_close(cx, move |window, cx| {
            view.update(cx, |this, cx| {
                // Unsaved files and open transactions both need an answer first.
                let busy = this.docs.iter().any(|d| d.dirty) || !this.paths_in_tx().is_empty();
                if busy {
                    this.confirm_quit(window, cx);
                }
                !busy
            })
            .unwrap_or(true)
        });
        let _subs = vec![
            cx.subscribe_in(&sidebar_filter, window, |_, _, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::Change) {
                    cx.notify();
                }
            }),
            cx.subscribe_in(&db_filter, window, |_, _, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::Change) {
                    cx.notify();
                }
            }),
            cx.subscribe_in(&history_filter, window, |this, _, ev: &InputEvent, _, cx| {
                if matches!(ev, InputEvent::Change) {
                    this.history_scroll.scroll_to_item(0, ScrollStrategy::Top);
                    cx.notify();
                }
            }),
            // Remember the open files and cursors for the next launch of this workspace.
            cx.on_app_quit(|this, cx| {
                this.remember(cx);
                async {}
            }),
            cx.observe_window_appearance(window, |this, window, cx| {
                if this.settings.settings.appearance == siesta_settings::Appearance::System {
                    Theme::sync_system_appearance(Some(window), cx);
                    // Switching mode reloads the highlight theme; re-apply the option.
                    themes::current_line(this.settings.settings.editor.highlight_current_line, cx);
                }
            }),
        ];
        let conns = Arc::new(config_dir.as_deref().map(Connections::load).unwrap_or_default());
        let mut this = Self {
            root: root.clone(),
            conns,
            base,
            docs: vec![],
            current: None,
            files: vec![],
            recent: vec![],
            side_tab: SideTab::Files,
            expanded: HashSet::new(),
            outlines: HashMap::new(),
            rows: vec![],
            sidebar_filter,
            schemas: Default::default(),
            db_rows: vec![],
            db_filter,
            db_toggled: HashSet::new(),
            collapsed_dirs: HashSet::new(),
            sidebar: true,
            result_open: true,
            picks: BTreeMap::new(),
            db_picks: BTreeMap::new(),
            db_lists: HashMap::new(),
            db_menu: Default::default(),
            conns_seen: None,
            close_armed: None,
            picker_sub: None,
            mru: vec![],
            tab_cycle: None,
            slots: HashMap::new(),
            store: results::Store::default(),
            ui: results::Ui::new(cx.entity().downgrade(), window, cx),
            ticking: false,
            settings,
            settings_window: None,
            connections_window: None,
            config_dir,
            conn_form: None,
            show_history: false,
            history: vec![],
            history_rows: vec![],
            history_filter,
            history_scroll: UniformListScrollHandle::new(),
            focus: cx.focus_handle(),
            default_mono: cx.theme().mono_font_family.clone(),
            _subs,
        };
        this.report_connections(&this.conns, window, cx);
        this.load_workspace(open, window, cx);
        this.apply_settings(window, cx);
        this
    }

    /// Scan the root and bring back the saved tabs; `open` (a launch argument) ends up in front.
    /// Without any file, a sample is offered (and written only on save).
    fn load_workspace(&mut self, open: Option<(PathBuf, Option<u32>)>, window: &mut Window, cx: &mut Context<Self>) {
        self.expanded = HashSet::new();
        self.rescan();
        let root = self.root.clone();
        self.recent = self.base.as_deref().map(siesta_workspace::recent).unwrap_or_default().into_iter().filter(|p| *p != root).collect();
        let state = self.base.as_deref().map(|b| siesta_workspace::load_state(b, &self.root)).unwrap_or_default();
        self.picks = state.connections;
        self.db_picks = state.databases;
        for f in state.open.iter().filter(|f| f.path.is_file()) {
            if let Some(ix) = self.open(f.path.clone(), None, window, cx) {
                self.docs[ix].editor.update(cx, |s, cx| s.set_cursor_position(Position::new(f.line, f.column), window, cx));
            }
        }
        if let Some(path) = state.current.filter(|p| self.docs.iter().any(|d| *d.path == *p)) {
            self.open(path, None, window, cx);
        }
        match open {
            Some((path, line)) => {
                self.open(path.clone(), None, window, cx);
                if let Some(line) = line {
                    self.go_to(path, line as usize, false, window, cx);
                }
            }
            None if self.docs.is_empty() => {
                let first = self.files.iter().find(|f| siesta_workspace::is_sql(f)).cloned();
                match first {
                    Some(path) => self.open(path, None, window, cx),
                    None => self.open(self.folder().join("untitled.sql"), Some(SAMPLE.into()), window, cx),
                };
            }
            None => {}
        }
    }

    fn rescan(&mut self) {
        self.files = siesta_workspace::scan(&self.root);
    }

    /// The folder relative paths count from: the workspace folder, or the single file's folder.
    pub(crate) fn folder(&self) -> &Path {
        siesta_workspace::folder(&self.root)
    }

    /// Cmd+O: pick another workspace folder. Refused while there are unsaved files.
    pub(crate) fn open_folder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions { files: false, directories: true, multiple: false, prompt: Some("Open Folder".into()) });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else { return };
            let Some(root) = paths.into_iter().next() else { return };
            this.update_in(cx, |this, window, cx| this.set_root(root, window, cx)).ok();
        })
        .detach();
    }

    /// File → Open File…: one `.sql` file alone becomes the workspace (its state and history are
    /// kept under the file's own path). Refused while there are unsaved files.
    pub(crate) fn open_single_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: false, prompt: Some("Open File".into()) });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else { return };
            let Some(file) = paths.into_iter().next() else { return };
            this.update_in(cx, |this, window, cx| this.set_root(file, window, cx)).ok();
        })
        .detach();
    }

    /// Switch the workspace to `root`: a folder, or a single file.
    fn set_root(&mut self, root: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if self.docs.iter().any(|d| d.dirty) {
            window.push_notification("Save or close the unsaved files before opening another workspace", cx);
            return;
        }
        self.remember(cx);
        self.docs.clear();
        self.current = None;
        self.mru.clear();
        self.outlines.clear();
        self.collapsed_dirs.clear();
        self.conns_seen = None;
        self.settings = siesta_settings::Loaded::load_from(self.settings.global_path.clone());
        for e in &self.settings.errors {
            window.push_notification(format!("Settings: {e}"), cx);
        }
        self.root = root;
        self.load_workspace(None, window, cx);
        self.apply_settings(window, cx);
        cx.notify();
    }

    /// Open files changed on disk: an unedited tab takes the new text (as an undoable edit), an
    /// edited one keeps the user's text and says so. A changed connection file reloads connections.
    fn check_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for ix in 0..self.docs.len() {
            let d = &mut self.docs[ix];
            let now = doc::mtime(&d.path);
            if now.is_none() || now == d.mtime {
                continue;
            }
            d.mtime = now;
            if d.dirty {
                window.push_notification(format!("{} changed on disk; your unsaved edits are kept (saving overwrites it)", d.name()), cx);
                continue;
            }
            let Ok(disk) = std::fs::read_to_string(&*d.path) else { continue };
            let editor = d.editor.clone();
            editor.update(cx, |s, cx| {
                let old = s.value().to_string();
                if old == disk {
                    return;
                }
                let (range, text) = doc::changed(&old, &disk);
                let rope = s.text();
                let range = lsp_types::Range::new(rope.offset_to_position(range.start), rope.offset_to_position(range.end));
                s.apply_lsp_edits(&vec![lsp_types::TextEdit { range, new_text: text }], window, cx);
            });
            // The edit marks it dirty; it matches the disk.
            self.docs[ix].dirty = false;
            self.outlines.remove(&*self.docs[ix].path.clone());
        }
        let conns = self.conns_stamp();
        if self.conns_seen.as_ref().is_some_and(|seen| *seen != conns) {
            self.reload_connections(window, cx);
        }
        self.conns_seen = Some(conns);
    }

    /// The connection files of the config dir with their mtimes; a change reloads connections.
    fn conns_stamp(&self) -> Stamp {
        let Some(dir) = &self.config_dir else { return vec![] };
        db_conn::names(dir).into_iter().map(|n| dir.join("connections").join(format!("{n}.json"))).map(|p| (p.clone(), doc::mtime(&p))).collect()
    }

    /// Surface what `Connections::load` found wrong, the way Respite does for env files.
    fn report_connections(&self, c: &Connections, window: &mut Window, cx: &mut App) {
        for w in &c.warnings {
            notify_later(window, cx, format!("Connections: {w}"));
        }
        for (name, e) in &c.errors {
            notify_later(window, cx, format!("Connection '{name}' skipped: {e}"));
        }
    }

    fn reload_connections(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let c = Arc::new(self.config_dir.as_deref().map(Connections::load).unwrap_or_default());
        self.report_connections(&c, window, cx);
        self.conns = c.clone();
        for d in &mut self.docs {
            d.conns = c.clone();
            d.reparse(cx);
        }
        cx.notify();
    }

    /// Re-read the settings files when they change on disk (edited by hand, another window).
    // ponytail: polls mtimes every second (with the open files), as Respite does; `notify` if this ever shows up in a profile.
    fn start_watch(&self, window: &mut Window, cx: &mut Context<Self>) {
        let stamp = |l: &siesta_settings::Loaded| {
            [&l.global_path, &keymap_path(l)].map(|p| p.as_ref().and_then(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok()))
        };
        let mut last = stamp(&self.settings);
        cx.spawn_in(window, async move |this, cx| {
            loop {
                cx.background_executor().timer(std::time::Duration::from_secs(1)).await;
                let alive = this.update_in(cx, |this, window, cx| {
                    let now = stamp(&this.settings);
                    this.check_files(window, cx);
                    if now == last {
                        return;
                    }
                    last = now;
                    let fresh = siesta_settings::Loaded::load_from(this.settings.global_path.clone());
                    // A half-written or broken file keeps the current settings.
                    if !fresh.errors.is_empty() {
                        for e in &fresh.errors {
                            window.push_notification(format!("Settings: {e}"), cx);
                        }
                        return;
                    }
                    this.settings = fresh;
                    this.apply_settings(window, cx);
                    cx.notify();
                });
                if alive.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    /// Write one setting to the global file and apply everything live.
    fn set(&mut self, key: &str, value: serde_json::Value, window: &mut Window, cx: &mut Context<Self>) {
        if let Err(e) = self.settings.set(key, value) {
            window.push_notification(format!("Settings not saved: {e}"), cx);
        }
        self.apply_settings(window, cx);
        cx.notify();
    }

    fn apply_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        use siesta_settings::Appearance as A;
        let s = self.settings.settings.clone();
        themes::select(&s.theme, cx);
        match s.appearance {
            A::System => Theme::sync_system_appearance(Some(window), cx),
            A::Light => Theme::change(ThemeMode::Light, Some(window), cx),
            A::Dark => Theme::change(ThemeMode::Dark, Some(window), cx),
        }
        themes::current_line(s.editor.highlight_current_line, cx);
        let t = Theme::global_mut(cx);
        t.font_size = px(s.ui_font_size);
        t.mono_font_size = px(s.font_size);
        t.mono_font_family = if s.font_family.is_empty() { self.default_mono.clone() } else { s.font_family.clone().into() };
        for d in &self.docs {
            d.editor.update(cx, |e, cx| {
                e.set_line_number(s.editor.line_numbers, window, cx);
                e.set_soft_wrap(s.editor.soft_wrap, window, cx);
            });
        }
        for e in keymap::apply(s.keymap, keymap_path(&self.settings).as_deref(), cx) {
            notify_later(window, cx, e);
        }
        set_menus(cx); // the menus show the current shortcuts
        cx.refresh_windows();
    }

    /// Switch to `path`'s tab, opening it first if needed (`text` overrides the file content).
    fn open(&mut self, path: PathBuf, text: Option<String>, window: &mut Window, cx: &mut Context<Self>) -> Option<usize> {
        // Opening a file (sidebar, Cmd+P, history …) shows it, so leave the History page.
        self.show_history = false;
        let ix = match self.docs.iter().position(|d| *d.path == path) {
            Some(ix) => ix,
            None => {
                let text = match text.map(Ok).unwrap_or_else(|| std::fs::read_to_string(&path)) {
                    Ok(t) => t,
                    Err(e) => {
                        notify_later(window, cx, format!("{}: {e}", path.display()));
                        return None;
                    }
                };
                let st = &self.settings.settings;
                let (line_numbers, soft_wrap) = (st.editor.line_numbers, st.editor.soft_wrap);
                let sql = siesta_workspace::is_sql(&path);
                let editor = cx.new(|cx| {
                    let mut s = EditorState::new(window, cx);
                    if sql {
                        s = s.language("sql");
                    }
                    s = s.searchable(true).line_number(line_numbers).soft_wrap(soft_wrap).default_value(text);
                    if sql {
                        s.set_highlighter_factory(Rc::new(|_: &str| Some(Box::new(highlight::SqlHighlighter::default()) as _)), cx);
                        s.set_gutter_decorations_width(px(3.), cx);
                    }
                    s
                });
                let subs = vec![
                    cx.subscribe_in(&editor, window, |this, editor, ev: &InputEvent, window, cx| {
                        let Some(ix) = this.docs.iter().position(|d| d.editor == *editor) else { return };
                        if !matches!(ev, InputEvent::Change) {
                            return;
                        }
                        let d = &mut this.docs[ix];
                        let before = d.connection().map(str::to_string);
                        d.dirty = true;
                        d.reparse(cx);
                        // A typed `-- @connection` that names another connection acts like the picker:
                        // the database pick resets and the schema loads.
                        if d.connection() != before.as_deref() {
                            let path = (*d.path).clone();
                            if d.picked_db.take().is_some() {
                                this.db_picks.remove(&path);
                                this.docs[ix].reparse(cx);
                                this.remember(cx);
                            }
                            this.autoload_schema(&path, window, cx);
                        }
                        if let Some(key) = this.docs[ix].schema_key() {
                            this.load_cached_schema(&key);
                        }
                    }),
                    // Cursor moves and scrolling only notify; re-render overlays and the active statement.
                    cx.observe(&editor, |this, editor, cx| {
                        if let Some(d) = this.docs.iter_mut().find(|d| d.editor == editor) {
                            d.sync_active(cx);
                        }
                        cx.notify();
                    }),
                ];
                let picked = self.picks.get(&path).cloned().flatten();
                let picked_db = self.db_picks.get(&path).cloned();
                self.docs.push(Doc::new(path.clone(), editor, self.conns.clone(), picked, picked_db, self.schemas.clone(), subs, cx));
                let ix = self.docs.len() - 1;
                if let Some(key) = self.docs[ix].schema_key() {
                    self.load_cached_schema(&key);
                }
                // A restored pick loads the schema like a fresh one.
                self.autoload_schema(&path, window, cx);
                ix
            }
        };
        self.current = Some(ix);
        self.touch_mru();
        self.expanded.insert(path);
        // Now, not only on quit: closing the window or killing the process skips the quit hook.
        self.remember(cx);
        self.docs[ix].editor.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
        Some(ix)
    }

    fn doc(&self) -> Option<&Doc> {
        self.current.map(|ix| &self.docs[ix])
    }

    /// The current tab goes first in the Ctrl+Tab order (not while Ctrl+Tab is cycling).
    fn touch_mru(&mut self) {
        let Some(path) = self.doc().map(|d| (*d.path).clone()) else { return };
        if self.tab_cycle.is_none() {
            self.mru.retain(|p| *p != path);
            self.mru.insert(0, path);
        }
    }

    /// Ctrl+Tab: the previously used tab; again (Ctrl still held) the one before it, and so on.
    fn next_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let open: Vec<PathBuf> = self.mru.iter().filter(|p| self.docs.iter().any(|d| *d.path == **p)).cloned().collect();
        if open.len() < 2 {
            return;
        }
        let (order, at) = self.tab_cycle.get_or_insert((open, 0));
        *at = (*at + 1) % order.len();
        let path = order[*at].clone();
        if let Some(ix) = self.docs.iter().position(|d| *d.path == path) {
            self.select(ix, window, cx);
        }
    }

    fn select(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if ix < self.docs.len() {
            self.current = Some(ix);
            self.touch_mru();
            self.remember(cx);
            self.docs[ix].editor.update(cx, |s, cx| s.focus(window, cx));
            cx.notify();
        }
    }

    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(ix) = self.current {
            self.save_doc(ix, window, cx);
        }
    }

    /// Ask before quitting with unsaved files: save them all, discard them, or stay.
    /// Cmd+Q, the Quit menu item and the window's close button all come here.
    fn confirm_quit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dirty: Vec<String> = self.docs.iter().filter(|d| d.dirty).map(|d| d.name()).collect();
        if dirty.is_empty() {
            // Nothing unsaved: only open transactions can still stop the quit.
            return self.quit_checked(window, cx);
        }
        let message = match dirty.len() {
            1 => format!("Save changes to {} before quitting?", dirty[0]),
            n => format!("Save changes to {n} files before quitting?"),
        };
        let detail = (dirty.len() > 1).then(|| dirty.join("\n"));
        let answer = window.prompt(PromptLevel::Warning, &message, detail.as_deref(), &["Save All", "Don't Save", "Cancel"], cx);
        cx.spawn_in(window, async move |this, cx| {
            let Ok(answer) = answer.await else { return };
            this.update_in(cx, |this, window, cx| match answer {
                0 => {
                    let all_saved = (0..this.docs.len()).filter(|&ix| this.docs[ix].dirty).collect::<Vec<_>>().into_iter().all(|ix| this.save_doc(ix, window, cx));
                    // A failed save (shown as a notification) keeps the app open.
                    if all_saved {
                        this.quit_checked(window, cx);
                    }
                }
                1 => this.quit_checked(window, cx),
                _ => {}
            })
            .ok();
        })
        .detach();
    }

    /// Write tab `ix` to disk; `false` (with a notification) when that fails.
    fn save_doc(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(d) = self.docs.get_mut(ix) else { return false };
        let text = d.editor.read(cx).value();
        let saved = std::fs::write(&*d.path, text.as_bytes());
        match saved {
            Ok(()) => {
                d.dirty = false;
                d.mtime = doc::mtime(&d.path);
                let path = (*d.path).clone();
                self.outlines.remove(&path);
                // A new file (e.g. the untitled sample) shows up in the tree.
                self.rescan();
                self.remember(cx);
                cx.notify();
                true
            }
            Err(e) => {
                window.push_notification(format!("Save failed: {}: {e}", d.path.display()), cx);
                cx.notify();
                false
            }
        }
    }

    /// Open files, cursors, the front tab and the connection per file (`state.json`); reopened on next launch.
    fn remember(&self, cx: &App) {
        let Some(base) = &self.base else { return };
        let open = self
            .docs
            .iter()
            .filter(|d| d.path.is_file())
            .map(|d| {
                let pos = d.editor.read(cx).cursor_position();
                OpenFile { path: (*d.path).clone(), line: pos.line, column: pos.character }
            })
            .collect();
        let state = WorkspaceState { open, current: self.doc().map(|d| (*d.path).clone()), connections: self.picks.clone(), databases: self.db_picks.clone() };
        siesta_workspace::save_state(base, &self.root, &state);
    }

    /// A dirty tab needs a second close (or a save) — no silent data loss.
    fn close(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let Some(d) = self.docs.get(ix) else { return };
        if d.dirty && self.close_armed.as_deref() != Some(d.path.as_path()) {
            self.close_armed = Some((*d.path).clone());
            let msg = format!("Unsaved changes in {}. Cmd+S saves, closing again discards them.", d.name());
            window.push_notification(msg, cx);
            return;
        }
        let path = (*d.path).clone();
        self.close_armed = None;
        // An open transaction: Commit / Rollback / Cancel first. The unsaved-changes question has
        // been answered by now, so the answer closes the file without asking it again.
        if self.in_tx(&path) {
            let then = {
                let path = path.clone();
                move |this: &mut Self, window: &mut Window, cx: &mut Context<Self>| {
                    if let Some(ix) = this.docs.iter().position(|d| *d.path == path) {
                        this.close_now(ix, window, cx);
                    }
                }
            };
            return self.ask_transaction(path, then, window, cx);
        }
        self.close_now(ix, window, cx);
    }

    /// Closes tab `ix` for good: its sessions, results and outline go with it.
    fn close_now(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        let path = (*self.docs.remove(ix).path).clone();
        self.close_sessions(&path, None);
        self.store.entries.retain(|k, _| k.0 != path);
        self.store.batches.remove(&path);
        if self.ui.pinned.as_ref().is_some_and(|k| k.0 == path) {
            self.ui.pinned = None;
        }
        self.outlines.remove(&path);
        self.current = match self.docs.len() {
            0 => None,
            n => Some(ix.min(n - 1)),
        };
        match self.current {
            Some(ix) => self.select(ix, window, cx),
            // The closed editor may still hold focus; take it, or shortcuts stop working.
            None => {
                self.remember(cx);
                window.focus(&self.focus, cx);
            }
        }
        cx.notify();
    }

    /// Jump to a 1-based `line` of `path` (opening it); `run` also runs the statement there.
    fn go_to(&mut self, path: PathBuf, line: usize, run: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.open(path, None, window, cx) else { return };
        self.docs[ix].editor.update(cx, |s, cx| {
            s.set_cursor_position(Position::new(line.saturating_sub(1) as u32, 0), window, cx);
            s.focus(window, cx);
        });
        // The editor observer syncs the active statement only after this update: a run below needs it now.
        self.docs[ix].sync_active(cx);
        center_line(self.docs[ix].editor.clone(), line.saturating_sub(1), true, window, cx);
        self.remember(cx);
        if run {
            self.run_current(window, cx);
        }
    }

    /// A click on a statement row in the tree: one click jumps, a double click jumps and runs.
    pub(crate) fn click_statement(&mut self, path: PathBuf, line: usize, clicks: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.go_to(path, line, clicks >= 2, window, cx);
    }

    // ---- connection ----

    /// The picker's choice for the current file (`None` = no connection): remembered per file.
    /// With `-- @connection` in the text the stored pick is left alone: the choice rewrites the
    /// directive instead, after a confirmation.
    pub(crate) fn select_connection(&mut self, name: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(d) = self.doc() else { return };
        let Some(old) = d.locked().map(str::to_string) else {
            let path = (*d.path).clone();
            return self.pick_for(&path, name, window, cx);
        };
        if name.as_deref() == Some(old.as_str()) {
            return;
        }
        let prod = name.as_deref().and_then(|n| d.conns.connections.get(n)).is_some_and(|c| c.prod);
        let action = match &name {
            Some(n) => format!("Switch to `{n}`{} and rewrite that line in the file?", if prod { " (PRODUCTION)" } else { "" }),
            None => "Remove that line from the file?".to_string(),
        };
        let detail = format!("This file sets its connection with `-- @connection {old}`. {action}");
        let answer = window.prompt(PromptLevel::Warning, "Change connection in file?", Some(&detail), &["Rewrite", "Cancel"], cx);
        let path = (*d.path).clone();
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            this.update_in(cx, |this, window, cx| this.rewrite_directive(&path, "connection", name.as_deref(), window, cx)).ok();
        })
        .detach();
    }

    /// Replaces (or removes) the `-- @connection` / `-- @database` line (`word`) as an ordinary
    /// undoable edit; the editor's change handling then re-splits the text and the choice follows.
    fn rewrite_directive(&mut self, path: &Path, word: &str, name: Option<&str>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.docs.iter().position(|d| *d.path == path) else { return };
        let d = &self.docs[ix];
        let before = d.connection().map(str::to_string);
        let old = d.editor.read(cx).value().to_string();
        let rewritten = match word {
            "database" => sql_split::rewrite_database(&old, d.dialect, name),
            _ => sql_split::rewrite_connection(&old, d.dialect, name),
        };
        let Some(new) = rewritten else { return };
        self.replace_text(ix, &old, &new, window, cx);
        // Another connection starts from its default database.
        if self.docs[ix].connection().map(str::to_string) != before {
            self.db_picks.remove(path);
            self.docs[ix].picked_db = None;
            self.docs[ix].reparse(cx);
            self.remember(cx);
        }
        cx.notify();
    }

    /// Turns doc `ix`'s text `old` into `new` as one undoable edit of the changed span, then re-splits.
    fn replace_text(&mut self, ix: usize, old: &str, new: &str, window: &mut Window, cx: &mut Context<Self>) {
        let (range, text) = doc::changed(old, new);
        self.docs[ix].editor.update(cx, |s, cx| {
            let rope = s.text().clone();
            let edit = lsp_types::TextEdit { range: lsp_types::Range::new(rope.offset_to_position(range.start), rope.offset_to_position(range.end)), new_text: text };
            s.apply_lsp_edits(&vec![edit], window, cx);
        });
        self.docs[ix].reparse(cx);
    }

    /// Connection `old` was renamed to `new`: stored picks follow silently; files whose
    /// `-- @connection old` line would now name an unknown connection are offered a rewrite
    /// (open tabs as an undoable edit, other workspace files on disk).
    pub(crate) fn rename_references(&mut self, old: &str, new: &str, window: &mut Window, cx: &mut Context<Self>) {
        for pick in self.picks.values_mut().filter(|p| p.as_deref() == Some(old)) {
            *pick = Some(new.to_string());
        }
        for d in self.docs.iter_mut().filter(|d| d.picked.as_deref() == Some(old)) {
            d.picked = Some(new.to_string());
            d.reparse(cx);
        }
        self.remember(cx);
        let names = |f: &Path| f.strip_prefix(self.folder()).unwrap_or(f).display().to_string();
        let open: Vec<PathBuf> = self.docs.iter().filter(|d| d.locked() == Some(old)).map(|d| (*d.path).clone()).collect();
        let closed: Vec<PathBuf> = self
            .files
            .iter()
            .filter(|f| siesta_workspace::is_sql(f) && !self.docs.iter().any(|d| *d.path == **f))
            .filter(|f| std::fs::read_to_string(f).is_ok_and(|t| sql_split::split(&t, Default::default()).connection.as_deref() == Some(old)))
            .cloned()
            .collect();
        if open.is_empty() && closed.is_empty() {
            return;
        }
        let list: Vec<String> = open.iter().chain(&closed).map(|f| names(f)).collect();
        let detail = format!("These files set `-- @connection {old}`:\n{}\n\nRewrite it to `-- @connection {new}`?", list.join("\n"));
        let answer = window.prompt(PromptLevel::Info, "Connection renamed", Some(&detail), &["Rewrite", "Leave"], cx);
        let (old, new) = (old.to_string(), new.to_string());
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            this.update_in(cx, |this, window, cx| {
                for p in open {
                    let Some(ix) = this.docs.iter().position(|d| *d.path == p && d.locked() == Some(old.as_str())) else { continue };
                    let text = this.docs[ix].editor.read(cx).value().to_string();
                    if let Some(t) = sql_split::rewrite_connection(&text, this.docs[ix].dialect, Some(&new)) {
                        this.replace_text(ix, &text, &t, window, cx);
                    }
                }
                let failed: Vec<String> = closed
                    .iter()
                    .filter_map(|f| {
                        let t = std::fs::read_to_string(f).ok()?;
                        (sql_split::split(&t, Default::default()).connection.as_deref() == Some(old.as_str())).then_some(())?;
                        let r =sql_split::rewrite_connection(&t, Default::default(), Some(&new))?;
                        std::fs::write(f, r).err().map(|e| format!("{}: {e}", f.display()))
                    })
                    .collect();
                if !failed.is_empty() {
                    window.push_notification(failed.join("\n"), cx);
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// The database picker's choice for the current file (`None` = the connection's default).
    /// With `-- @database` in the text the choice rewrites the directive instead, after a
    /// confirmation; otherwise it is remembered per file. Either way the file moves to a new
    /// session (an open transaction in the old one is asked about first).
    pub(crate) fn select_database(&mut self, name: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(d) = self.doc().filter(|d| d.sql && d.connection().is_some()) else { return };
        let path = (*d.path).clone();
        let Some(old) = d.locked_db().map(str::to_string) else {
            return self.change_database(path, name, false, window, cx);
        };
        if name.as_deref() == Some(old.as_str()) {
            return;
        }
        let action = match &name {
            Some(n) => format!("Switch to `{n}` and rewrite that line in the file?"),
            None => "Remove that line from the file?".to_string(),
        };
        let detail = format!("This file sets its database with `-- @database {old}`. {action}");
        let answer = window.prompt(PromptLevel::Warning, "Change database in file?", Some(&detail), &["Rewrite", "Cancel"], cx);
        cx.spawn_in(window, async move |this, cx| {
            if answer.await == Ok(0) {
                this.update_in(cx, |this, window, cx| this.change_database(path, name, true, window, cx)).ok();
            }
        })
        .detach();
    }

    /// Moves `path` to database `name` (`rewrite`: through its `-- @database` line, else as a pick).
    /// A transaction open in a session that this leaves behind is asked about first.
    fn change_database(&mut self, path: PathBuf, name: Option<String>, rewrite: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(d) = self.docs.iter().find(|d| *d.path == path) else { return };
        let Some(base) = d.connection().and_then(|n| d.conns.connections.get(n)) else { return };
        let target = if rewrite { name.clone().or(d.picked_db.clone()) } else { name.clone() }.or(base.database.clone());
        if self.in_tx_elsewhere(&path, &base.name, &target) {
            return self.ask_transaction(path.clone(), move |this, window, cx| this.apply_database(path, name, rewrite, window, cx), window, cx);
        }
        self.apply_database(path, name, rewrite, window, cx);
    }

    fn apply_database(&mut self, path: PathBuf, name: Option<String>, rewrite: bool, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.docs.iter().position(|d| *d.path == path) else { return };
        if rewrite {
            self.rewrite_directive(&path, "database", name.as_deref(), window, cx);
        } else {
            match &name {
                Some(n) => self.db_picks.insert(path.clone(), n.clone()),
                None => self.db_picks.remove(&path),
            };
            self.docs[ix].picked_db = name;
            self.docs[ix].reparse(cx);
        }
        let d = &self.docs[ix];
        let (conn, keep, key) = (d.connection().map(str::to_string), d.shown_database(), d.schema_key());
        if let Some(c) = conn {
            self.close_other_database_sessions(&path, &c, &keep);
        }
        if let Some(key) = key {
            self.load_cached_schema(&key);
        }
        self.autoload_schema(&path, window, cx);
        self.remember(cx);
        cx.notify();
    }

    /// Top-right database picker, next to the connection picker: the databases the credentials can
    /// access, read from the server in the background when the picker opens (and kept until Refresh).
    fn database_picker(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let d = self.doc().filter(|d| d.sql)?;
        let forced = d.locked_db().is_some();
        let label = d.shown_database().unwrap_or_else(|| "(default)".into());
        let build = self.db_menu_builder(cx.entity().downgrade())?;
        let view = cx.entity().downgrade();
        let opened = view.clone();
        Some(
            Button::new("database-picker")
                .label(label)
                .dropdown_caret(true)
                .small()
                .ghost()
                .when(forced, |b| b.tooltip("Set by `-- @database` in this file"))
                .dropdown_menu(move |menu, window, cx| build(menu, window, cx))
                .on_open_change(move |open, window, cx| {
                    if *open {
                        opened.update(cx, |this, cx| this.fetch_databases(false, window, cx)).ok();
                    }
                })
                .into_any_element(),
        )
    }

    /// The database list rows of the current file's connection, as the picker shows them.
    #[cfg(test)]
    fn db_rows(&self) -> Option<Vec<DbRow>> {
        let d = self.doc().filter(|d| d.sql)?;
        let conn = d.conns.connections.get(d.connection()?)?;
        Some(db_rows(self.db_lists.get(&schema_cache::key(conn))))
    }

    /// Builds the picker menu from the current state. It also remembers the menu entity it fills,
    /// so a list that arrives while the menu is open can `rebuild` it (see `fetch_databases_of`).
    pub(crate) fn db_menu_builder(&self, view: WeakEntity<Self>) -> Option<DbMenuBuild> {
        let d = self.doc().filter(|d| d.sql)?;
        let conn = d.conns.connections.get(d.connection()?)?;
        let explicit = d.database().map(str::to_string);
        let default_label = format!("Default ({})", conn.database.as_deref().unwrap_or("server default"));
        let mysql = conn.driver == db_conn::Driver::MySql;
        let rows = db_rows(self.db_lists.get(&schema_cache::key(conn)));
        let slot = self.db_menu.clone();
        Some(Rc::new(move |menu, _, cx| {
            *slot.borrow_mut() = Some(cx.entity().downgrade());
            let pick = |name: Option<String>| {
                let view = view.clone();
                move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                    view.update(cx, |this, cx| this.select_database(name.clone(), window, cx)).ok();
                }
            };
            let again = |name: &'static str| {
                let view = view.clone();
                PopupMenuItem::new(name).on_click(move |_, window, cx| _ = view.update(cx, |this, cx| this.refresh_databases(window, cx)))
            };
            let mut menu = menu.item(PopupMenuItem::new(default_label.clone()).checked(explicit.is_none()).on_click(pick(None))).separator();
            for row in &rows {
                menu = match row {
                    DbRow::Loading => menu.item(PopupMenuItem::new("Loading…").disabled(true)),
                    DbRow::Error(t) => menu.item(PopupMenuItem::new(t.clone()).disabled(true)),
                    DbRow::Database(n) => {
                        let muted = mysql && db_exec::is_system_database(n);
                        let text = n.clone();
                        let item = PopupMenuItem::element(move |_, cx| {
                            let c = if muted { cx.theme().muted_foreground } else { cx.theme().foreground };
                            div().text_color(c).child(text.clone())
                        });
                        menu.item(item.checked(explicit.as_deref() == Some(n.as_str())).on_click(pick(Some(n.clone()))))
                    }
                    DbRow::Retry => menu.separator().item(again("Retry")),
                    DbRow::Refresh => menu.separator().item(again("Refresh")),
                };
            }
            menu
        }))
    }

    /// Remembers `name` as the pick of `path` (only that file) and loads its schema in the background.
    pub(crate) fn pick_for(&mut self, path: &Path, name: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.docs.iter().position(|d| *d.path == path) else { return };
        // The database belongs to a connection: another connection starts from its default.
        if self.docs[ix].picked != name {
            self.db_picks.remove(path);
            self.docs[ix].picked_db = None;
        }
        self.picks.insert(path.to_path_buf(), name.clone());
        self.docs[ix].picked = name.clone();
        self.docs[ix].reparse(cx);
        if let Some(key) = self.docs[ix].schema_key() {
            self.load_cached_schema(&key);
        }
        if name.is_some() {
            self.autoload_schema(path, window, cx);
        }
        self.remember(cx);
        cx.notify();
    }

    /// Above the editor: a red "Production" banner for a `prod` connection; a read-only one alone
    /// only gets a neutral badge.
    fn banner(&self, cx: &App) -> Option<AnyElement> {
        let d = self.doc()?;
        let name = d.connection()?;
        let c = d.conns.connections.get(name)?;
        let t = cx.theme();
        let bar = || div().flex_none().px_3().py_1().text_sm().font_weight(FontWeight::SEMIBOLD);
        Some(match (c.prod, c.read_only) {
            (false, false) => return None,
            (true, ro) => bar().bg(t.danger).text_color(t.danger_foreground).child(format!("Production: {name}{}", if ro { " (read only)" } else { "" })).into_any_element(),
            (false, true) => bar().bg(t.secondary).text_color(t.muted_foreground).child(format!("Read only: {name}")).into_any_element(),
        })
    }

    /// Top-right connection picker: the connections, "No connection", and the form / page entries. The dot is grey until sessions exist (green / red later).
    fn connection_picker(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let d = self.doc().filter(|d| d.sql)?;
        let current = d.connection().map(str::to_string);
        let forced = d.locked().is_some();
        // A directive naming a connection that does not exist: shown in red.
        let unknown = d.locked().filter(|n| !d.conns.connections.contains_key(*n)).map(str::to_string);
        let names: Vec<String> = d.conns.names().iter().map(|n| n.to_string()).collect();
        // Nothing to pick yet: the picker becomes the way to make the first connection.
        if names.is_empty() && !forced {
            return Some(
                Button::new("connection-add")
                    .icon(IconName::Plus)
                    .label("Add connection")
                    .small()
                    .primary()
                    .tooltip("No connections yet: create one to run queries")
                    .on_click(cx.listener(|this, _, window, cx| this.open_conn_form(None, window, cx)))
                    .into_any_element(),
            );
        }
        let conn_of = |n: &str| d.conns.connections.get(n);
        let prod = current.as_deref().and_then(conn_of).is_some_and(|c| c.prod);
        // Each connection's colour (its own, or red for prod) marks it in the picker.
        let colors: std::collections::HashMap<String, Hsla> = names.iter().filter_map(|n| Some((n.clone(), format::parse_color(conn_of(n)?.display_color()?)?))).collect();
        let strip = current.as_ref().and_then(|n| colors.get(n).copied());
        let status = self.dot_of(&d.path, current.as_deref(), d.shown_database());
        let (can_test, can_disconnect) = (current.is_some(), self.slots.keys().any(|(p, ..)| *p == *d.path));
        let t = cx.theme();
        let (muted, fs) = (t.muted_foreground, t.font_size);
        let status_color = match status {
            runner::Dot::Grey => muted.opacity(0.6),
            runner::Dot::Green => t.success,
            runner::Dot::Red => t.danger,
        };
        let dot = |color: Hsla| div().flex_none().size(fs * 0.55).rounded_full().bg(color);
        let label = current.clone().unwrap_or_else(|| "No connection".into());
        let view = cx.entity().downgrade();
        Some(
            Button::new("connection-picker")
                .icon(IconName::Network)
                .child(h_flex().gap_1p5().children(strip.map(|c| div().flex_none().w(px(3.)).h(fs).rounded_full().bg(c))).child(dot(status_color)).child(label))
                .dropdown_caret(true)
                .small()
                .when(prod || unknown.is_some(), |b| b.danger())
                .when(!(prod || unknown.is_some()), |b| b.ghost())
                .when(forced, |b| {
                    b.tooltip(match &unknown {
                        Some(n) => format!("Unknown connection `{n}` (from `-- @connection`)"),
                        None => "Set by `-- @connection` in this file".to_string(),
                    })
                })
                .dropdown_menu(move |menu, _, cx| {
                    let pick = |name: Option<String>| {
                        let view = view.clone();
                        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                            view.update(cx, |this, cx| this.select_connection(name.clone(), window, cx)).ok();
                        }
                    };
                    let row = |label: String, name: Option<String>| {
                        let color = name.as_ref().and_then(|n| colors.get(n).copied()).unwrap_or(cx.theme().muted_foreground.opacity(0.6));
                        let size = cx.theme().font_size * 0.55;
                        // A pencil on each connection edits it without picking it: the pencil's click
                        // runs first and marks it, then the row's own click (which also closes the
                        // menu) opens the form instead of picking.
                        let edit_clicked = std::rc::Rc::new(std::cell::Cell::new(false));
                        let pencil_name = name.clone();
                        let flag = edit_clicked.clone();
                        let checked = current == name;
                        let on_pick = pick(name.clone());
                        let view = view.clone();
                        PopupMenuItem::element(move |_, _| {
                            let edit = pencil_name.as_ref().map(|n| {
                                let flag = flag.clone();
                                Button::new(SharedString::from(format!("edit-conn-{n}")))
                                    .icon(Icon::default().data(include_bytes!("../assets/pencil.svg")))
                                    .ghost()
                                    .xsmall()
                                    .tooltip("Edit connection")
                                    .on_click(move |_, _, _| flag.set(true))
                            });
                            h_flex()
                                .w_full()
                                .gap_2()
                                .items_center()
                                .child(div().flex_none().size(size).rounded_full().bg(color))
                                .child(div().flex_1().child(label.clone()))
                                .children(edit)
                        })
                        .checked(checked)
                        .on_click(move |ev, window, cx| match (edit_clicked.replace(false), name.clone()) {
                            (true, Some(n)) => _ = view.update(cx, |this, cx| this.open_conn_form(Some(n), window, cx)),
                            _ => on_pick(ev, window, cx),
                        })
                    };
                    let mut menu = menu.item(row("No connection".into(), None)).separator();
                    if names.is_empty() {
                        menu = menu.item(PopupMenuItem::new("New connection…").on_click({
                            let view = view.clone();
                            move |_, window, cx| _ = view.update(cx, |this, cx| this.open_conn_form(None, window, cx))
                        }));
                    }
                    menu = names.iter().fold(menu, |menu, n| menu.item(row(n.clone(), Some(n.clone()))));
                    if forced {
                        menu = menu.separator().label("Set by `-- @connection` in this file");
                    }
                    // Connections are made in a form (edited with the pencil on each row); Test needs a connection,
                    // Disconnect a live session.
                    let new = {
                        let view = view.clone();
                        PopupMenuItem::new("New connection…").on_click(move |_, window, cx| _ = view.update(cx, |this, cx| this.open_conn_form(None, window, cx)))
                    };
                    menu = menu.separator().item(new).item(
                        PopupMenuItem::new("Manage connections…").on_click({
                            let view = view.clone();
                            move |_, window, cx| _ = view.update(cx, |this, cx| this.open_connections(window, cx))
                        }),
                    );
                    let hook = |label: &'static str, enabled: bool, run: fn(&mut Siesta, &mut Window, &mut Context<Siesta>)| {
                        let view = view.clone();
                        PopupMenuItem::new(label).disabled(!enabled).on_click(move |_, window, cx| {
                            view.update(cx, |this, cx| run(this, window, cx)).ok();
                        })
                    };
                    menu = menu
                        .separator()
                        .item(hook("Test connection", can_test, Siesta::test_connection))
                        .item(hook("Disconnect", can_disconnect, Siesta::disconnect));
                    menu
                })
                .into_any_element(),
        )
    }

    /// Open files as rounded buttons on the window background; the current one is raised with a
    /// fill and a hairline border. The close button shows on the current or hovered tab, the
    /// unsaved dot always.
    fn tabs(&self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme();
        let (muted, fg, hover_bg, raised, border) = (t.muted_foreground, t.foreground, t.list_hover, t.secondary, t.border);
        // Sized from the interface font, so the tabs keep up with the label.
        let fs = t.font_size;
        let tabs = self.docs.iter().enumerate().map(|(i, d)| {
            let current = self.current == Some(i);
            let group: SharedString = format!("doc-tab-{i}").into();
            // An icon, not a `×` glyph: the glyph sits small and low next to the label.
            let close = div()
                .id(("close", i))
                .size(fs * 1.4)
                .flex()
                .items_center()
                .justify_center()
                .rounded_sm()
                .text_color(muted)
                .hover(move |s| s.text_color(fg).bg(hover_bg))
                .map(|el| match d.dirty {
                    true => el.child(div().size(fs * 0.5).rounded_full().bg(muted)),
                    false => el.child(Icon::new(IconName::Close).size(fs)),
                })
                .when(!current && !d.dirty, |el| el.opacity(0.).group_hover(group.clone(), |s| s.opacity(1.)))
                .on_click(cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.close(i, window, cx);
                }));
            div()
                .id(("doc-tab", i))
                .group(group)
                .flex_none()
                .h(fs * 2.3)
                .pl_3()
                .pr_1()
                .flex()
                .items_center()
                .gap_1()
                .rounded_lg()
                .border_1()
                .map(|el| match current {
                    true => el.bg(raised).border_color(border).text_color(fg),
                    false => el.border_color(gpui::transparent_black()).text_color(muted).hover(move |s| s.bg(hover_bg).text_color(fg)),
                })
                .on_click(cx.listener(move |this, _, window, cx| this.select(i, window, cx)))
                .child(d.name())
                .child(close)
        });
        // ponytail: no scroll-into-view for the current tab; add when many open files hide it.
        div()
            .flex_none()
            .flex()
            .items_center()
            .gap_2()
            .px_1()
            .pb_1()
            .child(div().id("docs").flex_1().min_w_0().flex().gap_1().overflow_x_scroll().children(tabs))
            .children(self.connection_picker(cx))
            .children(self.database_picker(cx))
            .into_any_element()
    }

    fn center(&mut self, focused: bool, cx: &mut Context<Self>) -> AnyElement {
        if self.show_history {
            let page = self.history_page(cx);
            return card(focused, cx).child(page).into_any_element();
        }
        let body = match self.doc() {
            Some(d) => div().flex_1().min_h_0().relative().child(Editor::new(&d.editor).size_full()).child(d.statement_tint()),
            None => div()
                .flex_1()
                .flex()
                .items_center()
                .justify_center()
                .text_color(cx.theme().muted_foreground)
                .child("Open a file from the sidebar"),
        };
        let tabs = (!self.docs.is_empty()).then(|| self.tabs(cx));
        let crumbs = self.breadcrumbs(cx);
        let card = card(focused, cx).children(self.banner(cx)).children(crumbs).child(body);
        div().size_full().flex().flex_col().children(tabs).child(card).into_any_element()
    }

}

/// A panel on the window background: rounded, hairline border, ring colour while it has focus.
fn card(focused: bool, cx: &App) -> Div {
    let t = cx.theme();
    div()
        .flex_1()
        .min_h_0()
        .size_full()
        .flex()
        .flex_col()
        .overflow_hidden()
        .rounded_lg()
        .border_1()
        .border_color(if focused { t.ring } else { t.border })
        .bg(t.background)
}

/// Panel groups without a painted divider: the gap between cards is the drag area.
fn invisible_dividers(group: gpui_component::resizable::ResizablePanelGroup) -> gpui_component::resizable::ResizablePanelGroup {
    group.with_handle_appearance(std::rc::Rc::new(|_: &gpui_base::ResizeHandleContext, _: &mut Window, _: &mut App| {
        Some(div().into_any_element())
    }))
}

/// Scroll so 0-based `line` sits mid-viewport, unless it's already well inside it (the editor
/// alone only scrolls it into view, onto the bottom row). A just-opened editor has no layout
/// yet: `retry` waits one frame for it.
fn center_line(editor: Entity<EditorState>, line: usize, retry: bool, window: &mut Window, cx: &mut App) {
    let done = editor.update(cx, |s, cx| {
        let (Some(lh), Some(bounds)) = (s.line_height(), s.text_bounds()) else { return false };
        let row = s.display_map().buffer_line_to_display_row(line);
        if s.visible_row_range().is_some_and(|r| r.start + 2 < row && row + 3 < r.end) {
            return true;
        }
        let y = (bounds.size.height - lh) / 2. - lh * row as f32;
        s.set_scroll_offset(point(px(0.), y.min(px(0.))), cx);
        true
    });
    if !done && retry {
        window.on_next_frame(move |window, cx| center_line(editor, line, false, window, cx));
    }
}

impl Siesta {
    /// Editor and result panel side by side or stacked (`split`, Cmd+\), sized by `split_ratio`;
    /// dragging the divider saves the new ratio. Cmd+J hides the result panel.
    fn editor_and_result(&mut self, center: AnyElement, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let s = &self.settings.settings;
        let vertical = s.split == siesta_settings::Split::Vertical;
        let ratio = s.split_ratio.clamp(0.2, 0.8);
        // Initial sizes only: the group keeps what the user drags. A separate id per
        // orientation gives a switch fresh sizes.
        let vp = window.viewport_size();
        let total = if vertical { vp.height - px(40.) } else { vp.width - if self.sidebar { px(260.) } else { px(0.) } };
        let group = invisible_dividers(if vertical { v_resizable("split-v") } else { h_resizable("split-h") });
        let (view, open) = (cx.entity().downgrade(), self.result_open);
        // Half the gap on each side of a divider; the outer edge gets the root's padding.
        let result = div().size_full().p_1().child(card(false, cx).child(self.result_panel(window, cx)));
        group
            .on_resize(move |state, window, cx| {
                let sizes = state.read(cx).sizes().clone();
                if !open || sizes.len() != 2 || sizes[0] + sizes[1] <= px(0.) {
                    return;
                }
                let r = ((sizes[0] / (sizes[0] + sizes[1])) * 100.).round() / 100.;
                view.update(cx, |this, cx| this.set("split_ratio", serde_json::json!(r), window, cx)).ok();
            })
            .child(resizable_panel().size(total * ratio).child(div().size_full().p_1().child(center)))
            .child(resizable_panel().size(total * (1. - ratio)).visible(self.result_open).child(result))
            .into_any_element()
    }
}

impl Render for Siesta {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if window.focused(cx).is_none() {
            window.focus(&self.focus, cx);
        }
        let sidebar = div().size_full().p_1().child(card(false, cx).child(self.sidebar(cx)));
        let editor_focused = self.doc().is_some_and(|d| d.editor.focus_handle(cx).contains_focused(window, cx));
        let center = self.center(editor_focused, cx);
        div()
            .key_context("Siesta")
            .track_focus(&self.focus)
            .size_full()
            .p_1()
            // The sidebar, the editor and the result float on it as cards.
            .bg(cx.theme().sidebar)
            .text_color(cx.theme().foreground)
            .on_action(cx.listener(|this, _: &RunStatement, window, cx| this.run_current(window, cx)))
            .on_action(cx.listener(|this, _: &RunAll, window, cx| this.run_all(window, cx)))
            .on_action(cx.listener(|this, _: &RepeatLast, window, cx| this.repeat_last(window, cx)))
            .on_action(cx.listener(|this, _: &CancelRun, window, cx| this.cancel_run(window, cx)))
            .on_action(cx.listener(|this, _: &ToggleFilters, window, cx| this.toggle_filters(window, cx)))
            .on_action(cx.listener(|this, _: &RefreshSchema, window, cx| this.refresh_schema(window, cx)))
            // Esc reaches here only when the editor had nothing of its own to close.
            // ponytail: Esc that closes the completion menu also gets here (the menu propagates it).
            .on_action(cx.listener(|this, _: &gpui_component::input::Escape, window, cx| this.cancel_run(window, cx)))
            .on_action(cx.listener(|this, _: &FocusEditor, window, cx| {
                if let Some(d) = this.doc() {
                    d.editor.update(cx, |s, cx| s.focus(window, cx));
                }
            }))
            // Ctrl+Tab cycles in most-recently-used order while Ctrl is held; letting go commits.
            .on_modifiers_changed(cx.listener(|this, ev: &ModifiersChangedEvent, _, _| {
                if this.tab_cycle.is_some() && !ev.modifiers.modified() {
                    this.tab_cycle = None;
                    this.touch_mru();
                }
            }))
            .on_action(cx.listener(|this, _: &GoToFile, window, cx| this.pick_file(window, cx)))
            .on_action(cx.listener(|this, _: &GoToStatement, window, cx| this.pick_statement(window, cx)))
            .on_action(cx.listener(|this, _: &GoToWorkspaceStatement, window, cx| this.pick_workspace_statement(window, cx)))
            .on_action(cx.listener(|this, _: &PickConnection, window, cx| this.pick_connection(window, cx)))
            .on_action(cx.listener(|this, _: &CommandPalette, window, cx| this.pick_command(commands(), window, cx)))
            .on_action(cx.listener(|this, _: &PrevStatement, window, cx| this.step_statement(false, window, cx)))
            .on_action(cx.listener(|this, _: &NextStatement, window, cx| this.step_statement(true, window, cx)))
            .on_action(cx.listener(|this, _: &SaveFile, window, cx| this.save(window, cx)))
            .on_action(cx.listener(|this, _: &CloseTab, window, cx| {
                if let Some(ix) = this.current {
                    this.close(ix, window, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &OpenFolder, window, cx| this.open_folder(window, cx)))
            .on_action(cx.listener(|this, _: &OpenSingleFile, window, cx| this.open_single_file(window, cx)))
            .on_action(cx.listener(|this, _: &OpenSettings, window, cx| this.open_settings(window, cx)))
            .on_action(cx.listener(|this, _: &OpenConnections, window, cx| this.open_connections(window, cx)))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| {
                this.sidebar = !this.sidebar;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &NextTab, window, cx| this.next_tab(window, cx)))
            .on_action(cx.listener(|this, _: &ToggleResult, _, cx| {
                this.result_open = !this.result_open;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleHistory, window, cx| this.toggle_history(window, cx)))
            .on_action(cx.listener(|this, _: &ToggleSplit, window, cx| {
                use siesta_settings::Split;
                let next = if this.settings.settings.split == Split::Vertical { Split::Horizontal } else { Split::Vertical };
                this.set("split", serde_json::json!(next), window, cx);
            }))
            .child(
                invisible_dividers(h_resizable("main"))
                    .child(resizable_panel().size(px(260.)).size_range(px(160.)..px(520.)).visible(self.sidebar).child(sidebar))
                    .child(resizable_panel().child(self.editor_and_result(center, window, cx))),
            )
    }
}

/// macOS menu bar (Linux has none); shortcuts shown come from the keymap.
fn set_menus(cx: &mut App) {
    use gpui_component::input::{Copy, Cut, Paste, Redo, Search, SelectAll, Undo};
    cx.set_menus([
        Menu::new("Siesta").items([
            MenuItem::action("Settings…", OpenSettings),
            MenuItem::action("Connections…", OpenConnections),
            MenuItem::separator(),
            MenuItem::os_submenu("Services", SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action("Quit Siesta", Quit),
        ]),
        Menu::new("File").items([
            MenuItem::action("Open Folder…", OpenFolder),
            MenuItem::action("Open File…", OpenSingleFile),
            MenuItem::separator(),
            MenuItem::action("Save", SaveFile),
            MenuItem::action("Close Tab", CloseTab),
        ]),
        Menu::new("Edit").items([
            MenuItem::os_action("Undo", Undo, OsAction::Undo),
            MenuItem::os_action("Redo", Redo, OsAction::Redo),
            MenuItem::separator(),
            MenuItem::os_action("Cut", Cut, OsAction::Cut),
            MenuItem::os_action("Copy", Copy, OsAction::Copy),
            MenuItem::os_action("Paste", Paste, OsAction::Paste),
            MenuItem::os_action("Select All", SelectAll, OsAction::SelectAll),
            MenuItem::separator(),
            MenuItem::action("Find", Search),
        ]),
        Menu::new("Run").items([
            MenuItem::action("Run Statement", RunStatement),
            MenuItem::action("Run All in File", RunAll),
            MenuItem::action("Repeat Last", RepeatLast),
            MenuItem::action("Cancel Running", CancelRun),
            MenuItem::action("Toggle Column Filters", ToggleFilters),
            MenuItem::separator(),
            MenuItem::action("Pick Connection…", PickConnection),
            MenuItem::action("Refresh Schema", RefreshSchema),
        ]),
        Menu::new("View").items([
            MenuItem::action("Toggle Sidebar", ToggleSidebar),
            MenuItem::action("Toggle Result Panel", ToggleResult),
            MenuItem::action("History", ToggleHistory),
            MenuItem::action("Toggle Split Orientation", ToggleSplit),
            MenuItem::action("Next Tab", NextTab),
            MenuItem::separator(),
            MenuItem::action("Focus Editor", FocusEditor),
        ]),
        Menu::new("Go").items([
            MenuItem::action("Go to File…", GoToFile),
            MenuItem::action("Go to Statement…", GoToStatement),
            MenuItem::action("Go to Statement in Workspace…", GoToWorkspaceStatement),
            MenuItem::action("Previous Statement", PrevStatement),
            MenuItem::action("Next Statement", NextStatement),
            MenuItem::separator(),
            MenuItem::action("Command Palette…", CommandPalette),
        ]),
    ]);
}

/// What Cmd+Shift+P offers: the menu commands, by their menu names.
fn commands() -> Vec<(&'static str, Box<dyn Action>)> {
    vec![
        ("Run Statement", Box::new(RunStatement)),
        ("Run All in File", Box::new(RunAll)),
        ("Repeat Last", Box::new(RepeatLast)),
        ("Cancel Running", Box::new(CancelRun)),
        ("Toggle Column Filters", Box::new(ToggleFilters)),
        ("Pick Connection…", Box::new(PickConnection)),
        ("Refresh Schema", Box::new(RefreshSchema)),
        ("Go to File…", Box::new(GoToFile)),
        ("Go to Statement…", Box::new(GoToStatement)),
        ("Go to Statement in Workspace…", Box::new(GoToWorkspaceStatement)),
        ("Previous Statement", Box::new(PrevStatement)),
        ("Next Statement", Box::new(NextStatement)),
        ("Focus Editor", Box::new(FocusEditor)),
        ("Toggle Result Panel", Box::new(ToggleResult)),
        ("History", Box::new(ToggleHistory)),
        ("Toggle Split Orientation", Box::new(ToggleSplit)),
        ("Toggle Sidebar", Box::new(ToggleSidebar)),
        ("Next Tab", Box::new(NextTab)),
        ("Save", Box::new(SaveFile)),
        ("Close Tab", Box::new(CloseTab)),
        ("Open Folder…", Box::new(OpenFolder)),
        ("Open File…", Box::new(OpenSingleFile)),
        ("Settings…", Box::new(OpenSettings)),
        ("Connections", Box::new(OpenConnections)),
        ("Quit Siesta", Box::new(Quit)),
    ]
}

/// `siesta [path[:line]]`: a folder opens as the workspace, a file opens in its repo's workspace;
/// no argument reopens the last workspace.
fn main() -> std::process::ExitCode {
    let (config, base) = (siesta_settings::config_dir(), siesta_settings::data_dir());
    // Older versions kept the password key among the user's files; it belongs to the data dir.
    if let (Some(c), Some(d)) = (&config, &base)
        && let Err(e) = db_conn::crypto::adopt_old_key(c, d)
    {
        eprintln!("siesta: password key not moved: {e}");
    }
    if let Some(code) = siesta_cli::main_if_cli() {
        return code;
    }
    // ponytail: hand-rolled GUI args; fold into clap when the GUI grows options.
    // `-psn_…` is a process serial number some macOS launches still pass to app bundles.
    let target = std::env::args().skip(1).rfind(|a| a != "open" && !a.starts_with("-psn_"));
    let cwd = std::env::current_dir().expect("current dir");
    let (root, open) = match target.map(|t| parse_target(&t)).map(|(p, l)| (std::path::absolute(&p).unwrap_or_else(|_| cwd.join(&p)), l)) {
        Some((p, _)) if p.is_dir() => (p, None),
        // A file opens alone (a single-file workspace), not its repository.
        Some((p, line)) => (p.clone(), Some((p, line))),
        // No target: the last workspace, else the user's scratch folder `~/.config/siesta/sql`
        // (cwd is `/` from Finder, `target/…` from cargo). It holds the user's own .sql files, so
        // it lives with the config, not in the data dir.
        None => {
            let root = base.as_deref().and_then(siesta_workspace::last_root).or_else(|| config.as_ref().map(|c| c.join("sql")));
            let root = root.unwrap_or(cwd);
            if let Err(e) = std::fs::create_dir_all(&root) {
                eprintln!("siesta: {}: {e}", root.display());
            }
            (root, None)
        }
    };
    gpui_platform::application().with_assets(gpui_kit_assets::Assets).run(move |cx: &mut App| {
        gpui_component::init(cx); // theme and fonts come from settings (Siesta::apply_settings)
        themes::register(cx);
        // Shortcuts themselves are bound by `Siesta::apply_settings` (they depend on settings).
        keymap::init(cx);
        let options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(1440.), px(880.)), cx)),
            titlebar: Some(TitlebarOptions { title: Some("Siesta".into()), ..Default::default() }),
            ..Default::default()
        };
        let mut view = None;
        let handle = cx
            .open_window(options, |window, cx| {
                let settings = siesta_settings::Loaded::load();
                let v = cx.new(|cx| Siesta::new(root, open, base, settings, config, window, cx));
                v.update(cx, |this, cx| this.start_watch(window, cx));
                view = Some(v.downgrade());
                cx.new(|cx| Root::new(v, window, cx))
            })
            .unwrap();
        let main = handle.window_id();
        // Quit (Cmd+Q, menu) asks about unsaved files in the main window, from any window.
        let view = view.expect("main view");
        cx.on_action(move |_: &Quit, cx| {
            let view = view.clone();
            let asked = handle.update(cx, |_, window, cx| view.update(cx, |this, cx| this.confirm_quit(window, cx)));
            if !matches!(asked, Ok(Ok(()))) {
                cx.quit();
            }
        });
        // Closing the settings window keeps the app running.
        cx.on_window_closed(move |cx, id| {
            if id == main {
                cx.quit();
            }
        })
        .detach();
        cx.activate(true);
    });
    std::process::ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    // Not `super::*`: it brings `gpui::test`, which shadows `#[test]`.
    use super::{Siesta, parse_target};
    use gpui::{AppContext as _, Entity, TestAppContext, VisualTestContext};
    use gpui_component::Root;
    use std::path::{Path, PathBuf};

    #[test]
    fn target_with_line() {
        assert_eq!(parse_target("/no/such/q.sql:12"), (PathBuf::from("/no/such/q.sql"), Some(12)));
        assert_eq!(parse_target("/no/such/q.sql"), (PathBuf::from("/no/such/q.sql"), None));
        assert_eq!(parse_target("/no/such/q.sql:x"), (PathBuf::from("/no/such/q.sql:x"), None));
    }

    /// The config dir inside a test workspace (hidden, so the file tree skips it).
    const CONFIG: &str = ".cfg";

    /// A workspace with three connections in its config dir (`dev` Postgres, `my` MySQL, `live` prod) and a few files.
    fn workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        // Nothing listens on port 1: a picked connection tries to load its schema and fails at once.
        std::fs::create_dir_all(r.join(CONFIG).join("connections")).unwrap();
        for (name, json) in [
            ("dev", r#"{"driver": "postgres", "url": "postgres://nobody@127.0.0.1:1/d"}"#),
            ("my", r#"{"driver": "mysql", "read_only": true, "url": "mysql://nobody@127.0.0.1:1/d"}"#),
            ("live", r#"{"driver": "postgres", "prod": true, "url": "postgres://nobody@127.0.0.1:1/p"}"#),
        ] {
            std::fs::write(r.join(CONFIG).join(format!("connections/{name}.json")), json).unwrap();
        }
        std::fs::write(r.join("a.sql"), "SELECT 1;\n\nSELECT 2;\n\n-- @name third\nSELECT 3;\n").unwrap();
        std::fs::write(r.join("b.sql"), "SELECT 'b';\n").unwrap();
        std::fs::write(r.join("c.sql"), "-- @connection my\nSELECT 'c';\n").unwrap();
        dir
    }

    fn open_app(cx: &mut TestAppContext, root: &Path, base: &Path, open: Option<PathBuf>) -> (Entity<Siesta>, VisualTestContext) {
        if !cx.update(|cx| cx.has_global::<gpui_component::Theme>()) {
            cx.update(gpui_component::init);
        }
        // A picked connection loads its schema on the tokio runtime (nothing listens on port 1, so
        // it fails at once); its result wakes GPUI from the tokio thread, which the deterministic
        // scheduler would otherwise report as an error at a random point.
        cx.executor().allow_parking();
        let mut view = None;
        let window = cx.update(|cx| {
            cx.open_window(Default::default(), |window, cx| {
                let settings = siesta_settings::Loaded::load_from(None);
                let v = cx.new(|cx| Siesta::new(root.to_path_buf(), open.map(|p| (p, None)), Some(base.to_path_buf()), settings, Some(root.join(CONFIG)), window, cx));
                view = Some(v.clone());
                cx.new(|cx| Root::new(v, window, cx))
            })
            .unwrap()
        });
        (view.unwrap(), VisualTestContext::from_window(window.into(), cx))
    }

    fn cursor_line(app: &Entity<Siesta>, cx: &mut VisualTestContext) -> u32 {
        cx.update(|_, cx| app.read(cx).doc().unwrap().editor.read(cx).cursor_position().line)
    }

    #[gpui::test]
    fn active_statement_follows_the_cursor(cx: &mut TestAppContext) {
        let (ws, base) = (workspace(), tempfile::tempdir().unwrap());
        let (app, mut cx) = open_app(cx, ws.path(), base.path(), Some(ws.path().join("a.sql")));
        let active = |app: &Entity<Siesta>, cx: &mut VisualTestContext| {
            cx.update(|_, cx| app.read(cx).doc().unwrap().active_statement().map(|s| s.line))
        };
        assert_eq!(active(&app, &mut cx), Some(1));
        cx.update(|window, cx| {
            app.update(cx, |this, cx| this.docs[this.current.unwrap()].editor.update(cx, |s, cx| s.set_cursor_position(gpui_component::input::Position::new(5, 3), window, cx)))
        });
        cx.run_until_parked();
        assert_eq!(active(&app, &mut cx), Some(6)); // the `@name third` statement starts on line 6
        // Between statements (a blank line) the previous one stays active.
        cx.update(|window, cx| {
            app.update(cx, |this, cx| this.docs[this.current.unwrap()].editor.update(cx, |s, cx| s.set_cursor_position(gpui_component::input::Position::new(1, 0), window, cx)))
        });
        cx.run_until_parked();
        assert_eq!(active(&app, &mut cx), Some(1));
    }

    fn done(text: &str, cols: &[(&str, &str)], rows: Vec<Vec<db_exec::Value>>, has_more: bool) -> crate::results::Done {
        crate::results::Done {
            text: text.into(),
            kind: sql_split::Kind::Query,
            page: db_exec::Page::default(),
            range: None,
            conn: "dev".into(),
            database: None,
            color: None,
            driver: db_conn::Driver::Postgres,
            result: Ok(db_exec::Outcome {
                columns: cols.iter().map(|(n, t)| db_exec::Column { name: n.to_string(), type_name: t.to_string() }).collect(),
                rows,
                wrapped: true,
                has_more,
                truncated: false,
                rows_affected: None,
                notices: vec![],
                executed_sql: text.into(),
                params: vec![],
                in_transaction: false,
                duration: std::time::Duration::from_millis(5),
            }),
        }
    }

    /// Cursor to (0-based) `line`, then let the panel load what it shows; the table's statement.
    fn shown_at(app: &Entity<Siesta>, cx: &mut VisualTestContext, line: u32) -> Option<String> {
        cx.update(|window, cx| {
            app.update(cx, |this, cx| {
                this.docs[this.current.unwrap()].editor.update(cx, |s, cx| s.set_cursor_position(gpui_component::input::Position::new(line, 0), window, cx));
            })
        });
        cx.run_until_parked();
        cx.update(|window, cx| {
            app.update(cx, |this, cx| {
                this.sync_results(window, cx);
                this.ui.table.read(cx).delegate().done.as_ref().map(|d| d.text.clone())
            })
        })
    }

    #[gpui::test]
    fn result_follows_the_cursor(cx: &mut TestAppContext) {
        use db_exec::Value::Int;
        let (ws, base) = (workspace(), tempfile::tempdir().unwrap());
        let a = ws.path().join("a.sql");
        let (app, mut cx) = open_app(cx, ws.path(), base.path(), Some(a.clone()));
        cx.update(|_, cx| {
            app.update(cx, |this, _| {
                this.store.put((a.clone(), "SELECT 1".into()), done("SELECT 1", &[("a", "int4")], vec![vec![Int(1)]], false));
                this.store.put((a.clone(), "SELECT 2".into()), done("SELECT 2", &[("b", "int4")], vec![vec![Int(2)], vec![Int(3)]], false));
            })
        });
        assert_eq!(shown_at(&app, &mut cx, 0).as_deref(), Some("SELECT 1"));
        assert_eq!(shown_at(&app, &mut cx, 2).as_deref(), Some("SELECT 2"));
        let rows = cx.update(|_, cx| app.read(cx).ui.table.read(cx).delegate().order.len());
        assert_eq!(rows, 2);
        // `SELECT 3` was never run: nothing to show. Back on the first one, its result is back.
        assert_eq!(shown_at(&app, &mut cx, 5), None);
        assert_eq!(shown_at(&app, &mut cx, 0).as_deref(), Some("SELECT 1"));
        // The sidebar counts rows per statement.
        let status = |line: usize, cx: &mut VisualTestContext| cx.update(|_, cx| app.read(cx).statement_status(&a, line, cx));
        assert_eq!(status(3, &mut cx), Some(("2".to_string(), false)));
        assert_eq!(status(6, &mut cx), None);
        // Pin keeps the first result while the cursor moves.
        cx.update(|_, cx| app.update(cx, |this, cx| this.ui.pinned = this.active_key(cx)));
        assert_eq!(shown_at(&app, &mut cx, 2).as_deref(), Some("SELECT 1"));
    }

    #[gpui::test]
    fn popup_shows_formatted_json_and_the_row_as_json(cx: &mut TestAppContext) {
        use db_exec::Value::{Int, Json, Text};
        let (ws, base) = (workspace(), tempfile::tempdir().unwrap());
        let a = ws.path().join("a.sql");
        let (app, mut cx) = open_app(cx, ws.path(), base.path(), Some(a.clone()));
        let doc = serde_json::json!({"tags": ["x", "y"], "n": {"k": null}});
        let rows = vec![vec![Int(1), Json(doc.clone()), Text("{\"t\":1}".into())], vec![Int(2), Json(serde_json::json!([1])), Text("plain".into())]];
        cx.update(|_, cx| app.update(cx, |this, _| this.store.put((a.clone(), "SELECT 1".into()), done("SELECT 1", &[("id", "int4"), ("doc", "jsonb"), ("note", "text")], rows, false))));
        assert_eq!(shown_at(&app, &mut cx, 0).as_deref(), Some("SELECT 1"));
        let popup = |cx: &mut VisualTestContext, row: usize, col: Option<usize>| {
            cx.update(|window, cx| app.update(cx, |this, cx| this.open_popup(row, col, window, cx)));
            cx.run_until_parked();
            cx.update(|_, cx| {
                let p = app.read(cx).ui.popup.clone().unwrap();
                let p = p.read(cx);
                (p.title(), p.shown_text().to_string())
            })
        };
        let (title, text) = popup(&mut cx, 0, Some(1));
        assert_eq!(title, "doc · jsonb · row 1");
        assert_eq!(text, serde_json::to_string_pretty(&doc).unwrap());
        assert!(text.contains("\n  \"tags\": [\n    \"x\","));
        // Text that parses as JSON is formatted too; plain text stays as it is.
        assert_eq!(popup(&mut cx, 0, Some(2)).1, "{\n  \"t\": 1\n}");
        assert_eq!(popup(&mut cx, 1, Some(2)).1, "plain");
        // The whole row: a JSON object in column order.
        let (title, text) = popup(&mut cx, 0, None);
        assert_eq!(title, "Row 1");
        assert!(text.starts_with("{\n  \"id\": 1,\n  \"doc\": {\n    \"tags\": ["), "{text}");
        assert!(serde_json::from_str::<serde_json::Value>(&text).is_ok());
    }

    #[gpui::test]
    fn sort_clicks_and_filters_become_the_page(cx: &mut TestAppContext) {
        use db_exec::Value::Int;
        let (ws, base) = (workspace(), tempfile::tempdir().unwrap());
        let a = ws.path().join("a.sql");
        let key = (a.clone(), "SELECT 1".to_string());
        let (app, mut cx) = open_app(cx, ws.path(), base.path(), Some(a.clone()));
        cx.update(|_, cx| app.update(cx, |this, _| this.store.put(key.clone(), done("SELECT 1", &[("a", "int4"), ("b", "int4")], vec![vec![Int(1), Int(2)]; 3], true))));
        assert_eq!(shown_at(&app, &mut cx, 0).as_deref(), Some("SELECT 1"));
        // No connection is picked, so nothing is sent; the page that would be asked for is what counts.
        let page = |cx: &mut VisualTestContext| cx.update(|_, cx| app.read(cx).store.entries[&key].page.clone());
        let click = |col: &str, shift: bool, cx: &mut VisualTestContext| {
            let col = col.to_string();
            cx.update(|window, cx| app.update(cx, |this, cx| this.sort_by(col, shift, window, cx)))
        };
        click("a", false, &mut cx);
        assert_eq!(page(&mut cx).sort, [("a".to_string(), false)]);
        click("a", false, &mut cx);
        assert_eq!(page(&mut cx).sort, [("a".to_string(), true)]);
        click("b", true, &mut cx);
        assert_eq!(page(&mut cx).sort, [("a".to_string(), true), ("b".to_string(), false)]);
        click("a", false, &mut cx); // a plain click works on that column alone: desc → none
        assert_eq!(page(&mut cx).sort, []);
        // Next page: offset by one page size; the first page has no previous one.
        cx.update(|window, cx| app.update(cx, |this, cx| this.step_page(false, window, cx)));
        assert_eq!(page(&mut cx).offset, 0);
        cx.update(|window, cx| app.update(cx, |this, cx| this.step_page(true, window, cx)));
        assert_eq!(page(&mut cx).offset, 100);
        // Filter fields: Enter applies them as (column, text); a sort resets to the first page.
        cx.update(|window, cx| {
            app.update(cx, |this, cx| {
                this.toggle_filters(window, cx);
                this.sync_results(window, cx);
                let inputs = this.ui.table.read(cx).delegate().filters.clone();
                assert_eq!(inputs.len(), 2);
                inputs[1].update(cx, |s, cx| s.set_value(">10", window, cx));
                this.apply_filters(window, cx);
            })
        });
        let p = page(&mut cx);
        assert_eq!((p.filters, p.offset), ([("b".to_string(), ">10".to_string())].to_vec(), 0));
    }

    #[gpui::test]
    fn alt_down_and_up_step_between_statements(cx: &mut TestAppContext) {
        let (ws, base) = (workspace(), tempfile::tempdir().unwrap());
        let (app, mut cx) = open_app(cx, ws.path(), base.path(), Some(ws.path().join("a.sql")));
        assert_eq!(cursor_line(&app, &mut cx), 0);
        for want in [2, 5] {
            cx.update(|window, cx| app.update(cx, |this, cx| this.step_statement(true, window, cx)));
            assert_eq!(cursor_line(&app, &mut cx), want);
        }
        // No next statement: the cursor stays.
        cx.update(|window, cx| app.update(cx, |this, cx| this.step_statement(true, window, cx)));
        assert_eq!(cursor_line(&app, &mut cx), 5);
        cx.update(|window, cx| app.update(cx, |this, cx| this.step_statement(false, window, cx)));
        assert_eq!(cursor_line(&app, &mut cx), 2);
    }

    #[gpui::test]
    fn clicking_a_statement_in_the_tree_moves_the_cursor(cx: &mut TestAppContext) {
        let (ws, base) = (workspace(), tempfile::tempdir().unwrap());
        let (app, mut cx) = open_app(cx, ws.path(), base.path(), Some(ws.path().join("b.sql")));
        let a = ws.path().join("a.sql");
        // The file expands into its statements: `@name` or the first line.
        let rows = cx.update(|_, cx| {
            app.update(cx, |this, cx| {
                this.expanded.insert(a.clone());
                this.build_rows("", cx)
            })
        });
        let statements: Vec<(usize, String)> = rows
            .iter()
            .filter_map(|r| match r {
                crate::sidebar::Row::Statement { path, line, label, .. } if *path == a => Some((*line, label.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(statements, [(1, "SELECT 1".to_string()), (3, "SELECT 2".to_string()), (6, "third".to_string())]);
        // A click on the third one opens the file and puts the cursor on its line (a double click would also run it).
        cx.update(|window, cx| app.update(cx, |this, cx| this.click_statement(a.clone(), 6, 1, window, cx)));
        cx.run_until_parked();
        assert_eq!(cx.update(|_, cx| app.read(cx).doc().unwrap().path.as_ref().clone()), a);
        assert_eq!(cursor_line(&app, &mut cx), 5);
        assert_eq!(cx.update(|_, cx| app.read(cx).doc().unwrap().active_statement().map(|s| s.line)), Some(6));
    }

    #[gpui::test]
    fn picker_is_remembered_per_file_and_at_connection_wins(tcx: &mut TestAppContext) {
        let (ws, base) = (workspace(), tempfile::tempdir().unwrap());
        let (a, b, c) = (ws.path().join("a.sql"), ws.path().join("b.sql"), ws.path().join("c.sql"));
        let (app, mut cx) = open_app(tcx, ws.path(), base.path(), Some(a.clone()));
        let conn = |app: &Entity<Siesta>, cx: &mut VisualTestContext, p: &Path| -> (Option<String>, sql_split::Dialect) {
            cx.update(|_, cx| {
                let d = app.read(cx).docs.iter().find(|d| *d.path == p).unwrap();
                (d.connection().map(str::to_string), d.dialect)
            })
        };
        // Nothing picked: no connection, default dialect.
        assert_eq!(conn(&app, &mut cx, &a), (None, sql_split::Dialect::Postgres));
        cx.update(|window, cx| app.update(cx, |this, cx| this.select_connection(Some("dev".into()), window, cx)));
        cx.update(|window, cx| {
            app.update(cx, |this, cx| {
                this.open(b.clone(), None, window, cx);
                this.select_connection(Some("my".into()), window, cx);
                this.open(c.clone(), None, window, cx);
                // c.sql names its own connection: `my` from the file, whatever was picked before.
                this.select_connection(Some("dev".into()), window, cx);
            })
        });
        assert_eq!(conn(&app, &mut cx, &a), (Some("dev".into()), sql_split::Dialect::Postgres));
        assert_eq!(conn(&app, &mut cx, &b), (Some("my".into()), sql_split::Dialect::MySql)); // the dialect follows the driver
        assert_eq!(conn(&app, &mut cx, &c), (Some("my".into()), sql_split::Dialect::MySql));
        // The picks survive a restart (same state dir), tabs included.
        let state = siesta_workspace::load_state(base.path(), ws.path());
        assert_eq!(state.connections[&a].as_deref(), Some("dev"));
        assert_eq!(state.connections[&b].as_deref(), Some("my"));
        assert_eq!(state.open.len(), 3);
        let (again, mut cx2) = open_app(tcx, ws.path(), base.path(), None);
        assert_eq!(conn(&again, &mut cx2, &a), (Some("dev".into()), sql_split::Dialect::Postgres));
        assert_eq!(conn(&again, &mut cx2, &b), (Some("my".into()), sql_split::Dialect::MySql));
        // A restored pick reads its schema in the background right away (nothing was run).
        let loading = cx2.update(|_, cx| {
            let this = again.read(cx);
            let key = this.docs.iter().find(|d| *d.path == a).unwrap().schema_key().unwrap();
            this.schemas.borrow().loading.contains(&key)
        });
        assert!(loading);
    }

    #[gpui::test]
    fn database_precedence_session_key_schema_key_and_directive_rewrite(tcx: &mut TestAppContext) {
        use crate::runner::Slot;
        let (ws, base) = (workspace(), tempfile::tempdir().unwrap());
        let a = ws.path().join("a.sql");
        let (app, mut cx) = open_app(tcx, ws.path(), base.path(), Some(a.clone()));
        // (database(), shown database, session key's database, schema key)
        let st = |app: &Entity<Siesta>, cx: &mut VisualTestContext| {
            cx.update(|_, cx| {
                let this = app.read(cx);
                let d = this.doc().unwrap();
                (d.database().map(str::to_string), d.shown_database(), this.connection_of(&a).unwrap().1.database, d.schema_key().unwrap())
            })
        };
        let select = |cx: &mut VisualTestContext, db: Option<&str>| cx.update(|window, cx| app.update(cx, |this, cx| this.select_database(db.map(Into::into), window, cx)));
        let connect = |cx: &mut VisualTestContext, n: &str| cx.update(|window, cx| app.update(cx, |this, cx| this.select_connection(Some(n.into()), window, cx)));
        connect(&mut cx, "dev");
        let (_, default_db, _, default_key) = st(&app, &mut cx);
        // No choice: the connection's default.
        assert_eq!(st(&app, &mut cx).0, None);

        // A pick (no directive in the text): remembered, and it changes the session key and the schema cache key.
        select(&mut cx, Some("app"));
        let (pick, shown, session_db, key_app) = st(&app, &mut cx);
        assert_eq!((pick.as_deref(), shown.as_deref(), session_db.as_deref()), (Some("app"), Some("app"), Some("app")));
        assert_ne!(key_app, default_key);
        assert_eq!(siesta_workspace::load_state(base.path(), ws.path()).databases[&a], "app");
        // Switching closes the session of the old database (nothing in a transaction: no question).
        cx.update(|_, cx| app.update(cx, |this, _| _ = this.slots.insert((a.clone(), "dev".into(), Some("app".into())), Slot::new())));
        select(&mut cx, Some("other"));
        let (_, _, session_db, key_other) = st(&app, &mut cx);
        assert_eq!(session_db.as_deref(), Some("other"));
        assert!(key_other != key_app && key_other != default_key);
        assert!(cx.update(|_, cx| app.read(cx).slots.keys().all(|(_, _, db)| db.as_deref() != Some("app"))));
        // A transaction in the old session is asked about first.
        cx.update(|_, cx| {
            app.update(cx, |this, _| {
                let mut slot = Slot::new();
                slot.in_tx = true;
                this.slots.insert((a.clone(), "dev".into(), Some("other".into())), slot);
            })
        });
        select(&mut cx, Some("app"));
        assert!(cx.pending_prompt().unwrap().0.contains("open transaction"));
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert_eq!(st(&app, &mut cx).0.as_deref(), Some("other"));
        // "Default" forgets the pick.
        cx.update(|_, cx| app.update(cx, |this, _| _ = this.slots.remove(&(a.clone(), "dev".into(), Some("other".into())))));
        select(&mut cx, None);
        let (pick, shown, _, key) = st(&app, &mut cx);
        assert_eq!((pick, shown, key), (None, default_db, default_key));
        assert!(siesta_workspace::load_state(base.path(), ws.path()).databases.is_empty());

        // `-- @database` wins over the pick.
        select(&mut cx, Some("app"));
        cx.update(|window, cx| {
            app.update(cx, |this, cx| {
                this.doc().unwrap().editor.update(cx, |s, cx| s.set_value("-- @database fromfile\nSELECT 1;\n", window, cx));
                this.docs[0].reparse(cx);
            })
        });
        assert_eq!(st(&app, &mut cx).2.as_deref(), Some("fromfile"));
        assert_eq!(cx.update(|_, cx| app.read(cx).doc().unwrap().locked_db().map(str::to_string)).as_deref(), Some("fromfile"));
        // Choosing another one asks, and rewrites the line (the pick stays); Cancel changes nothing.
        let text = |cx: &mut VisualTestContext| text_of(&app, cx);
        select(&mut cx, Some("x"));
        assert!(cx.pending_prompt().unwrap().1.contains("-- @database fromfile"));
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert_eq!(text(&mut cx), "-- @database fromfile\nSELECT 1;\n");
        select(&mut cx, Some("x"));
        cx.simulate_prompt_answer("Rewrite");
        cx.run_until_parked();
        assert_eq!(text(&mut cx), "-- @database x\nSELECT 1;\n");
        assert_eq!(st(&app, &mut cx).2.as_deref(), Some("x"));
        assert_eq!(siesta_workspace::load_state(base.path(), ws.path()).databases[&a], "app");
        // "Default" removes the line; the pick applies again.
        select(&mut cx, None);
        cx.simulate_prompt_answer("Rewrite");
        cx.run_until_parked();
        assert_eq!(text(&mut cx), "SELECT 1;\n");
        assert_eq!(st(&app, &mut cx).2.as_deref(), Some("app"));

        // Another connection starts from its own default again.
        connect(&mut cx, "my");
        let (pick, ..) = st(&app, &mut cx);
        assert_eq!(pick, None);
        assert!(siesta_workspace::load_state(base.path(), ws.path()).databases.is_empty());
    }

    #[gpui::test]
    fn at_connection_locks_the_picker_and_keeps_the_stored_pick(tcx: &mut TestAppContext) {
        let (ws, base) = (workspace(), tempfile::tempdir().unwrap());
        let (app, mut cx) = open_app(tcx, ws.path(), base.path(), Some(ws.path().join("a.sql")));
        let state = |app: &Entity<Siesta>, cx: &mut VisualTestContext| {
            cx.update(|_, cx| {
                let d = app.read(cx).doc().unwrap();
                (d.connection().map(str::to_string), d.locked().map(str::to_string), d.picked.clone())
            })
        };
        let set_text = |cx: &mut VisualTestContext, text: &'static str| {
            cx.update(|window, cx| {
                app.update(cx, |this, cx| {
                    this.doc().unwrap().editor.update(cx, |s, cx| s.set_value(text, window, cx));
                    this.docs[0].reparse(cx);
                })
            })
        };
        cx.update(|window, cx| app.update(cx, |this, cx| this.select_connection(Some("dev".into()), window, cx)));
        assert_eq!(state(&app, &mut cx), (Some("dev".into()), None, Some("dev".into())));
        set_text(&mut cx, "-- @connection my\nSELECT 1;\n");
        let pick = |cx: &mut VisualTestContext, n: Option<&str>| {
            cx.update(|window, cx| app.update(cx, |this, cx| this.select_connection(n.map(Into::into), window, cx)));
        };
        let text = |cx: &mut VisualTestContext| text_of(&app, cx);
        // Cancel: a question (naming the prod target), nothing changes.
        pick(&mut cx, Some("live"));
        assert!(cx.pending_prompt().unwrap().1.contains("PRODUCTION"));
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert_eq!(state(&app, &mut cx), (Some("my".into()), Some("my".into()), Some("dev".into())));
        assert_eq!(text(&mut cx), "-- @connection my\nSELECT 1;\n");
        // Rewrite: the line changes in the buffer (undoable edit), the connection follows, the pick stays.
        pick(&mut cx, Some("live"));
        cx.simulate_prompt_answer("Rewrite");
        cx.run_until_parked();
        assert_eq!(text(&mut cx), "-- @connection live\nSELECT 1;\n");
        assert_eq!(state(&app, &mut cx), (Some("live".into()), Some("live".into()), Some("dev".into())));
        assert!(cx.update(|_, cx| app.read(cx).doc().unwrap().dirty));
        // "No connection" removes the directive; the stored pick applies again.
        pick(&mut cx, None);
        cx.simulate_prompt_answer("Rewrite");
        cx.run_until_parked();
        assert_eq!(text(&mut cx), "SELECT 1;\n");
        assert_eq!(state(&app, &mut cx), (Some("dev".into()), None, Some("dev".into())));
    }

    #[gpui::test]
    fn prod_run_asks_once_and_paging_does_not(tcx: &mut TestAppContext) {
        use db_exec::Value::Int;
        let (ws, base) = (workspace(), tempfile::tempdir().unwrap());
        let a = ws.path().join("a.sql");
        let (app, mut cx) = open_app(tcx, ws.path(), base.path(), Some(a.clone()));
        let run = |cx: &mut VisualTestContext| cx.update(|window, cx| app.update(cx, |this, cx| this.run_current(window, cx)));
        let ran = |cx: &mut VisualTestContext| cx.update(|_, cx| app.read(cx).store.last.is_some());
        // A dev connection runs a plain SELECT without asking.
        cx.update(|window, cx| app.update(cx, |this, cx| this.select_connection(Some("dev".into()), window, cx)));
        run(&mut cx);
        assert!(!cx.has_pending_prompt() && ran(&mut cx));
        cx.update(|_, cx| app.update(cx, |this, _| this.store.last = None));

        // A prod one runs a SELECT without asking, but asks for an UPDATE; Cancel runs nothing.
        cx.update(|window, cx| app.update(cx, |this, cx| this.select_connection(Some("live".into()), window, cx)));
        run(&mut cx);
        assert!(!cx.has_pending_prompt() && ran(&mut cx));
        cx.update(|_, cx| app.update(cx, |this, _| this.store.last = None));
        cx.update(|window, cx| {
            app.update(cx, |this, cx| {
                this.doc().unwrap().editor.update(cx, |s, cx| s.set_value("UPDATE t SET a = 1", window, cx));
                this.docs[0].reparse(cx);
            })
        });
        run(&mut cx);
        assert!(cx.has_pending_prompt());
        assert!(cx.pending_prompt().unwrap().0.contains("PRODUCTION live"));
        cx.simulate_prompt_answer("Cancel");
        cx.run_until_parked();
        assert!(!ran(&mut cx));
        run(&mut cx);
        cx.simulate_prompt_answer("Run");
        cx.run_until_parked();
        assert!(ran(&mut cx));

        // Paging, sorting and filtering re-run the confirmed result: no second question.
        let key = (a.clone(), "SELECT 9".to_string());
        cx.update(|_, cx| app.update(cx, |this, _| this.store.put(key.clone(), done("SELECT 9", &[("a", "int4")], vec![vec![Int(1)]; 3], true))));
        cx.update(|_, cx| app.update(cx, |this, _| this.ui.pinned = Some(key.clone())));
        cx.update(|window, cx| app.update(cx, |this, cx| this.step_page(true, window, cx)));
        assert!(!cx.has_pending_prompt());
        // The re-run really was requested (page 2, running), without a dialog.
        let (offset, running) = cx.update(|_, cx| {
            let e = &app.read(cx).store.entries[&key];
            (e.page.offset, e.running.is_some())
        });
        assert_eq!((offset, running), (100, true));
    }

    #[gpui::test]
    fn form_saves_into_the_config_dir_and_picks_for_its_own_file_only(tcx: &mut TestAppContext) {
        use db_conn::Driver;
        let (ws, base, cfg) = (workspace(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (a, b) = (ws.path().join("a.sql"), ws.path().join("b.sql"));
        let (app, mut cx) = open_app(tcx, ws.path(), base.path(), Some(a.clone()));
        let dir = cfg.path().join("siesta");
        cx.update(|window, cx| {
            app.update(cx, |this, cx| {
                this.config_dir = Some(dir.clone());
                this.open_conn_form(None, window, cx);
                // The user moves on to another file before pressing Save.
                this.open(b.clone(), None, window, cx);
            })
        });
        let form = cx.update(|_, cx| app.read(cx).conn_form.as_ref().and_then(|f| f.upgrade()).expect("the form is open"));
        cx.update(|window, cx| {
            form.update(cx, |f, cx| {
                f.name.update(cx, |s, cx| s.set_value("fresh", window, cx));
                f.host.update(cx, |s, cx| s.set_value("127.0.0.1", window, cx));
                f.port.update(cx, |s, cx| s.set_value("1", window, cx));
                f.password.update(cx, |s, cx| s.set_value("pw", window, cx));
                f.driver = Some(Driver::MySql);
                f.save(window, cx);
            })
        });
        let file: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join("connections/fresh.json")).unwrap()).unwrap();
        let mut file = file;
        assert!(file.as_object_mut().unwrap().remove("password_enc").is_some());
        assert_eq!(file, serde_json::json!({"driver": "mysql", "host": "127.0.0.1", "port": 1}));
        // The key went to the data dir (`base`), not next to the connection files.
        assert!(db_conn::crypto::key_path(base.path()).is_file() && !db_conn::crypto::key_path(&dir).exists());
        let stored = |d: &Path, n: &str| db_conn::Connections::load(d).connections[n].resolve_password_in(base.path()).unwrap().password;
        assert_eq!(stored(&dir, "fresh").as_deref(), Some("pw"));
        // Every open file sees the connection; only `a.sql` (the file the form was opened for) picked it.
        let (pa, pb, known) = cx.update(|_, cx| {
            let this = app.read(cx);
            let doc = |p: &Path| this.docs.iter().find(|d| *d.path == p).unwrap();
            (doc(&a).picked.clone(), doc(&b).picked.clone(), doc(&b).conns.connections.contains_key("fresh"))
        });
        assert_eq!((pa.as_deref(), pb, known), (Some("fresh"), None, true));
        let state = siesta_workspace::load_state(base.path(), ws.path());
        assert_eq!((state.connections[&a].as_deref(), state.connections.get(&b)), (Some("fresh"), None));
        // Edit: the same dialog on the saved connection, then Delete clears the pick.
        cx.update(|window, cx| {
            app.update(cx, |this, cx| {
                this.open(a.clone(), None, window, cx);
                this.open_conn_form(Some("fresh".into()), window, cx);
            })
        });
        let form = cx.update(|_, cx| app.read(cx).conn_form.as_ref().and_then(|f| f.upgrade()).unwrap());
        assert_eq!(cx.update(|_, cx| form.read(cx).collect(cx).host), "127.0.0.1");
        cx.update(|window, cx| app.update(cx, |this, cx| this.connection_deleted("fresh", window, cx)));
        cx.update(|_, cx| assert!(app.read(cx).docs.iter().all(|d| d.picked.is_none())));
    }

    #[gpui::test]
    fn a_single_file_is_a_workspace_of_its_own(tcx: &mut TestAppContext) {
        let (ws, base) = (workspace(), tempfile::tempdir().unwrap());
        let b = ws.path().join("b.sql");
        let (app, mut cx) = open_app(tcx, ws.path(), base.path(), None);
        cx.update(|window, cx| app.update(cx, |this, cx| this.set_root(b.clone(), window, cx)));
        let (files, tabs) = cx.update(|_, cx| {
            let this = app.read(cx);
            (this.files.clone(), this.docs.iter().map(|d| (*d.path).clone()).collect::<Vec<_>>())
        });
        assert_eq!((files, tabs), (vec![b.clone()], vec![b.clone()]));
        // History: the one history folder, entries tagged with this workspace, paths relative to the file's folder.
        cx.update(|_, cx| app.update(cx, |this, cx| this.record_history(&b, vec![db_exec::history::Entry { connection: "dev".into(), sql: "SELECT 'b'".into(), ..Default::default() }], cx)));
        cx.run_until_parked();
        let store = db_exec::history::Store::open(base.path(), &b).unwrap();
        assert_eq!(store.dir(), base.path().join("history"));
        assert_eq!((store.list()[0].file.as_str(), store.list()[0].workspace.as_str()), ("b.sql", b.to_str().unwrap()));
        assert!(db_exec::history::Store::open(base.path(), ws.path()).unwrap().list().is_empty());
        // State is kept under the file's own path, and a plain launch reopens it.
        cx.update(|_, cx| app.read(cx).remember(cx));
        assert_eq!(siesta_workspace::load_state(base.path(), &b).current, Some(b.clone()));
        assert_eq!(siesta_workspace::last_root(base.path()), Some(b));
    }

    #[gpui::test]
    fn clicking_a_column_adds_it_to_the_select_under_the_cursor(tcx: &mut TestAppContext) {
        let (ws, base) = (workspace(), tempfile::tempdir().unwrap());
        let (app, mut cx) = open_app(tcx, ws.path(), base.path(), Some(ws.path().join("b.sql")));
        let set = |cx: &mut VisualTestContext, text: &'static str| {
            cx.update(|window, cx| {
                app.update(cx, |this, cx| {
                    this.docs[0].editor.update(cx, |s, cx| {
                        s.set_value(text, window, cx);
                        s.set_cursor_position(gpui_component::input::Position::new(0, 0), window, cx);
                    });
                    this.docs[0].reparse(cx);
                })
            })
        };
        let click = |cx: &mut VisualTestContext, col: &'static str| {
            cx.update(|window, cx| app.update(cx, |this, cx| this.add_column(col, window, cx)));
            text_of(&app, cx)
        };
        set(&mut cx, "SELECT * FROM t;\n");
        assert_eq!(click(&mut cx, "id"), "SELECT id FROM t;\n");
        assert_eq!(click(&mut cx, "kind"), "SELECT id, kind FROM t;\n");
        assert_eq!(click(&mut cx, "id"), "SELECT id, kind FROM t;\n"); // already there
        assert_eq!(click(&mut cx, "Created At"), "SELECT id, kind, \"Created At\" FROM t;\n");
        // Not a SELECT: the name goes at the cursor.
        set(&mut cx, "UPDATE t SET  = 1;\n");
        cx.update(|window, cx| app.update(cx, |this, cx| this.docs[0].editor.update(cx, |s, cx| s.set_cursor_position(gpui_component::input::Position::new(0, 13), window, cx))));
        assert_eq!(click(&mut cx, "kind"), "UPDATE t SET kind = 1;\n");
    }

    #[gpui::test]
    fn history_records_filters_and_inserts(tcx: &mut TestAppContext) {
        let (ws, base) = (workspace(), tempfile::tempdir().unwrap());
        let b = ws.path().join("b.sql");
        let (app, mut cx) = open_app(tcx, ws.path(), base.path(), Some(b.clone()));
        let entry = |sql: &str, error: Option<&str>| db_exec::history::Entry { connection: "dev".into(), sql: sql.into(), error: error.map(Into::into), rows: Some(1), ..Default::default() };
        cx.update(|_, cx| app.update(cx, |this, cx| this.record_history(&b, vec![entry("SELECT 42", None), entry("SELECT nope", Some("42703 column does not exist"))], cx)));
        cx.run_until_parked();
        cx.update(|window, cx| app.update(cx, |this, cx| this.toggle_history(window, cx)));
        let shown = |cx: &mut VisualTestContext| cx.update(|_, cx| {
            let this = app.read(cx);
            (this.show_history, this.history.iter().map(|e| (e.sql.clone(), e.file.clone(), e.outcome())).collect::<Vec<_>>())
        });
        assert_eq!(shown(&mut cx), (true, vec![("SELECT nope".into(), "b.sql".into(), "42703 column does not exist".into()), ("SELECT 42".into(), "b.sql".into(), "1 row".into())]));
        // The filter matches errors too; rows are computed when the page renders.
        cx.update(|window, cx| app.update(cx, |this, cx| this.history_filter.update(cx, |s, cx| s.set_value("42703", window, cx))));
        cx.run_until_parked();
        assert_eq!(cx.update(|_, cx| app.read(cx).history_rows.clone()), vec![0]);
        // Insert: into the file it was recorded in (b.sql, though a.sql is current), after a blank
        // line, with a `;`; the page closes.
        cx.update(|window, cx| {
            app.update(cx, |this, cx| {
                this.open(ws.path().join("a.sql"), None, window, cx);
                this.show_history = true;
                _ = this.history_insert(1, window, cx);
            })
        });
        let current = cx.update(|_, cx| (*app.read(cx).doc().unwrap().path).clone());
        assert_eq!((current, text_of(&app, &mut cx)), (b.clone(), "SELECT 'b';\n\nSELECT 42;\n".into()));
        assert!(!shown(&mut cx).0);
    }

    #[gpui::test]
    fn a_finished_run_lands_in_history_but_a_page_change_does_not(tcx: &mut TestAppContext) {
        use crate::runner::{Report, Stmt};
        let (ws, base) = (workspace(), tempfile::tempdir().unwrap());
        let a = ws.path().join("a.sql");
        let (app, mut cx) = open_app(tcx, ws.path(), base.path(), Some(a.clone()));
        // `finish` with a ready report: a real run would wake the test scheduler from the tokio thread.
        let finish = |cx: &mut VisualTestContext, fresh: bool| {
            cx.update(|window, cx| {
                app.update(cx, |this, cx| {
                    let conn = this.doc().unwrap().conns.connections["dev"].clone();
                    let stmt = Stmt { text: "SELECT 2".into(), kind: sql_split::Kind::Query, page: Default::default(), range: None, fresh };
                    let report = Report { results: vec![Err(db_exec::DbError { message: "connection refused".into(), ..Default::default() })], in_tx: false, live: false, skipped: false };
                    this.finish(a.clone(), "dev".into(), conn, vec![stmt], vec![(a.clone(), "SELECT 2".into())], report, window, cx);
                })
            });
            cx.run_until_parked();
            db_exec::history::Store::open(base.path(), ws.path()).unwrap().list()
        };
        assert!(finish(&mut cx, false).is_empty());
        let list = finish(&mut cx, true);
        let e = &list[0];
        assert_eq!((e.sql.as_str(), e.connection.as_str(), e.file.as_str(), e.line, e.source.as_str()), ("SELECT 2", "dev", "a.sql", 3, "gui"));
        assert_eq!((e.error.as_deref(), e.rows), (Some("connection refused"), None));
    }

    #[gpui::test]
    fn typed_at_connection_resets_the_database_pick(tcx: &mut TestAppContext) {
        let (ws, base) = (workspace(), tempfile::tempdir().unwrap());
        let a = ws.path().join("a.sql");
        let (app, mut cx) = open_app(tcx, ws.path(), base.path(), Some(a.clone()));
        let type_text = |cx: &mut VisualTestContext, text: &'static str| {
            cx.update(|window, cx| {
                let editor = app.read(cx).doc().unwrap().editor.clone();
                editor.update(cx, |s, cx| {
                    s.set_value(text, window, cx);
                    cx.emit(gpui_component::input::InputEvent::Change); // what typing sends
                });
            });
            cx.run_until_parked();
        };
        let state = |cx: &mut VisualTestContext| cx.update(|_, cx| {
            let this = app.read(cx);
            (this.doc().unwrap().connection().map(str::to_string), this.doc().unwrap().picked_db.clone(), this.db_picks.get(&a).cloned())
        });
        cx.update(|window, cx| {
            app.update(cx, |this, cx| {
                this.select_connection(Some("dev".into()), window, cx);
                this.select_database(Some("other".into()), window, cx);
            })
        });
        assert_eq!(state(&mut cx), (Some("dev".into()), Some("other".into()), Some("other".into())));
        // Same connection typed: the pick stays.
        type_text(&mut cx, "-- @connection dev\nSELECT 1;\n");
        assert_eq!(state(&mut cx).1.as_deref(), Some("other"));
        type_text(&mut cx, "-- @connection my\nSELECT 1;\n");
        assert_eq!(state(&mut cx), (Some("my".into()), None, None));
    }

    #[gpui::test]
    fn rename_moves_picks_and_offers_to_rewrite_directives_and_closing_drops_the_form(tcx: &mut TestAppContext) {
        let (ws, base) = (workspace(), tempfile::tempdir().unwrap());
        let (a, c, d) = (ws.path().join("a.sql"), ws.path().join("c.sql"), ws.path().join("d.sql"));
        std::fs::write(&d, "-- @connection my\nSELECT 'd';\n").unwrap();
        let (app, mut cx) = open_app(tcx, ws.path(), base.path(), Some(a.clone()));
        cx.update(|window, cx| {
            app.update(cx, |this, cx| {
                this.select_connection(Some("my".into()), window, cx);
                this.open(c.clone(), None, window, cx);
                this.open_conn_form_for(Some("my".into()), false, false, window, cx);
            })
        });
        let form = cx.update(|_, cx| app.read(cx).conn_form.as_ref().and_then(|f| f.upgrade()).unwrap());
        cx.update(|window, cx| {
            form.update(cx, |f, cx| {
                f.name.update(cx, |s, cx| s.set_value("mine", window, cx));
                f.save(window, cx);
            })
        });
        drop(form);
        cx.run_until_parked();
        cx.simulate_prompt_answer("Rewrite");
        cx.run_until_parked();
        let (picked, c_text) = cx.update(|_, cx| {
            let this = app.read(cx);
            let doc = |p: &Path| this.docs.iter().find(|d| *d.path == p).unwrap();
            (doc(&a).picked.clone(), doc(&c).editor.read(cx).value().to_string())
        });
        assert_eq!(picked.as_deref(), Some("mine"));
        assert_eq!(c_text, "-- @connection mine\nSELECT 'c';\n");
        assert_eq!(std::fs::read_to_string(&d).unwrap(), "-- @connection mine\nSELECT 'd';\n");
        // Esc / × close the dialog without the form's buttons: the form (and a running Test) goes with it.
        cx.update(|window, cx| app.update(cx, |this, cx| this.open_conn_form_for(Some("mine".into()), false, false, window, cx)));
        cx.update(gpui_component::WindowExt::close_dialog);
        cx.run_until_parked();
        assert!(cx.update(|_, cx| app.read(cx).conn_form.as_ref().and_then(|f| f.upgrade()).is_none()));
    }

    #[gpui::test]
    fn typing_a_url_fills_the_fields_and_saving_writes_no_url(tcx: &mut TestAppContext) {
        let (ws, base, cfg) = (workspace(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (app, mut cx) = open_app(tcx, ws.path(), base.path(), Some(ws.path().join("a.sql")));
        let dir = cfg.path().join("siesta");
        cx.update(|window, cx| {
            app.update(cx, |this, cx| {
                this.config_dir = Some(dir.clone());
                this.open_conn_form(None, window, cx);
            })
        });
        let form = cx.update(|_, cx| app.read(cx).conn_form.as_ref().and_then(|f| f.upgrade()).unwrap());
        cx.update(|window, cx| {
            form.update(cx, |f, cx| {
                f.name.update(cx, |s, cx| s.set_value("viaurl", window, cx));
                // `set_value` emits no Change event (typing does), so apply what the subscription would.
                f.apply_url("mysql://me:p%40ss@db.local:3307/shop?ssl-mode=REQUIRED", window, cx);
            })
        });
        cx.run_until_parked();
        let f = cx.update(|_, cx| form.read(cx).collect(cx));
        assert_eq!((f.driver, f.user.as_str(), f.password.as_str(), f.host.as_str(), f.port.as_str(), f.database.as_str(), f.ssl_mode), (Some(db_conn::Driver::MySql), "me", "p@ss", "db.local", "3307", "shop", Some(db_conn::SslMode::Require)));
        // An incomplete URL leaves the fields as they are.
        cx.update(|window, cx| form.update(cx, |f, cx| f.apply_url("postgres://", window, cx)));
        cx.run_until_parked();
        assert_eq!(cx.update(|_, cx| form.read(cx).collect(cx).host), "db.local");
        cx.update(|window, cx| form.update(cx, |f, cx| f.save(window, cx)));
        let file: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(dir.join("connections/viaurl.json")).unwrap()).unwrap();
        let mut file = file;
        assert!(file.as_object_mut().unwrap().remove("password_enc").is_some());
        assert_eq!(file, serde_json::json!({"driver": "mysql", "host": "db.local", "port": 3307, "database": "shop", "user": "me", "ssl": {"mode": "require"}}));
        assert_eq!(db_conn::Connections::load(&dir).connections["viaurl"].resolve_password_in(base.path()).unwrap().password.as_deref(), Some("p@ss"));
    }

    #[gpui::test]
    fn connections_page_lists_edits_and_deletes_without_touching_picks(tcx: &mut TestAppContext) {
        use crate::connections_page::{ConnectionsPage, build_rows};
        let (ws, base, cfg) = (workspace(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (a, dir) = (ws.path().join("a.sql"), cfg.path().to_path_buf());
        std::fs::create_dir_all(dir.join("connections")).unwrap();
        std::fs::write(dir.join("connections/gl.json"), r#"{"driver": "postgres", "host": "h", "port": 5, "database": "d"}"#).unwrap();
        std::fs::write(dir.join("connections/other.json"), r#"{"driver": "postgres", "host": "127.0.0.1", "port": 1}"#).unwrap();
        let (app, mut cx) = open_app(tcx, ws.path(), base.path(), Some(a.clone()));
        db_conn::set_password(&dir, base.path(), "gl", "pw").unwrap();
        cx.update(|window, cx| {
            app.update(cx, |this, cx| {
                this.config_dir = Some(dir.clone());
                this.reload_connections(window, cx);
                this.pick_for(&a, Some("other".into()), window, cx);
            })
        });
        // The page is one flat list with a "password set" mark (`password_enc` present).
        let rows = cx.update(|_, cx| {
            let (conns, current) = app.read(cx).conns_for_page();
            build_rows(&conns, current.as_deref(), "")
        });
        assert_eq!(rows.iter().map(|r| (r.name.as_str(), r.password, r.used, r.target.as_str())).collect::<Vec<_>>(), [("gl", true, false, "h:5/d"), ("other", false, true, "127.0.0.1:1")]);
        // The page itself renders.
        let (weak, main) = (app.downgrade(), cx.update(|window, _| window.window_handle()));
        let page = tcx.update(|cx| cx.open_window(Default::default(), |w, cx| cx.new(|cx| ConnectionsPage::new(weak, main, w, cx))).unwrap());
        VisualTestContext::from_window(page.into(), tcx).run_until_parked();
        // Duplicate: a new form prefilled from `gl`, no password.
        cx.update(|window, cx| app.update(cx, |this, cx| this.open_conn_form_for(Some("gl".into()), true, false, window, cx)));
        let form = cx.update(|_, cx| app.read(cx).conn_form.as_ref().and_then(|f| f.upgrade()).unwrap());
        let f = cx.update(|_, cx| form.read(cx).collect(cx));
        assert_eq!((f.name.as_str(), f.host.as_str(), f.password.as_str(), f.has_password), ("gl-copy", "h", "", false));
        // Edit a connection the file did not pick: the pick stays; the stored password is announced, not shown.
        cx.update(|window, cx| app.update(cx, |this, cx| this.open_conn_form_for(Some("gl".into()), false, false, window, cx)));
        let form = cx.update(|_, cx| app.read(cx).conn_form.as_ref().and_then(|f| f.upgrade()).unwrap());
        let f = cx.update(|_, cx| form.read(cx).collect(cx));
        assert_eq!((f.name.as_str(), f.password.as_str(), f.has_password), ("gl", "", true));
        cx.update(|window, cx| {
            form.update(cx, |f, cx| {
                f.host.update(cx, |s, cx| s.set_value("edited", window, cx));
                f.save(window, cx);
            })
        });
        assert_eq!(cx.update(|_, cx| app.read(cx).doc().unwrap().picked.clone()).as_deref(), Some("other"));
        assert_eq!(db_conn::read_form(&dir, "gl").unwrap().host, "edited");
        assert_eq!(db_conn::Connections::load(&dir).connections["gl"].resolve_password_in(base.path()).unwrap().password.as_deref(), Some("pw")); // the empty password field kept it
        // Delete removes the connection with its password, and leaves the pick.
        cx.update(|window, cx| app.update(cx, |this, cx| this.delete_connection("gl", window, cx)));
        assert!(db_conn::read_form(&dir, "gl").is_none() && !db_conn::has_password(&dir, "gl"));
        assert_eq!(cx.update(|_, cx| app.read(cx).doc().unwrap().picked.clone()).as_deref(), Some("other"));
    }

    #[gpui::test]
    fn json_cells_stay_one_line_until_toggled_on(tcx: &mut TestAppContext) {
        use db_exec::Value::{Int, Json, Text};
        let (ws, base) = (workspace(), tempfile::tempdir().unwrap());
        let a = ws.path().join("a.sql");
        let (app, mut cx) = open_app(tcx, ws.path(), base.path(), Some(a.clone()));
        let rows = vec![vec![Int(1), Json(serde_json::json!({"a": [1, 2, 3], "b": {"c": null}}))], vec![Int(2), Text("[1]".into())]];
        cx.update(|_, cx| app.update(cx, |this, _| this.store.put((a.clone(), "SELECT 1".into()), done("SELECT 1", &[("id", "int4"), ("doc", "jsonb")], rows, false))));
        shown_at(&app, &mut cx, 0);
        let grid = |cx: &mut VisualTestContext| cx.update(|_, cx| {
            let g = app.read(cx).ui.table.read(cx).delegate();
            (g.has_json, g.tall())
        });
        assert_eq!(grid(&mut cx), (true, false)); // results.json_preview_lines defaults to 0
        // The `{ }` toggle turns it on (4 lines when the setting is 0) and off again.
        let toggle = |cx: &mut VisualTestContext| {
            cx.update(|_, cx| app.update(cx, |this, cx| this.toggle_json_preview(cx)));
            shown_at(&app, cx, 0);
        };
        toggle(&mut cx);
        assert_eq!((grid(&mut cx), cx.update(|_, cx| app.read(cx).json_lines())), ((true, true), 4));
        toggle(&mut cx);
        assert_eq!(grid(&mut cx), (true, false));
    }

    /// `public.users` (+ `analytics.events`), as `Session::schema()` would give it.
    fn demo_schema() -> db_exec::Schema {
        use db_exec::{ColumnInfo, SchemaNode, Table, TableKind};
        let table = |name: &str| Table {
            name: name.into(),
            kind: TableKind::Table,
            columns: vec![ColumnInfo { name: "id".into(), data_type: "integer".into(), nullable: false, primary_key: true, ordinal: 1 }],
            indexes: vec![],
        };
        db_exec::Schema {
            schemas: vec![
                SchemaNode { name: "analytics".into(), tables: vec![table("events")] },
                SchemaNode { name: "public".into(), tables: vec![table("users")] },
            ],
        }
    }

    fn text_of(app: &Entity<Siesta>, cx: &mut VisualTestContext) -> String {
        cx.update(|_, cx| app.read(cx).doc().unwrap().editor.read(cx).value().to_string())
    }

    #[gpui::test]
    fn db_tab_inserts_the_select_after_the_current_statement(tcx: &mut TestAppContext) {
        let (ws, base) = (workspace(), tempfile::tempdir().unwrap());
        let (app, mut cx) = open_app(tcx, ws.path(), base.path(), Some(ws.path().join("a.sql")));
        cx.update(|window, cx| {
            app.update(cx, |this, cx| {
                this.select_connection(Some("dev".into()), window, cx);
                let key = this.doc().unwrap().schema_key().unwrap();
                this.schemas.borrow_mut().schemas.insert(key, std::sync::Arc::new(demo_schema()));
            })
        });
        // The tree: `public` is the default schema, both are listed.
        let rows = cx.update(|_, cx| {
            app.update(cx, |this, _| {
                let key = this.doc().unwrap().schema_key().unwrap();
                crate::dbtab::rows(&this.schemas.borrow().schemas[&key], "", &this.db_toggled)
            })
        });
        assert_eq!(rows.len(), 4);
        // The cursor is on the first statement: the new one goes right below it and becomes the active one.
        let ok = cx.update(|window, cx| app.update(cx, |this, cx| this.insert_select("public", "users", window, cx)));
        assert!(ok);
        // A blank line separates it from the statement above.
        assert_eq!(text_of(&app, &mut cx), "SELECT 1;\n\nSELECT * FROM users;\n\nSELECT 2;\n\n-- @name third\nSELECT 3;\n");
        let active = cx.update(|_, cx| {
            let d = app.read(cx).doc().unwrap();
            let src = d.editor.read(cx).value();
            d.active_statement().map(|s| (s.line, s.text(&src).to_string()))
        });
        assert_eq!(active, Some((3, "SELECT * FROM users".to_string())));
        assert_eq!(cursor_line(&app, &mut cx), 2);
        // Outside the default schema the name is qualified; a statement without `;` gets one.
        cx.update(|window, cx| {
            app.update(cx, |this, cx| {
                this.docs[0].editor.update(cx, |s, cx| s.set_value("SELECT 1", window, cx));
                this.docs[0].reparse(cx);
                assert!(this.insert_select("analytics", "events", window, cx));
            })
        });
        assert_eq!(text_of(&app, &mut cx), "SELECT 1;\n\nSELECT * FROM analytics.events;");
        // "Insert name" replaces the selection.
        cx.update(|window, cx| {
            app.update(cx, |this, cx| {
                this.docs[0].editor.update(cx, |s, cx| s.set_value("SELECT  FROM x", window, cx));
                this.docs[0].editor.update(cx, |s, cx| s.set_cursor_position(gpui_component::input::Position::new(0, 7), window, cx));
                this.insert_name("public", "users", window, cx);
            })
        });
        assert_eq!(text_of(&app, &mut cx), "SELECT users FROM x");
    }

    #[gpui::test]
    fn completion_provider_offers_tables_after_from(tcx: &mut TestAppContext) {
        use gpui_component::input::{CompletionProvider as _, Rope};
        let (ws, base) = (workspace(), tempfile::tempdir().unwrap());
        let (app, mut cx) = open_app(tcx, ws.path(), base.path(), Some(ws.path().join("a.sql")));
        cx.update(|window, cx| {
            app.update(cx, |this, cx| {
                this.select_connection(Some("dev".into()), window, cx);
                let key = this.doc().unwrap().schema_key().unwrap();
                this.schemas.borrow_mut().schemas.insert(key, std::sync::Arc::new(demo_schema()));
            })
        });
        let lang = cx.update(|_, cx| crate::lang::Lang(app.read(cx).doc().unwrap().lang.clone()));
        let ask = |cx: &mut VisualTestContext, sql: &str| -> Vec<String> {
            let (text, offset) = (Rope::from(sql), sql.len());
            let ctx = lsp_types::CompletionContext { trigger_kind: lsp_types::CompletionTriggerKind::INVOKED, trigger_character: None };
            let task = cx.update(|window, cx| lang.completions(&text, offset, ctx, window, cx));
            match cx.foreground_executor().block_test(task).unwrap() {
                lsp_types::CompletionResponse::Array(items) => items.into_iter().map(|i| i.label).collect(),
                lsp_types::CompletionResponse::List(l) => l.items.into_iter().map(|i| i.label).collect(),
            }
        };
        assert_eq!(ask(&mut cx, "SELECT * FROM "), ["analytics.events", "users"]);
        assert_eq!(ask(&mut cx, "SELECT * FROM us"), ["users"]);
        assert_eq!(ask(&mut cx, "SELECT * FROM users u WHERE u."), ["id"]);
        // Directives need no schema; a string needs nothing at all.
        assert_eq!(ask(&mut cx, "-- @conf"), ["confirm"]);
        assert!(ask(&mut cx, "SELECT 'FROM us").is_empty());
        // The provider follows the picked connection: none, no tables.
        cx.update(|window, cx| app.update(cx, |this, cx| this.select_connection(None, window, cx)));
        assert!(ask(&mut cx, "SELECT * FROM ").is_empty());
    }

    #[test]
    fn db_error_text_is_first_line_truncated() {
        assert_eq!(super::db_error_text("password authentication failed\nDETAIL: x"), "Can't list databases: password authentication failed");
        let long = "x".repeat(200);
        assert_eq!(super::db_error_text(&long), format!("Can't list databases: {}…", "x".repeat(80)));
    }

    #[gpui::test]
    fn database_list_failure_shows_error_and_retry(tcx: &mut TestAppContext) {
        use super::DbRow;
        use crate::runner::DbList;
        let (ws, base) = (workspace(), tempfile::tempdir().unwrap());
        let a = ws.path().join("a.sql");
        let (app, mut cx) = open_app(tcx, ws.path(), base.path(), Some(a));
        cx.update(|window, cx| app.update(cx, |this, cx| this.select_connection(Some("dev".into()), window, cx)));
        let key = cx.update(|_, cx| crate::schema_cache::key(&app.read(cx).doc().unwrap().conns.connections["dev"]));
        let rows = |app: &Entity<Siesta>, cx: &mut VisualTestContext| cx.update(|_, cx| app.read(cx).db_rows().unwrap());
        let set = |cx: &mut VisualTestContext, l: DbList| cx.update(|_, cx| app.update(cx, |this, _| _ = this.db_lists.insert(key.clone(), l)));
        set(&mut cx, DbList::Failed("28P01 password authentication failed".into()));
        assert_eq!(rows(&app, &mut cx), vec![DbRow::Error("Can't list databases: 28P01 password authentication failed".into()), DbRow::Retry]);
        set(&mut cx, DbList::Ready(vec!["app".into()]));
        assert_eq!(rows(&app, &mut cx), vec![DbRow::Database("app".into()), DbRow::Refresh]);
    }
}
