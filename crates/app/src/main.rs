//! Siesta GUI: sidebar (Files | DB), tabs of open files, a plain-text SQL editor (no run buttons or
//! statuses in its lines), connection picker in the top-right corner, result panel on the right.
//!
//! Running statements lives in `runner`, the result panel and table in `results`, the value
//! dialog in `value_popup`, pure formatting in `format`; everything else of the shell is here.

mod complete;
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
    menu::{DropdownMenu as _, PopupMenuItem},
    resizable_panel,
};
use siesta_workspace::{OpenFile, WorkspaceState};

actions!(
    siesta,
    [
        RunStatement, RunAll, RepeatLast, CancelRun, SaveFile, CloseTab, ToggleSidebar, NextTab, OpenSettings, OpenFolder, Quit,
        PickConnection, ToggleResult, ToggleSplit, GoToFile, GoToStatement, GoToWorkspaceStatement, CommandPalette, PrevStatement,
        NextStatement, FocusEditor, ToggleFilters, RefreshSchema,
        // In the result table ("ResultTable" context) and the value dialog ("ValuePopup").
        CopyCells, OpenCell, OpenRow, NextPage, PrevPage, PopupPrev, PopupNext
    ]
);

/// Shown in an empty workspace (not written until saved).
const SAMPLE: &str = "-- @connection local\nSELECT 1;\n";
const CONN_TEMPLATE: &str =
    "{\n  \"local\": {\n    \"driver\": \"postgres\",\n    \"host\": \"localhost\",\n    \"database\": \"postgres\",\n    \"user\": \"postgres\"\n  }\n}\n";
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
    /// The workspace folder: root of the file tree, of `siesta*.json` chains and of saved state.
    root: PathBuf,
    /// Where `state.json` lives (`~/.siesta`); `None` = nothing is saved.
    base: Option<PathBuf>,
    docs: Vec<Doc>,
    current: Option<usize>,
    /// The workspace's files (`.sql` and connection files, or all of them).
    files: Vec<PathBuf>,
    show_all_files: bool,
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
    /// `siesta*.json` files of the open tabs' chains at the last check; a change reloads connections.
    conns_seen: Option<Stamp>,
    /// A dirty tab whose close was requested once; the next close discards it.
    close_armed: Option<PathBuf>,
    /// Re-renders the open search dialog (Cmd+P …) as its query changes.
    picker_sub: Option<Subscription>,
    /// Tabs, most recently used first (Ctrl+Tab).
    mru: Vec<PathBuf>,
    /// Ctrl+Tab held down: the MRU order when it started and how far it has gone.
    tab_cycle: Option<(Vec<PathBuf>, usize)>,
    /// Sessions per (file, connection), opened by the first run and closed with the file.
    slots: HashMap<(PathBuf, String), runner::Slot>,
    /// Results per (file, statement text).
    store: results::Store,
    /// The result panel's table, filter fields and pin.
    ui: results::Ui,
    /// The "Running…" clock is ticking.
    ticking: bool,
    settings: siesta_settings::Loaded,
    settings_window: Option<WindowHandle<Root>>,
    /// Holds focus when nothing else does (no tabs), so shortcuts still reach the "Siesta" context.
    focus: FocusHandle,
    /// Theme's own monospace font, used when `font_family` is empty.
    default_mono: SharedString,
    _subs: Vec<Subscription>,
}

