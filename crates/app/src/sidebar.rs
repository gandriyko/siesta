//! Sidebar: tabs Files | DB. Files: folders → `.sql` files → statements.
//!
//! Styled after the VS Code / Zed project panels: disclosure chevrons, indent guides,
//! compact single-child folders (`crates/app`), inset rounded selection and muted meta on the
//! right.

use std::{
    collections::BTreeMap,
    ops::Range,
    path::{Path, PathBuf},
};

use gpui::{prelude::FluentBuilder as _, *};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, WindowExt as _,
    input::Input,
    tab::{Tab, TabBar},
};

use crate::{Siesta, SideTab};

pub(crate) const ROW_H: f32 = 24.;
pub(crate) const INDENT: f32 = 14.;
/// Left padding before the first chevron.
pub(crate) const PAD: f32 = 6.;
/// Chevron column width; leaves get a spacer of the same width.
pub(crate) const CHEVRON: f32 = 14.;

pub(crate) enum Row {
    Dir { path: PathBuf, name: String, depth: usize },
    File { path: PathBuf, name: String, depth: usize, statements: usize },
    Statement { path: PathBuf, line: usize, label: String, depth: usize },
    Note { text: &'static str, depth: usize },
}

impl Row {
    fn depth(&self) -> usize {
        match self {
            Row::Dir { depth, .. } | Row::File { depth, .. } | Row::Statement { depth, .. } | Row::Note { depth, .. } => *depth,
        }
    }
}

/// Folder tree of the workspace's files.
#[derive(Default)]
struct Tree {
    dirs: BTreeMap<String, Tree>,
    files: Vec<PathBuf>,
}

impl Tree {
    fn build(root: &Path, files: &[PathBuf]) -> Self {
        let mut tree = Tree::default();
        for f in files {
            let rel = f.strip_prefix(root).unwrap_or(f);
            let dirs = rel.parent().into_iter().flat_map(|p| p.components()).map(|c| c.as_os_str().to_string_lossy().into_owned());
            dirs.fold(&mut tree, |node, d| node.dirs.entry(d).or_default()).files.push(f.clone());
        }
        tree
    }
}

impl Siesta {
    /// `(line, label)` of a file's statements: live split when open, else read once.
    pub(crate) fn outline_of(&mut self, path: &Path, cx: &App) -> Vec<(usize, String)> {
        if !siesta_workspace::is_sql(path) {
            return vec![];
        }
        match self.docs.iter().find(|d| *d.path == *path) {
            Some(d) => crate::doc::outline(&d.editor.read(cx).value(), &d.file),
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

    /// Hover `+`: a new `.sql` file in `dir`, named in the system save dialog.
    fn add_button(&self, ix: usize, dir: PathBuf, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id(("add-file", ix))
            .flex_none()
            .px_1()
            .rounded_sm()
            .text_color(cx.theme().muted_foreground)
            .invisible()
            .group_hover("row", |s| s.visible())
            .hover(|s| s.bg(cx.theme().list_active))
            .child("+")
            .on_click(cx.listener(move |this, _, window, cx| {
                cx.stop_propagation();
                this.new_file(&dir, window, cx);
            }))
    }

    pub(crate) fn new_file(&mut self, dir: &Path, window: &mut Window, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_new_path(dir, Some("query.sql"));
        cx.spawn_in(window, async move |this, cx| {
            let Ok(Ok(Some(mut path))) = picked.await else { return };
            if !siesta_workspace::is_sql(&path) {
                path.set_extension("sql");
            }
            this.update_in(cx, |this, window, cx| {
                // The dialog already confirmed overwriting; an existing file is just opened, never truncated.
                if !path.exists()
                    && let Err(e) = std::fs::write(&path, "")
                {
                    window.push_notification(format!("{}: {e}", path.display()), cx);
                    return;
                }
                this.rescan();
                this.open(path, None, window, cx);
                cx.notify();
            })
            .ok();
        })
        .detach();
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

    pub(crate) fn build_rows(&mut self, query: &str, cx: &App) -> Vec<Row> {
        let (root, files) = (self.root.clone(), self.files.clone());
        let mut rows = vec![];
        if files.is_empty() {
            rows.push(Row::Note { text: "No .sql files", depth: 0 });
        }
        self.emit(&Tree::build(&root, &files), &root, 0, query, &mut rows, cx);
        rows
    }

    fn emit(&mut self, tree: &Tree, base: &Path, depth: usize, query: &str, rows: &mut Vec<Row>, cx: &App) {
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
                self.emit(node, &path, depth + 1, query, &mut found, cx);
                if !found.is_empty() {
                    rows.push(Row::Dir { path: path.clone(), name: label, depth });
                    rows.extend(found);
                }
                continue;
            }
            rows.push(Row::Dir { path: path.clone(), name: label, depth });
            if !self.collapsed_dirs.contains(&path) {
                self.emit(node, &path, depth + 1, query, rows, cx);
            }
        }
        for f in &tree.files {
            let outline = self.outline_of(f, cx);
            let name = f.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned());
            if searching {
                // A file-name match keeps all its statements; otherwise only the matching ones.
                let file_hit = name.to_lowercase().contains(query);
                let hits: Vec<_> = outline.into_iter().filter(|(_, l)| file_hit || l.to_lowercase().contains(query)).collect();
                if hits.is_empty() && !file_hit {
                    continue;
                }
                rows.push(Row::File { path: f.clone(), name, depth, statements: hits.len() });
                rows.extend(hits.into_iter().map(|(line, label)| Row::Statement { path: f.clone(), line, label, depth: depth + 1 }));
                continue;
            }
            rows.push(Row::File { path: f.clone(), name, depth, statements: outline.len() });
            if self.expanded.contains(f) {
                rows.extend(outline.into_iter().map(|(line, label)| Row::Statement { path: f.clone(), line, label, depth: depth + 1 }));
            }
        }
    }

