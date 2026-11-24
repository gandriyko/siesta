//! Sidebar: tabs Files | DB. Files, as in Respite: the central folder (`~/.config/siesta/collections`:
//! folders → `.sql` files → statements), then the `.sql` files added from elsewhere.
//!
//! Styled after the VS Code / Zed project panels: disclosure chevrons, indent guides,
//! compact single-child folders (`api/v2`), inset rounded selection and muted meta on the right.

use std::{
    collections::{BTreeMap, HashMap},
    path::{Path, PathBuf},
};

use gpui::{prelude::FluentBuilder as _, *};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    h_flex,
    input::{Input, InputEvent, InputState},
    notification::Notification,
    tab::{Tab, TabBar},
};

use crate::{Siesta, SideTab};

pub(crate) const ROW_H: f32 = 24.;
/// Statements of one file shown at once; more scroll inside the file's own block.
pub(crate) const FILE_STATEMENTS: usize = 20;
pub(crate) const INDENT: f32 = 14.;
/// Left padding before the first chevron.
pub(crate) const PAD: f32 = 6.;
/// Chevron column width; leaves get a spacer of the same width.
pub(crate) const CHEVRON: f32 = 14.;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Section {
    /// `~/.config/siesta/collections`
    Collections,
    /// Added from elsewhere.
    Files,
}

pub(crate) enum Row {
    Section(Section),
    Dir { path: PathBuf, name: String, depth: usize },
    /// `added`: in the Files section (× drops it from the list).
    File { path: PathBuf, name: String, depth: usize, statements: usize, added: bool },
    Statement { path: PathBuf, line: usize, label: String, depth: usize },
    Note { text: &'static str, depth: usize },
}

impl Row {
    fn depth(&self) -> usize {
        match self {
            Row::Section(_) => 0,
            Row::Dir { depth, .. } | Row::File { depth, .. } | Row::Statement { depth, .. } | Row::Note { depth, .. } => *depth,
        }
    }
}

/// `rows` cut into spans in order: a file's statements longer than [`FILE_STATEMENTS`] with that
/// file's path (they scroll in their own block), everything else with `None`.
fn long_statement_runs(rows: &[Row]) -> Vec<(std::ops::Range<usize>, Option<&Path>)> {
    let mut out: Vec<(std::ops::Range<usize>, Option<&Path>)> = vec![];
    let mut ix = 0;
    while ix < rows.len() {
        let Row::Statement { path, .. } = &rows[ix] else {
            ix += 1;
            continue;
        };
        let end = ix + rows[ix..].iter().take_while(|r| matches!(r, Row::Statement { path: p, .. } if p == path)).count();
        if end - ix > FILE_STATEMENTS {
            out.push((ix..end, Some(path)));
        }
        ix = end;
    }
    // Fill the gaps between the long runs with plain spans.
    let mut all = vec![];
    let mut at = 0;
    for (span, path) in out {
        if at < span.start {
            all.push((at..span.start, None));
        }
        at = span.end;
        all.push((span, path));
    }
    if at < rows.len() {
        all.push((at..rows.len(), None));
    }
    all
}

/// Folder tree of the central folder.
#[derive(Default)]
struct Tree {
    dirs: BTreeMap<String, Tree>,
    files: Vec<PathBuf>,
}

impl Tree {
    /// `entries`: folders and `.sql` files under `root` (folders make empty nodes).
    fn build(root: &Path, entries: &[PathBuf]) -> Self {
        let mut tree = Tree::default();
        for e in entries {
            let rel = e.strip_prefix(root).unwrap_or(e);
            let is_dir = !siesta_workspace::is_sql(e);
            let parent = if is_dir { Some(rel) } else { rel.parent() };
            let dirs = parent.into_iter().flat_map(|p| p.components()).map(|c| c.as_os_str().to_string_lossy().into_owned());
            let node = dirs.fold(&mut tree, |node, d| node.dirs.entry(d).or_default());
            if !is_dir {
                node.files.push(e.clone());
            }
        }
        tree
    }
}

/// Import, new file, new folder, `+`, `×`, Trash … on a row or a section header: shown on hover,
/// or always; `tip` on hover.
fn row_action(id: impl Into<ElementId>, child: impl IntoElement, tip: &'static str, always: bool, cx: &App) -> Stateful<Div> {
    div()
        .id(id)
        .tooltip(move |window, cx| gpui_component::tooltip::Tooltip::new(tip).build(window, cx))
        .flex_none()
        .p_0p5()
        .rounded_sm()
        .text_color(cx.theme().muted_foreground)
        .when(!always, |d| d.invisible().group_hover("row", |s| s.visible()))
        .hover(|s| s.bg(cx.theme().list_active))
        .child(child)
}

/// `p` with its leading `from` replaced by `to`; any other path as it is.
fn moved(p: &Path, from: &Path, to: &Path) -> PathBuf {
    match p.strip_prefix(from) {
        Ok(rest) if rest.as_os_str().is_empty() => to.to_path_buf(),
        Ok(rest) => to.join(rest),
        Err(_) => p.to_path_buf(),
    }
}

/// `name` as a `.sql` file name: the extension is appended unless it is already there (a dot
/// inside the name, as in `report.2024`, is part of the name, not an extension).
fn sql_name(name: &str) -> String {
    if siesta_workspace::is_sql(Path::new(name)) { name.to_string() } else { format!("{name}.sql") }
}

/// Where `from` goes when renamed to `input`: the same folder, the name trimmed; a file gets
/// `.sql` when it lacks it (as New File does). Names with a path separator or empty ones are refused.
fn renamed_path(from: &Path, input: &str, is_dir: bool) -> Result<PathBuf, String> {
    let name = input.trim();
    if name.is_empty() || name.contains(['/', '\\']) {
        return Err(format!("Invalid name: {name}"));
    }
    Ok(from.with_file_name(if is_dir { name.to_string() } else { sql_name(name) }))
}

/// `to` is `from` under a name that differs in case only (the same file on a case-insensitive
/// disk, where `to.exists()` is true), so renaming onto it overwrites nothing.
fn case_only_rename(from: &Path, to: &Path) -> bool {
    name_of(from).eq_ignore_ascii_case(&name_of(to)) && same_file(from, to)
}

fn same_file(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        matches!((std::fs::metadata(a), std::fs::metadata(b)), (Ok(x), Ok(y)) if (x.dev(), x.ino()) == (y.dev(), y.ino()))
    }
    #[cfg(not(unix))]
    {
        matches!((a.canonicalize(), b.canonicalize()), (Ok(x), Ok(y)) if x == y)
    }
}