/// `path` or `path:line` (1-based). A real file named `a:3` wins over the suffix.
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
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        for e in &settings.errors {
            notify_later(window, cx, format!("Settings: {e}"));
        }
        let sidebar_filter = cx.new(|cx| InputState::new(window, cx).placeholder("Search statements"));
        let db_filter = cx.new(|cx| InputState::new(window, cx).placeholder("Search tables and columns"));
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
        let mut this = Self {
            root: root.clone(),
            base,
            docs: vec![],
            current: None,
            files: vec![],
            show_all_files: false,
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
            focus: cx.focus_handle(),
            default_mono: cx.theme().mono_font_family.clone(),
            _subs,
        };
        this.load_workspace(open, window, cx);
        this.apply_settings(window, cx);
        this
    }

    /// Scan the root and bring back the saved tabs; `open` (a launch argument) ends up in front.
    /// Without any file, a sample is offered (and written only on save).
    fn load_workspace(&mut self, open: Option<(PathBuf, Option<u32>)>, window: &mut Window, cx: &mut Context<Self>) {
        self.expanded = HashSet::new();
        self.rescan();
        let state = self.base.as_deref().map(|b| siesta_workspace::load_state(b, &self.root)).unwrap_or_default();
        self.picks = state.connections;
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
                    None => self.open(self.root.join("untitled.sql"), Some(SAMPLE.into()), window, cx),
                };
            }
            None => {}
        }
    }

    fn rescan(&mut self) {
        self.files = siesta_workspace::scan(&self.root, self.show_all_files);
    }

    pub(crate) fn toggle_all_files(&mut self, cx: &mut Context<Self>) {
        self.show_all_files = !self.show_all_files;
        self.rescan();
        cx.notify();
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

    fn set_root(&mut self, root: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        if self.docs.iter().any(|d| d.dirty) {
            window.push_notification("Save or close the unsaved files before opening another folder", cx);
            return;
        }
        self.remember(cx);
        self.docs.clear();
        self.current = None;
        self.mru.clear();
        self.outlines.clear();
        self.collapsed_dirs.clear();
        self.conns_seen = None;
        self.settings = siesta_settings::Loaded::load(Some(&root));
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

    /// Where the connection chain of `path` may have files: `siesta.json` / `siesta.private.json`
    /// from the root down to its folder, plus `connections.files`.
    fn conn_candidates(&self, path: &Path) -> Vec<PathBuf> {
        let root = self.root_of(path);
        let dir = path.parent().unwrap_or(&root);
        let mut dirs: Vec<&Path> = dir.ancestors().take_while(|d| d.starts_with(&root)).collect();
        dirs.reverse();
        let mut out: Vec<PathBuf> = dirs.iter().flat_map(|d| [d.join("siesta.json"), d.join("siesta.private.json")]).collect();
        out.extend(self.settings.settings.connection_files(&root));
        out
    }

    fn conns_stamp(&self) -> Stamp {
        let mut out: Stamp = self.docs.iter().flat_map(|d| self.conn_candidates(&d.path)).map(|p| (p.clone(), doc::mtime(&p))).collect();
        out.sort();
        out.dedup();
        out
    }

    fn load_connections(&self, path: &Path) -> Connections {
        Connections::load(path, &self.root_of(path), &self.settings.settings.connection_files(&self.root_of(path)))
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
        for ix in 0..self.docs.len() {
            let c = self.load_connections(&self.docs[ix].path.clone());
            if ix == self.current.unwrap_or(0) {
                self.report_connections(&c, window, cx);
            }
            self.docs[ix].conns = Arc::new(c);
            self.docs[ix].reparse(cx);
        }
        cx.notify();
    }

    /// Re-read the settings files when they change on disk (edited by hand, another window).
    // ponytail: polls mtimes every second (with the open files), as Respite does; `notify` if this ever shows up in a profile.
    fn start_watch(&self, window: &mut Window, cx: &mut Context<Self>) {
        let stamp = |l: &siesta_settings::Loaded| {
            [&l.global_path, &l.workspace_path, &keymap_path(l)].map(|p| p.as_ref().and_then(|p| std::fs::metadata(p).and_then(|m| m.modified()).ok()))
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
                    let fresh = siesta_settings::Loaded::load_from(this.settings.global_path.clone(), this.settings.workspace_path.clone());
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
                    cx.subscribe(&editor, |this, editor, ev: &InputEvent, cx| {
                        if let (InputEvent::Change, Some(d)) = (ev, this.docs.iter_mut().find(|d| d.editor == editor)) {
                            d.dirty = true;
                            d.reparse(cx);
                            // `-- @connection` may have just changed.
                            if let Some(key) = d.schema_key() {
                                this.load_cached_schema(&key);
                            }
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
                let conns = self.load_connections(&path);
                self.report_connections(&conns, window, cx);
                let picked = self.picks.get(&path).cloned().flatten();
                self.docs.push(Doc::new(path.clone(), editor, Arc::new(conns), picked, self.schemas.clone(), subs, cx));
                let ix = self.docs.len() - 1;
                if let Some(key) = self.docs[ix].schema_key() {
                    self.load_cached_schema(&key);
                }
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

    /// Root of `path`'s connection chain: the workspace, or (for a file outside it) its own repo.
    fn root_of(&self, path: &Path) -> PathBuf {
        if path.starts_with(&self.root) { self.root.clone() } else { db_conn::workspace_root(path) }
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
                if siesta_workspace::is_conn_file(&path) {
                    // New connections apply to every open file right away.
                    self.reload_connections(window, cx);
                    self.conns_seen = Some(self.conns_stamp());
                }
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
        let state = WorkspaceState { open, current: self.doc().map(|d| (*d.path).clone()), connections: self.picks.clone() };
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
    /// `-- @connection` in the text still wins.
    pub(crate) fn select_connection(&mut self, name: Option<String>, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.current else { return };
        self.picks.insert((*self.docs[ix].path).clone(), name.clone());
        self.docs[ix].picked = name;
        self.docs[ix].reparse(cx);
        if let Some(key) = self.docs[ix].schema_key() {
            self.load_cached_schema(&key);
        }
        self.remember(cx);
        cx.notify();
    }

    /// Red when the file's connection looks like production or is read-only.
    fn banner(&self, cx: &App) -> Option<AnyElement> {
        let d = self.doc()?;
        let name = d.connection()?;
        let prod = self.settings.settings.connections.is_prod(name);
        let read_only = d.conns.connections.get(name).is_some_and(|c| c.read_only);
        let what = match (prod, read_only) {
            (true, true) => "production, read-only",
            (true, false) => "production",
            (false, true) => "read-only",
            (false, false) => return None,
        };
        let t = cx.theme();
        Some(
            div()
                .flex_none()
                .px_3()
                .py_1()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .bg(t.danger)
                .text_color(t.danger_foreground)
                .child(format!("{name}: {what} connection"))
                .into_any_element(),
        )
    }

    /// Top-right connection picker: the connections of the current file's chain, "No connection",
    /// and the connection files to edit. The dot is grey until sessions exist (green / red later).
    fn connection_picker(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let d = self.doc().filter(|d| d.sql)?;
        let current = d.connection().map(str::to_string);
        let forced = d.file.connection.is_some();
        let names: Vec<String> = d.conns.names().iter().map(|n| n.to_string()).collect();
        let prod = current.as_deref().is_some_and(|n| self.settings.settings.connections.is_prod(n));
        let status = self.dot_of(&d.path, current.as_deref());
        let (can_test, can_disconnect) = (current.is_some(), self.slots.keys().any(|(p, _)| *p == *d.path));
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
                .child(h_flex().gap_1p5().child(dot(status_color)).child(label))
                .dropdown_caret(true)
                .small()
                .when(prod, |b| b.danger())
                .when(!prod, |b| b.ghost())
                .dropdown_menu(move |menu, _, cx| {
                    let pick = |name: Option<String>| {
                        let view = view.clone();
                        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                            view.update(cx, |this, cx| this.select_connection(name.clone(), window, cx)).ok();
                        }
                    };
                    // Open a connection file in a tab; `template` creates it first.
                    let edit = |path: PathBuf, template: Option<&'static str>| {
                        let view = view.clone();
                        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
                            if let Some(t) = template
                                && let Err(e) = std::fs::write(&path, t)
                            {
                                window.push_notification(format!("{}: {e}", path.display()), cx);
                                return;
                            }
                            view.update(cx, |this, cx| {
                                if template.is_some() {
                                    this.rescan();
                                }
                                this.open(path.clone(), None, window, cx);
                            })
                            .ok();
                        }
                    };
                    let row = |label: String, name: Option<String>| {
                        let color = cx.theme().muted_foreground.opacity(0.6);
                        let size = cx.theme().font_size * 0.55;
                        PopupMenuItem::element(move |_, _| {
                            h_flex().gap_2().items_center().child(div().flex_none().size(size).rounded_full().bg(color)).child(label.clone())
                        })
                        .checked(current == name)
                        .on_click(pick(name))
                    };
                    let mut menu = menu.item(row("No connection".into(), None)).separator();
                    if names.is_empty() {
                        menu = menu.label("No connections: create siesta.json");
                    }
                    menu = names.iter().fold(menu, |menu, n| menu.item(row(n.clone(), Some(n.clone()))));
                    if forced {
                        menu = menu.separator().label("Set by -- @connection in the file");
                    }
                    // Test needs a connection, Disconnect a live session.
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
                    let chain = view.upgrade().and_then(|v| {
                        let this = v.read(cx);
                        this.doc().map(|d| (this.conn_candidates(&d.path), this.root.clone()))
                    });
                    let Some((candidates, root)) = chain else { return menu };
                    menu = menu.separator();
                    for p in candidates.iter().filter(|p| p.is_file()) {
                        let shown = p.strip_prefix(&root).unwrap_or(p).display().to_string();
                        menu = menu.item(PopupMenuItem::new(format!("Edit {shown}")).on_click(edit(p.clone(), None)));
                    }
                    let public = root.join("siesta.json");
                    if !public.exists() {
                        menu = menu.item(PopupMenuItem::new("Create siesta.json in the workspace").on_click(edit(public, Some(CONN_TEMPLATE))));
                    }
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
            .into_any_element()
    }

    fn center(&mut self, focused: bool, cx: &mut Context<Self>) -> AnyElement {
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
            .on_action(cx.listener(|this, _: &OpenSettings, window, cx| this.open_settings(window, cx)))
            .on_action(cx.listener(|this, _: &ToggleSidebar, _, cx| {
                this.sidebar = !this.sidebar;
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &NextTab, window, cx| this.next_tab(window, cx)))
            .on_action(cx.listener(|this, _: &ToggleResult, _, cx| {
                this.result_open = !this.result_open;
                cx.notify();
            }))
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
            MenuItem::separator(),
            MenuItem::os_submenu("Services", SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action("Quit Siesta", Quit),
        ]),
        Menu::new("File").items([
            MenuItem::action("Open Folder…", OpenFolder),
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
        ("Toggle Split Orientation", Box::new(ToggleSplit)),
        ("Toggle Sidebar", Box::new(ToggleSidebar)),
        ("Next Tab", Box::new(NextTab)),
        ("Save", Box::new(SaveFile)),
        ("Close Tab", Box::new(CloseTab)),
        ("Open Folder…", Box::new(OpenFolder)),
        ("Settings…", Box::new(OpenSettings)),
        ("Quit Siesta", Box::new(Quit)),
    ]
}

/// `siesta [path[:line]]`: a folder opens as the workspace, a file opens in its repo's workspace;
/// no argument reopens the last workspace.
fn main() -> std::process::ExitCode {
    if let Some(code) = siesta_cli::main_if_cli() {
        return code;
    }
    // ponytail: hand-rolled GUI args; fold into clap when the GUI grows options.
    // `-psn_…` is a process serial number some macOS launches still pass to app bundles.
    let target = std::env::args().skip(1).rfind(|a| a != "open" && !a.starts_with("-psn_"));
    let cwd = std::env::current_dir().expect("current dir");
    let base = siesta_workspace::base_dir();
    let (root, open) = match target.map(|t| parse_target(&t)).map(|(p, l)| (std::path::absolute(&p).unwrap_or_else(|_| cwd.join(&p)), l)) {
        Some((p, _)) if p.is_dir() => (p, None),
        Some((p, line)) => (db_conn::workspace_root(&p), Some((p, line))),
        // No target: the last workspace, else a scratch one (cwd is `/` from Finder, `target/…` from cargo).
        None => {
            let root = base.as_deref().and_then(siesta_workspace::last_root).or_else(|| base.as_ref().map(|b| b.join("workspace")));
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
                let settings = siesta_settings::Loaded::load(Some(&root));
                let v = cx.new(|cx| Siesta::new(root, open, base, settings, window, cx));
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

    /// A workspace with two connections (`dev` Postgres, `my` MySQL) and a few files.
    fn workspace() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        std::fs::write(r.join("siesta.json"), r#"{"dev": {"driver": "postgres", "database": "d"}, "my": {"driver": "mysql", "read_only": true}}"#).unwrap();
        std::fs::write(r.join("a.sql"), "SELECT 1;\n\nSELECT 2;\n\n-- @name third\nSELECT 3;\n").unwrap();
        std::fs::write(r.join("b.sql"), "SELECT 'b';\n").unwrap();
        std::fs::write(r.join("c.sql"), "-- @connection my\nSELECT 'c';\n").unwrap();
        dir
    }

    fn open_app(cx: &mut TestAppContext, root: &Path, base: &Path, open: Option<PathBuf>) -> (Entity<Siesta>, VisualTestContext) {
        if !cx.update(|cx| cx.has_global::<gpui_component::Theme>()) {
            cx.update(gpui_component::init);
        }
        let mut view = None;
        let window = cx.update(|cx| {
            cx.open_window(Default::default(), |window, cx| {
                let settings = siesta_settings::Loaded::load_from(None, None);
                let v = cx.new(|cx| Siesta::new(root.to_path_buf(), open.map(|p| (p, None)), Some(base.to_path_buf()), settings, window, cx));
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
        assert_eq!(text_of(&app, &mut cx), "SELECT 1;\nSELECT * FROM users;\n\nSELECT 2;\n\n-- @name third\nSELECT 3;\n");
        let active = cx.update(|_, cx| {
            let d = app.read(cx).doc().unwrap();
            let src = d.editor.read(cx).value();
            d.active_statement().map(|s| (s.line, s.text(&src).to_string()))
        });
        assert_eq!(active, Some((2, "SELECT * FROM users".to_string())));
        assert_eq!(cursor_line(&app, &mut cx), 1);
        // Outside the default schema the name is qualified; a statement without `;` gets one.
        cx.update(|window, cx| {
            app.update(cx, |this, cx| {
                this.docs[0].editor.update(cx, |s, cx| s.set_value("SELECT 1", window, cx));
                this.docs[0].reparse(cx);
                assert!(this.insert_select("analytics", "events", window, cx));
            })
        });
        assert_eq!(text_of(&app, &mut cx), "SELECT 1;\nSELECT * FROM analytics.events;");
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
}