    fn render_row(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
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
            Row::Dir { path, name, .. } => {
                let open = !self.collapsed_dirs.contains(path);
                let p = path.clone();
                base.child(chevron(open, muted))
                    .child(icon(if open { IconName::FolderOpen } else { IconName::Folder }, muted))
                    .child(div().flex_1().truncate().child(name.clone()))
                    .child(self.add_button(ix, path.clone(), cx))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_dir(&p);
                        cx.notify();
                    }))
                    .into_any_element()
            }
            Row::File { path, name, statements, .. } => {
                let doc = self.docs.iter().position(|d| *d.path == *path);
                let current = doc.is_some() && doc == self.current;
                let dirty = doc.is_some_and(|i| self.docs[i].dirty);
                let sql = siesta_workspace::is_sql(path);
                let open = self.expanded.contains(path);
                let (p, q) = (path.clone(), path.clone());
                let toggle = div()
                    .id(("file-chevron", ix))
                    .flex_none()
                    .child(chevron(open, muted))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.toggle(&q);
                        cx.notify();
                    }));
                let meta = match (dirty, sql) {
                    (true, _) => "●".to_string(),
                    (false, false) => String::new(),
                    (false, true) => statements.to_string(),
                };
                base.when(current, |r| r.bg(active))
                    .map(|r| if sql { r.child(toggle) } else { r.child(spacer()) })
                    .child(icon(IconName::FileText, if current { t.foreground } else { muted }))
                    .child(div().flex_1().truncate().child(name.clone()))
                    .children(path.parent().map(|d| self.add_button(ix, d.to_path_buf(), cx)))
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
                let status = self.statement_status(path, line, cx);
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
        let icon_button = |id: &'static str, name: IconName| {
            div().id(id).p_1().rounded_md().cursor_pointer().text_color(muted).hover(move |s| s.bg(hover)).child(Icon::new(name).size_3p5())
        };
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
        let all = self.show_all_files;
        let header = div()
            .flex_none()
            .h(px(36.))
            .px_2()
            .flex()
            .items_center()
            .justify_between()
            .gap_2()
            .child(tabs)
            .child(div().flex().items_center().when(self.side_tab == SideTab::Files, |el| {
                el.child(
                    icon_button("all-files", if all { IconName::Eye } else { IconName::EyeOff })
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_all_files(cx))),
                )
                .child(icon_button("open-folder", IconName::FolderOpen).on_click(cx.listener(|this, _, window, cx| this.open_folder(window, cx))))
            }));
        let body = match self.side_tab {
            SideTab::Files => {
                let query = self.sidebar_filter.read(cx).value().trim().to_lowercase();
                self.rows = self.build_rows(&query, cx);
                let list = uniform_list("sidebar", self.rows.len(), cx.processor(|this, range: Range<usize>, _, cx| {
                    range.map(|ix| this.render_row(ix, cx)).collect::<Vec<_>>()
                }));
                let root_name = self.root.file_name().map_or_else(|| self.root.display().to_string(), |n| n.to_string_lossy().into_owned());
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
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .child(
                        div()
                            .flex_none()
                            .px_3()
                            .pb_1()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(muted)
                            .truncate()
                            .child(root_name.to_uppercase()),
                    )
                    .child(search)
                    .child(div().flex_1().min_h_0().child(list.size_full()))
                    .into_any_element()
            }
            SideTab::Db => self.db_body(cx),
        };
        let footer = div().flex_none().mx_1().mb_1().child(
            div()
                .id("open-settings")
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
                .child(Icon::new(IconName::Settings).size_3p5())
                .child("Settings")
                .on_click(cx.listener(|this, _, window, cx| this.open_settings(window, cx))),
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
    use super::Tree;
    use std::path::{Path, PathBuf};

    #[test]
    fn tree_groups_by_folder() {
        let root = Path::new("/w");
        let files = ["/w/a.sql", "/w/crates/app/x.sql", "/w/crates/app/y.sql", "/w/api/v1/z.sql"].map(PathBuf::from);
        let t = Tree::build(root, &files);
        assert_eq!(t.files, [PathBuf::from("/w/a.sql")]);
        assert_eq!(t.dirs.keys().collect::<Vec<_>>(), ["api", "crates"]);
        assert_eq!(t.dirs["crates"].dirs["app"].files.len(), 2);
        assert_eq!(t.dirs["api"].dirs["v1"].files, [PathBuf::from("/w/api/v1/z.sql")]);
    }
}