fn name_of(p: &Path) -> String {
    p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned())
}

impl Siesta {
    /// `(line, label)` of a file's statements: live split when open, else read once.
    pub(crate) fn outline_of(&mut self, path: &Path) -> Vec<(usize, String)> {
        if !siesta_workspace::is_sql(path) {
            return vec![];
        }
        match self.docs.iter().find(|d| *d.path == *path) {
            Some(d) => d.outline.clone(),
            None => self
                .outlines
                .entry(path.to_path_buf())
                .or_insert_with(|| {
                    let src = std::fs::read_to_string(path).unwrap_or_default();
                    crate::doc::outline(&src, &sql_split::split(&src, sql_split::Dialect::default()))
                })
                .clone(),
        }
    }

    /// `+` in the Files section (File → Add Files…, Cmd+O): `.sql` files from anywhere, several at once.
    pub(crate) fn add_files(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: true, prompt: Some("Add".into()) });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else { return };
            this.update_in(cx, |this, window, cx| this.add_paths(paths, window, cx)).ok();
        })
        .detach();
    }

    /// Lists and opens the `.sql` files among `paths` (the `+` dialog, or files dropped on the
    /// window); the rest are skipped with a notice.
    pub(crate) fn add_paths(&mut self, paths: impl IntoIterator<Item = PathBuf>, window: &mut Window, cx: &mut Context<Self>) {
        // ponytail: the system dialog has no extension filter here; other files are skipped after.
        let (ok, skipped): (Vec<PathBuf>, Vec<PathBuf>) = paths.into_iter().partition(|p| p.is_file() && siesta_workspace::is_sql(p));
        if !skipped.is_empty() {
            let names: Vec<String> = skipped.iter().map(|p| name_of(p)).collect();
            let title = if skipped.len() == 1 { "Not a query file" } else { "Not query files" };
            window.push_notification(Notification::warning(format!("{}\nOnly .sql, .es and .mongodb.js files can be added.", names.join(", "))).title(title), cx);
        }
        let first = ok.first().cloned();
        for p in ok {
            self.add_file(p);
        }
        if let Some(p) = first {
            self.open(p, None, window, cx);
        }
        cx.notify();
    }

    /// List `path` in the Files section, unless it's there already or under the central folder.
    pub(crate) fn add_file(&mut self, path: PathBuf) {
        if self.config_dir.as_ref().is_some_and(|c| path.starts_with(c)) || self.added.contains(&path) {
            return;
        }
        self.added.push(path);
        self.save_added();
        self.rescan();
    }

    /// Drops the file from the list only; the file, its tab and its history stay.
    pub(crate) fn remove_file(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        self.added.retain(|p| p != path);
        self.save_added();
        self.rescan();
        window.push_notification(format!("Removed {} from the list. The file and its history are kept.", name_of(path)), cx);
        cx.notify();
    }

    fn save_added(&self) {
        if let Some(base) = &self.base {
            siesta_workspace::save_files(base, &self.added);
        }
    }

    /// File → Import…: into the central folder.
    pub(crate) fn import(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.import_into(self.folder(), window, cx);
    }

    /// Several `.sql` files at once, copied into `dir` (a name already there is skipped, never
    /// overwritten); the first one opens.
    pub(crate) fn import_into(&mut self, dir: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(PathPromptOptions { files: true, directories: false, multiple: true, prompt: Some("Import".into()) });
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else { return };
            this.update_in(cx, |this, window, cx| this.import_paths(paths, &dir, window, cx)).ok();
        })
        .detach();
    }

    /// Copies the `.sql` files among `paths` into `dir`; returns what was copied.
    pub(crate) fn import_paths(&mut self, paths: Vec<PathBuf>, dir: &Path, window: &mut Window, cx: &mut Context<Self>) -> Vec<PathBuf> {
        let mut copied = vec![];
        for src in paths {
            let name = name_of(&src);
            let dest = dir.join(&name);
            let result = if !siesta_workspace::is_sql(&src) {
                Err("only .sql files can be imported".to_string())
            } else if dest.exists() {
                Err("a file with this name is already there".to_string())
            } else {
                std::fs::create_dir_all(dir).and_then(|()| std::fs::copy(&src, &dest)).map_err(|e| e.to_string())
            };
            match result {
                Ok(_) => copied.push(dest),
                Err(e) => window.push_notification(Notification::warning(format!("{name}: {e}")).title("Not imported"), cx),
            }
        }
        self.collapsed_dirs.remove(dir);
        self.rescan();
        if let Some(f) = copied.first() {
            self.open(f.clone(), None, window, cx);
        }
        cx.notify();
        copied
    }

    /// A new `.sql` file in `dir`, named in an in-app dialog.
    pub(crate) fn new_file(&mut self, dir: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let dir = dir.to_path_buf();
        self.ask_name("New File", "query", "Create", window, cx, move |this, name, window, cx| {
            this.create_file(&dir, name, window, cx);
        });
    }

    /// `name` (`.sql` added when missing) in `dir`, made empty unless it exists (never truncated),
    /// then opened; `None` when it can't be written.
    pub(crate) fn create_file(&mut self, dir: &Path, name: &str, window: &mut Window, cx: &mut Context<Self>) -> Option<PathBuf> {
        let path = dir.join(sql_name(name));
        if !path.exists()
            && let Err(e) = std::fs::create_dir_all(dir).and_then(|()| std::fs::write(&path, ""))
        {
            window.push_notification(format!("{}: {e}", path.display()), cx);
            return None;
        }
        self.rescan();
        self.open(path.clone(), None, window, cx);
        cx.notify();
        Some(path)
    }

    /// A new folder in `dir`, named in an in-app dialog.
    pub(crate) fn new_folder(&mut self, dir: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let dir = dir.to_path_buf();
        self.ask_name("New Folder", "New Folder", "Create", window, cx, move |this, name, window, cx| {
            let path = dir.join(name);
            if let Err(e) = std::fs::create_dir_all(&path) {
                window.push_notification(format!("{}: {e}", path.display()), cx);
                return;
            }
            this.collapsed_dirs.remove(&path);
            this.rescan();
            cx.notify();
        });
    }

    /// Not the system save dialog: macOS won't open it inside the hidden `~/.config`, so it
    /// started wherever it was last. Enter or the button applies; a name with `/` is refused.
    fn ask_name(
        &mut self,
        title: &'static str,
        default: &str,
        ok: &'static str,
        window: &mut Window,
        cx: &mut Context<Self>,
        apply: impl Fn(&mut Self, &str, &mut Window, &mut Context<Self>) + 'static,
    ) {
        let input = cx.new(|cx| InputState::new(window, cx).default_value(default.to_string()));
        let this = cx.entity().downgrade();
        let apply = std::rc::Rc::new(apply);
        let apply = std::rc::Rc::new({
            let input = input.clone();
            move |window: &mut Window, cx: &mut App| {
                let name = input.read(cx).value().trim().to_string();
                if name.is_empty() || name.starts_with('.') || name.contains(['/', '\\']) {
                    window.push_notification(format!("Invalid name: {name}"), cx);
                    return;
                }
                window.close_dialog(cx);
                // Enter arrives through a subscription of `Siesta`, which is mid-update then.
                let (this, apply) = (this.clone(), apply.clone());
                window.defer(cx, move |window, cx| {
                    this.update(cx, |this, cx| apply(this, &name, window, cx)).ok();
                });
            }
        });
        self.picker_sub = Some(cx.subscribe_in(&input, window, {
            let apply = apply.clone();
            move |_, _, ev: &InputEvent, window, cx| {
                if matches!(ev, InputEvent::PressEnter { .. }) {
                    apply(window, cx);
                }
            }
        }));
        let field = input.clone();
        window.open_dialog(cx, move |dialog, _, _| {
            let apply = apply.clone();
            dialog.title(title).w(px(420.)).child(Input::new(&field)).footer(
                h_flex()
                    .w_full()
                    .justify_end()
                    .gap_2()
                    .child(Button::new("name-cancel").label("Cancel").on_click(|_, window, cx| window.close_dialog(cx)))
                    .child(Button::new("name-ok").label(ok).primary().on_click(move |_, window, cx| apply(window, cx))),
            )
        });
        input.update(cx, |s, cx| {
            s.focus(window, cx);
            s.select_all(window, cx);
        });
    }

    /// The pencil on a row and File → Rename File…: asks for the new name of a file or folder.
    pub(crate) fn rename(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let (title, ok) = if path.is_dir() { ("Rename Folder", "Rename") } else { ("Rename File", "Rename") };
        // A file is shown without `.sql`: it is always added back.
        let name = if path.is_dir() { name_of(&path) } else { path.file_stem().map_or_else(|| name_of(&path), |n| n.to_string_lossy().into_owned()) };
        self.ask_name(title, &name, ok, window, cx, move |this, name, window, cx| {
            this.rename_path(&path, name, window, cx);
        });
    }

    /// Renames the file or folder `from` (its new name is `input`; a file gets `.sql` when it
    /// lacks it) and moves everything of the app that is keyed by its path along. Refused, with a
    /// notification, when the target exists (never overwritten; a case-only rename of the same
    /// file is fine), or an affected tab has an open transaction or a running statement.
    /// `from` that is not on disk yet (the untitled sample) only changes the tab's path.
    pub(crate) fn rename_path(&mut self, from: &Path, input: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let on_disk = from.exists();
        let to = match renamed_path(from, input, from.is_dir()) {
            Ok(to) => to,
            Err(e) => {
                window.push_notification(e, cx);
                return false;
            }
        };
        if to == from {
            return false;
        }
        if to.exists() && !case_only_rename(from, &to) {
            window.push_notification(Notification::warning(format!("{} already exists.", name_of(&to))).title("Not renamed"), cx);
            return false;
        }
        let affected: Vec<PathBuf> = self.docs.iter().filter(|d| d.path.starts_with(from)).map(|d| (*d.path).clone()).collect();
        if let Some(p) = affected.iter().find(|p| self.in_tx(p)) {
            window.push_notification(format!("End the transaction in {} first.", name_of(p)), cx);
            return false;
        }
        if let Some(k) = self.store.entries.iter().find(|(k, e)| k.0.starts_with(from) && e.running.is_some()).map(|(k, _)| k) {
            window.push_notification(format!("{} is running a statement. Stop it first.", name_of(&k.0)), cx);
            return false;
        }
        if on_disk && let Err(e) = std::fs::rename(from, &to) {
            window.push_notification(format!("Couldn't rename {}: {e}", name_of(from)), cx);
            return false;
        }
        for p in &affected {
            // The next run reconnects under the new name.
            self.close_sessions(p, None);
        }
        self.rekey(from, &to);
        self.repoint_history(from, &to);
        self.rescan();
        self.remember(cx);
        cx.notify();
        true
    }

    /// Everything keyed by a path under `from` follows it to `to`: tabs (in place, so the order
    /// and the front tab stay), results, outlines, expanded / collapsed rows, the added files.
    fn rekey(&mut self, from: &Path, to: &Path) {
        let mv = |p: &Path| moved(p, from, to);
        for d in self.docs.iter_mut().filter(|d| d.path.starts_with(from)) {
            d.path = std::sync::Arc::new(mv(&d.path));
            d.mtime = crate::doc::mtime(&d.path);
        }
        let entries = std::mem::take(&mut self.store.entries);
        self.store.entries = entries.into_iter().map(|((p, s), e)| ((mv(&p), s), e)).collect();
        let batches = std::mem::take(&mut self.store.batches);
        self.store.batches = batches.into_iter().map(|(p, keys)| (mv(&p), keys.into_iter().map(|(p, s)| (mv(&p), s)).collect())).collect();
        if let Some((p, _)) = &mut self.store.last {
            *p = mv(p);
        }
        if let Some(k) = &mut self.ui.pinned {
            k.0 = mv(&k.0);
        }
        if let Some(e) = &mut self.ui.edit {
            e.key.0 = mv(&e.key.0);
        }
        self.outlines = std::mem::take(&mut self.outlines).into_iter().map(|(p, v)| (mv(&p), v)).collect();
        self.expanded = std::mem::take(&mut self.expanded).into_iter().map(|p| mv(&p)).collect();
        self.collapsed_dirs = std::mem::take(&mut self.collapsed_dirs).into_iter().map(|p| mv(&p)).collect();
        for p in self.mru.iter_mut().chain(self.close_armed.iter_mut()).chain(self.tab_cycle.iter_mut().flat_map(|(v, _)| v.iter_mut())) {
            *p = mv(p);
        }
        if self.added.iter().any(|p| p.starts_with(from)) {
            self.added = self.added.iter().map(|p| mv(p)).collect();
            self.save_added();
        }
    }

    /// History entries keep the file they ran in: point them at the new name.
    fn repoint_history(&mut self, from: &Path, to: &Path) {
        let Some(store) = self.base.as_deref().and_then(|b| db_exec::history::Store::open(b).ok()) else { return };
        for mut e in store.list().into_iter().filter(|e| Path::new(&e.file).starts_with(from)) {
            e.file = moved(Path::new(&e.file), from, to).to_string_lossy().into_owned();
            if let Ok(json) = serde_json::to_vec_pretty(&e) {
                let _ = std::fs::write(store.dir().join(format!("{}.json", e.id)), json);
            }
        }
        if self.show_history {
            self.reload_history();
        }
    }

    fn toggle(&mut self, key: &Path) {
        if !self.expanded.remove(key) {
            self.expanded.insert(key.to_path_buf());
        }
    }

    fn toggle_dir(&mut self, path: &Path) {
        if !self.collapsed_dirs.remove(path) {
            self.collapsed_dirs.insert(path.to_path_buf());
        }
    }

    pub(crate) fn build_rows(&mut self, query: &str) -> Vec<Row> {
        let root = self.folder();
        let central = self.central.clone();
        let mut rows = vec![Row::Section(Section::Collections)];
        let mut found = vec![];
        self.emit(&Tree::build(&root, &central), &root, 0, query, &mut found);
        if found.is_empty() && query.is_empty() {
            rows.push(Row::Note { text: "No files yet: the buttons above make or import one", depth: 0 });
        }
        rows.extend(found);
        rows.push(Row::Section(Section::Files));
        let mut found = vec![];
        for f in self.added.clone() {
            self.emit_file(&f, 0, query, true, &mut found);
        }
        if found.is_empty() && query.is_empty() {
            rows.push(Row::Note { text: "+ adds .sql / .es / .mongodb.js files from anywhere", depth: 0 });
        }
        rows.extend(found);
        rows
    }

    fn emit(&mut self, tree: &Tree, base: &Path, depth: usize, query: &str, rows: &mut Vec<Row>) {
        let searching = !query.is_empty();
        for (name, node) in &tree.dirs {
            // Compact folders: a chain of single-child folders shows as `a/b/c`.
            let (mut node, mut path, mut label) = (node, base.join(name), name.clone());
            while node.files.is_empty() && node.dirs.len() == 1 {
                let (child, next) = node.dirs.iter().next().unwrap();
                (node, path, label) = (next, path.join(child), format!("{label}/{child}"));
            }
            if searching {
                let mut found = vec![];
                self.emit(node, &path, depth + 1, query, &mut found);
                if !found.is_empty() {
                    rows.push(Row::Dir { path: path.clone(), name: label, depth });
                    rows.extend(found);
                }
                continue;
            }
            rows.push(Row::Dir { path: path.clone(), name: label, depth });
            if !self.collapsed_dirs.contains(&path) {
                self.emit(node, &path, depth + 1, query, rows);
            }
        }
        for f in &tree.files {
            self.emit_file(f, depth, query, false, rows);
        }
    }

    fn emit_file(&mut self, f: &Path, depth: usize, query: &str, added: bool, rows: &mut Vec<Row>) {
        let outline = self.outline_of(f);
        let (name, path) = (name_of(f), f.to_path_buf());
        if !query.is_empty() {
            // A file-name match keeps all its statements; otherwise only the matching ones.
            let file_hit = name.to_lowercase().contains(query);
            let hits: Vec<_> = outline.into_iter().filter(|(_, l)| file_hit || l.to_lowercase().contains(query)).collect();
            if hits.is_empty() && !file_hit {
                return;
            }
            rows.push(Row::File { path: path.clone(), name, depth, statements: hits.len(), added });
            rows.extend(hits.into_iter().map(|(line, label)| Row::Statement { path: path.clone(), line, label, depth: depth + 1 }));
            return;
        }
        rows.push(Row::File { path: path.clone(), name, depth, statements: outline.len(), added });
        if self.expanded.contains(&path) {
            rows.extend(outline.into_iter().map(|(line, label)| Row::Statement { path: path.clone(), line, label, depth: depth + 1 }));
        }
    }

    /// Import / new file / new folder buttons for `dir` (a folder row or the Collections header).
    fn folder_actions(&self, ix: usize, dir: &Path, always: bool, cx: &mut Context<Self>) -> [Stateful<Div>; 3] {
        let size = cx.theme().font_size * 0.95;
        let (d1, d2, d3) = (dir.to_path_buf(), dir.to_path_buf(), dir.to_path_buf());
        [
            row_action(("import", ix), Icon::default().data(include_bytes!("../assets/file-down.svg")).size(size), "Import Files…", always, cx).on_click(
                cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.import_into(d3.clone(), window, cx);
                }),
            ),
            row_action(("new-file", ix), Icon::default().data(include_bytes!("../assets/file-plus.svg")).size(size), "New File", always, cx).on_click(
                cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.new_file(&d1, window, cx);
                }),
            ),
            row_action(("new-folder", ix), Icon::default().data(include_bytes!("../assets/folder-plus.svg")).size(size), "New Folder", always, cx).on_click(
                cx.listener(move |this, _, window, cx| {
                    cx.stop_propagation();
                    this.new_folder(&d2, window, cx);
                }),
            ),
        ]
    }

    /// Trash button on a file or folder of the central folder.
    fn trash_action(&self, ix: usize, path: &Path, cx: &mut Context<Self>) -> Stateful<Div> {
        let size = cx.theme().font_size * 0.95;
        let p = path.to_path_buf();
        row_action(("trash", ix), Icon::default().data(include_bytes!("../assets/trash.svg")).size(size), "Move to Trash", false, cx).on_click(cx.listener(
            move |this, _, window, cx| {
                cx.stop_propagation();
                this.trash(p.clone(), window, cx);
            },
        ))
    }

    /// Pencil on a file or folder row: rename it.
    fn rename_action(&self, ix: usize, path: &Path, cx: &mut Context<Self>) -> Stateful<Div> {
        let size = cx.theme().font_size * 0.95;
        let p = path.to_path_buf();
        row_action(("rename", ix), Icon::default().data(include_bytes!("../assets/pencil.svg")).size(size), "Rename…", false, cx).on_click(cx.listener(move |this, _, window, cx| {
            cx.stop_propagation();
            this.rename(p.clone(), window, cx);
        }))
    }

    /// Asks, then moves a file or a folder of the central folder to the system Trash (it can be
    /// put back from there).
    fn trash(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let name = name_of(&path);
        let (message, detail) = if path.is_dir() {
            (format!("Move the folder “{name}” to the Trash?"), "Everything in it goes too. You can put it back from the Trash.")
        } else {
            (format!("Move “{name}” to the Trash?"), "You can put it back from the Trash.")
        };
        let answer = window.prompt(PromptLevel::Warning, &message, Some(detail), &["Move to Trash", "Cancel"], cx);
        cx.spawn_in(window, async move |this, cx| {
            if answer.await != Ok(0) {
                return;
            }
            this.update_in(cx, |this, window, cx| this.move_to_trash(&path, window, cx)).ok();
        })
        .detach();
    }

    fn move_to_trash(&mut self, path: &Path, window: &mut Window, cx: &mut Context<Self>) {
        // The user already confirmed the Trash, so unsaved edits go with the file; an open
        // transaction still needs a commit or rollback first.
        if let Some(d) = self.docs.iter().find(|d| d.path.starts_with(path) && self.in_tx(&d.path)) {
            window.push_notification(format!("End the transaction in {} first.", d.name()), cx);
            return;
        }
        if let Err(e) = trash::delete(path) {
            window.push_notification(format!("Couldn't move {} to the Trash: {e}", path.display()), cx);
            return;
        }
        while let Some(ix) = self.docs.iter().position(|d| d.path.starts_with(path)) {
            self.close_now(ix, window, cx);
        }
        self.rescan();
        self.remember(cx);
        window.push_notification(format!("Moved {} to the Trash", name_of(path)), cx);
        cx.notify();
    }

    /// Every row; a file's statements beyond [`FILE_STATEMENTS`] go into a block of that height
    /// that scrolls on its own, so one long file doesn't push the rest of the tree away. The block
    /// is a `uniform_list`: only its visible rows are built, as the tree is laid out every frame
    /// (each cursor move redraws the window) and hundreds of statement rows made ↑/↓ stutter.
    // ponytail: the tree outside the blocks is not virtualized; fine for hundreds of rows.
    fn render_rows(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let mut out = vec![];
        for (span, path) in long_statement_runs(&self.rows) {
            match path {
                // Keyed by the file, so its scroll position survives the tree changing around it.
                Some(p) => {
                    let id = ElementId::Name(format!("statements:{}", p.display()).into());
                    let start = span.start;
                    let list = uniform_list(
                        id,
                        span.len(),
                        cx.processor(move |this, range: std::ops::Range<usize>, _, cx| {
                            let range = (start + range.start).min(this.rows.len())..(start + range.end).min(this.rows.len());
                            let statuses = this.statuses_of(range.clone(), cx);
                            range.map(|i| this.render_row(i, &statuses, cx)).collect::<Vec<_>>()
                        }),
                    );
                    out.push(div().flex_none().h(px(FILE_STATEMENTS as f32 * ROW_H)).child(list.size_full()).into_any_element());
                }
                None => {
                    let statuses = self.statuses_of(span.clone(), cx);
                    out.extend(span.map(|i| self.render_row(i, &statuses, cx)));
                }
            }
        }
        out
    }

    /// Run statuses of the files whose statements are among `rows`, one pass per file.
    fn statuses_of(&self, rows: std::ops::Range<usize>, cx: &App) -> HashMap<PathBuf, HashMap<usize, (String, bool)>> {
        let mut out = HashMap::new();
        for r in &self.rows[rows] {
            if let Row::Statement { path, .. } = r
                && !out.contains_key(path)
            {
                out.insert(path.clone(), self.statement_statuses(path, cx));
            }
        }
        out
    }

    fn render_row(&self, ix: usize, statuses: &HashMap<PathBuf, HashMap<usize, (String, bool)>>, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme();
        let (muted, hover, active, guide) = (t.muted_foreground, t.list_hover, t.list_active, t.sidebar_border);
        let row = &self.rows[ix];
        let depth = row.depth();
        // Indent guides: one hairline per ancestor level, centred under its chevron.
        let guides = (0..depth).map(move |i| {
            div().absolute().top_0().bottom_0().left(px(PAD + i as f32 * INDENT + CHEVRON / 2.)).w(px(1.)).bg(guide)
        });
        let base = div()
            .id(("row", ix))
            .relative()
            .w_full()
            .h(px(ROW_H))
            .pl(px(PAD + depth as f32 * INDENT))
            .pr_2()
            .rounded_md()
            .flex()
            .items_center()
            .gap_1p5()
            .text_sm()
            .cursor_pointer()
            .group("row")
            .hover(move |s| s.bg(hover))
            .children(guides);
        let spacer = || div().w(px(CHEVRON)).flex_none();
        let icon = |name: IconName, color: Hsla| Icon::new(name).size_3p5().flex_none().text_color(color);

        // Inset by 4 px on both sides, so selection and hover read as rounded pills.
        let row = match row {
            Row::Section(section) => {
                let title = match section {
                    Section::Collections => "COLLECTIONS",
                    Section::Files => "FILES",
                };
                let head = base
                    .cursor_default()
                    .when(ix > 0, |r| r.mt_2())
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(muted)
                    .child(div().flex_1().child(title));
                match section {
                    Section::Collections => head.children(self.folder_actions(ix, &self.folder(), true, cx)),
                    Section::Files => head.child(
                        row_action("add-files", Icon::new(IconName::Plus).size_3p5(), "Add .sql Files…", true, cx)
                            .on_click(cx.listener(|this, _, window, cx| this.add_files(window, cx))),
                    ),
                }
                .into_any_element()
            }
            Row::Dir { path, name, .. } => {
                let open = !self.collapsed_dirs.contains(path);
                let p = path.clone();
                base.child(chevron(open, muted))
                    .child(icon(if open { IconName::FolderOpen } else { IconName::Folder }, muted))
                    .child(div().flex_1().truncate().child(name.clone()))
                    .children(self.folder_actions(ix, path, false, cx))
                    .child(self.rename_action(ix, path, cx))
                    .child(self.trash_action(ix, path, cx))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_dir(&p);
                        cx.notify();
                    }))
                    .into_any_element()
            }
            Row::File { path, name, statements, added, .. } => {
                let doc = self.docs.iter().position(|d| *d.path == *path);
                let current = doc.is_some() && doc == self.current;
                let dirty = doc.is_some_and(|i| self.docs[i].dirty);
                let open = self.expanded.contains(path);
                let (p, q, r) = (path.clone(), path.clone(), path.clone());
                let toggle = div()
                    .id(("file-chevron", ix))
                    .flex_none()
                    .child(chevron(open, muted))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.toggle(&q);
                        cx.notify();
                    }));
                let meta = if dirty { "●".to_string() } else { statements.to_string() };
                let remove = if *added {
                    row_action(("remove-file", ix), Icon::new(IconName::Close).size_3(), "Remove from the List (the file stays)", false, cx).on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        this.remove_file(&r, window, cx);
                    }))
                } else {
                    self.trash_action(ix, path, cx)
                };
                base.when(current, |r| r.bg(active))
                    .child(toggle)
                    .child(icon(IconName::FileText, if current { cx.theme().foreground } else { muted }))
                    .child(div().flex_1().truncate().child(name.clone()))
                    .child(self.rename_action(ix, path, cx))
                    .child(remove)
                    .child(div().flex_none().text_xs().text_color(muted).child(meta))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        let is_current = this.doc().is_some_and(|d| *d.path == p);
                        if is_current {
                            this.toggle(&p);
                        } else if this.open(p.clone(), None, window, cx).is_some() {
                            this.expanded.insert(p.clone());
                        }
                        cx.notify();
                    }))
                    .into_any_element()
            }
            Row::Statement { path, line, label, .. } => {
                let doc = self.docs.iter().position(|d| *d.path == *path);
                let selected = doc.is_some() && doc == self.current && doc.is_some_and(|i| self.docs[i].active_statement().is_some_and(|s| s.line == *line));
                let (p, line) = (path.clone(), *line);
                // The last run: row count, or ✕, between the label and the line number.
                let status = statuses.get(path).and_then(|s| s.get(&line)).cloned();
                base.when(selected, |r| r.bg(active))
                    .child(spacer())
                    .child(div().flex_1().truncate().font_family(t.mono_font_family.clone()).child(label.clone()))
                    .children(status.map(|(text, failed)| div().flex_none().text_xs().text_color(if failed { t.danger } else { muted }).child(text)))
                    .child(div().flex_none().text_xs().text_color(muted).child(line.to_string()))
                    // Click jumps to the statement; double click also runs it.
                    .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| this.click_statement(p.clone(), line, ev.click_count(), window, cx)))
                    .into_any_element()
            }
            Row::Note { text, .. } => base.cursor_default().child(spacer()).text_xs().text_color(muted).child(*text).into_any_element(),
        };
        div().w_full().px_1().child(row).into_any_element()
    }

    /// ↑/↓ in the search field: select the previous / next statement row, keeping focus in the field.
    fn step_search_hit(&mut self, step: isize, window: &mut Window, cx: &mut Context<Self>) {
        let hits: Vec<(PathBuf, usize)> =
            self.rows.iter().filter_map(|r| if let Row::Statement { path, line, .. } = r { Some((path.clone(), *line)) } else { None }).collect();
        if hits.is_empty() {
            return;
        }
        let cur = self.doc().and_then(|d| d.active_statement().map(|s| (d.path.to_path_buf(), s.line)));
        let next = match cur.and_then(|c| hits.iter().position(|r| *r == c)) {
            Some(i) => (i as isize + step).clamp(0, hits.len() as isize - 1) as usize,
            None if step > 0 => 0,
            None => hits.len() - 1,
        };
        let (path, line) = hits[next].clone();
        self.go_to(path, line, false, window, cx);
        self.sidebar_filter.update(cx, |s, cx| s.focus(window, cx));
        cx.notify();
    }

    pub(crate) fn sidebar(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme();
        let (muted, hover, fg, border) = (t.muted_foreground, t.list_hover, t.sidebar_foreground, t.sidebar_border);
        let tab_ix = if self.side_tab == SideTab::Files { 0 } else { 1 };
        let tabs = TabBar::new("side-tabs")
            .segmented()
            .small()
            .selected_index(tab_ix)
            .on_click(cx.listener(|this, ix: &usize, _, cx| {
                this.side_tab = if *ix == 0 { SideTab::Files } else { SideTab::Db };
                cx.notify();
            }))
            .child(Tab::new().label("Files"))
            .child(Tab::new().label("DB"));
        // A click anywhere in the navigation panel (tabs, folders, DB tree, empty space) leaves the
        // History page; the History button itself is outside and toggles it.
        let leave_history = |this: &mut Self, _: &MouseDownEvent, _: &mut Window, cx: &mut Context<Self>| {
            if this.show_history {
                this.show_history = false;
                cx.notify();
            }
        };
        let header = div().flex_none().h(px(36.)).px_2().flex().items_center().gap_2().on_mouse_down(MouseButton::Left, cx.listener(leave_history)).child(tabs);
        let body = match self.side_tab {
            SideTab::Files => {
                let query = self.sidebar_filter.read(cx).value().trim().to_lowercase();
                self.rows = self.build_rows(&query);
                let list = div().id("sidebar").overflow_y_scroll().children(self.render_rows(cx));
                let search = div()
                    .flex_none()
                    .px_2()
                    .pb_1p5()
                    .capture_key_down(cx.listener(|this, ev: &KeyDownEvent, window, cx| {
                        let step = match ev.keystroke.key.as_str() {
                            "up" => -1,
                            "down" => 1,
                            _ => return,
                        };
                        cx.stop_propagation();
                        this.step_search_hit(step, window, cx);
                    }))
                    .child(Input::new(&self.sidebar_filter).small().cleanable(true));
                div().flex_1().min_h_0().flex().flex_col().child(search).child(div().flex_1().min_h_0().child(list.size_full())).into_any_element()
            }
            SideTab::Db => self.db_body(cx),
        };
        let body = div().flex_1().min_h_0().flex().flex_col().on_mouse_down(MouseButton::Left, cx.listener(leave_history)).child(body);
        let item = |id: &'static str, icon: Icon, label: &'static str| {
            div()
                .id(id)
                .flex_1()
                .h(px(30.))
                .px_2()
                .rounded_md()
                .flex()
                .items_center()
                .gap_2()
                .text_sm()
                .text_color(muted)
                .cursor_pointer()
                .hover(move |s| s.bg(hover))
                .child(icon.size_3p5())
                .child(label)
        };
        let footer = div()
            .flex_none()
            .mx_1()
            .mb_1()
            .flex()
            .gap_1()
            .child(item("open-settings", Icon::new(IconName::Settings), "Settings").on_click(cx.listener(|this, _, window, cx| this.open_settings(window, cx))))
            .child(
                item("open-history", Icon::default().data(include_bytes!("../assets/clock.svg")), "History")
                    .when(self.show_history, |d| d.text_color(cx.theme().foreground))
                    .on_click(cx.listener(|this, _, window, cx| this.toggle_history(window, cx))),
            );
        div()
            .size_full()
            .flex()
            .flex_col()
            // Background and border come from the card it sits in (main.rs `card`).
            .text_color(fg)
            .child(header)
            .child(body)
            .child(div().flex_none().h(px(1.)).mx_2().bg(border))
            .child(footer)
            .into_any_element()
    }
}

pub(crate) fn chevron(open: bool, color: Hsla) -> impl IntoElement {
    div()
        .w(px(CHEVRON))
        .flex_none()
        .flex()
        .justify_center()
        .child(Icon::new(if open { IconName::ChevronDown } else { IconName::ChevronRight }).size_3().text_color(color))
}

#[cfg(test)]
mod tests {
    // Not `super::*`: gpui's glob import brings its own `test` macro.
    use super::{FILE_STATEMENTS, Row, Tree, long_statement_runs, moved, renamed_path};
    use std::path::{Path, PathBuf};

    #[test]
    fn only_long_statement_lists_get_their_own_block() {
        let (a, b) = (PathBuf::from("/w/a.sql"), PathBuf::from("/w/b.sql"));
        let file = |p: &PathBuf| Row::File { path: p.clone(), name: String::new(), depth: 0, statements: 0, added: false };
        let st = |p: &PathBuf, n: usize| (0..n).map(|line| Row::Statement { path: p.clone(), line, label: String::new(), depth: 1 }).collect::<Vec<_>>();
        let long = FILE_STATEMENTS + 1;
        // a: over the limit (own block), b: exactly the limit (inline).
        let mut rows = vec![file(&a)];
        rows.extend(st(&a, long));
        rows.push(file(&b));
        rows.extend(st(&b, FILE_STATEMENTS));
        let runs = long_statement_runs(&rows);
        let n = rows.len();
        assert_eq!(runs, [(0..1, None), (1..1 + long, Some(a.as_path())), (1 + long..n, None)]);
        // Nothing long: one plain span; nothing at all: no spans.
        assert_eq!(long_statement_runs(&rows[1 + long..]), [(0..n - 1 - long, None)]);
        assert!(long_statement_runs(&[]).is_empty());
    }

    #[test]
    fn rename_target_and_name_rules() {
        let from = Path::new("/w/q/a.sql");
        assert_eq!(renamed_path(from, " b.sql ", false), Ok(PathBuf::from("/w/q/b.sql")));
        assert_eq!(renamed_path(from, "b", false), Ok(PathBuf::from("/w/q/b.sql")));
        assert_eq!(renamed_path(from, "report.2024", false), Ok(PathBuf::from("/w/q/report.2024.sql")));
        assert_eq!(renamed_path(from, "B.SQL", false), Ok(PathBuf::from("/w/q/B.SQL")));
        assert_eq!(renamed_path(from, "a.sql", false), Ok(from.to_path_buf()));
        assert_eq!(renamed_path(Path::new("/w/q"), "r.v2", true), Ok(PathBuf::from("/w/r.v2")));
        for bad in ["", "  ", "x/y.sql", "x\\y"] {
            assert!(renamed_path(from, bad, false).is_err(), "{bad:?}");
        }
        assert_eq!(moved(Path::new("/w/q/a.sql"), Path::new("/w/q"), Path::new("/w/r")), PathBuf::from("/w/r/a.sql"));
        assert_eq!(moved(Path::new("/w/q/a.sql"), Path::new("/w/q/a.sql"), Path::new("/w/q/b.sql")).as_os_str(), "/w/q/b.sql");
        assert_eq!(moved(Path::new("/w/qq/a.sql"), Path::new("/w/q"), Path::new("/w/r")), PathBuf::from("/w/qq/a.sql"));
    }

    #[test]
    fn tree_groups_by_folder_and_keeps_empty_folders() {
        let root = Path::new("/w");
        let entries = ["/w/a.sql", "/w/crates/app/x.sql", "/w/crates/app/y.sql", "/w/api/v1/z.sql", "/w/empty"].map(PathBuf::from);
        let t = Tree::build(root, &entries);
        assert_eq!(t.files, [PathBuf::from("/w/a.sql")]);
        assert_eq!(t.dirs.keys().collect::<Vec<_>>(), ["api", "crates", "empty"]);
        assert_eq!(t.dirs["crates"].dirs["app"].files.len(), 2);
        assert_eq!(t.dirs["api"].dirs["v1"].files, [PathBuf::from("/w/api/v1/z.sql")]);
        assert!(t.dirs["empty"].files.is_empty() && t.dirs["empty"].dirs.is_empty());
    }
}
