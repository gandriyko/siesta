//! The DB tab: the active file's connection as a tree (schemas → tables / views → columns and
//! indexes), and what a table does: double click inserts `SELECT * FROM …;` and runs it, the
//! context menu copies its DDL or inserts its name.

use std::{collections::HashSet, ops::Range};

use db_exec::{Schema, TableKind};
use gpui::{prelude::FluentBuilder as _, *};
use gpui_component::{
    ActiveTheme as _, Icon, IconName, Sizable as _, WindowExt as _,
    button::{Button, ButtonVariants as _},
    input::{Input, Position, RopeExt as _},
    menu::{ContextMenuExt as _, PopupMenuItem},
};

use crate::{
    Siesta,
    sidebar::{CHEVRON, INDENT, PAD, ROW_H, chevron},
};

pub(crate) enum DbRow {
    Schema { name: String, open: bool, tables: usize },
    Table { schema: String, name: String, view: bool, open: bool, columns: usize },
    Column { name: String, ty: String, pk: bool, not_null: bool },
    Index { name: String, columns: String, unique: bool, primary: bool },
}

impl DbRow {
    fn depth(&self) -> usize {
        match self {
            DbRow::Schema { .. } => 0,
            DbRow::Table { .. } => 1,
            DbRow::Column { .. } | DbRow::Index { .. } => 2,
        }
    }
}

/// Header of the DB tab: `connection · database`, with `· loading…` while it is read.
pub(crate) fn db_title(conn: &str, database: Option<&str>, loading: bool) -> String {
    let mut t = conn.to_string();
    if let Some(db) = database {
        t += &format!(" · {db}");
    }
    if loading {
        t += " · loading…";
    }
    t
}

/// Toggle keys: a schema is open unless its key is in `toggled`, a table is closed unless it is.
pub(crate) fn schema_key(schema: &str) -> String {
    format!("s:{schema}")
}

pub(crate) fn table_key(schema: &str, table: &str) -> String {
    format!("t:{schema}.{table}")
}

/// The visible rows. A filter keeps the tables whose name matches (closed) and those with a
/// matching column (open, showing only those columns).
pub(crate) fn rows(schema: &Schema, query: &str, toggled: &HashSet<String>) -> Vec<DbRow> {
    let searching = !query.is_empty();
    let hit = |s: &str| s.to_lowercase().contains(query);
    let mut out = vec![];
    for sn in &schema.schemas {
        let tables: Vec<_> = sn
            .tables
            .iter()
            .filter_map(|t| {
                let name_hit = !searching || hit(&t.name) || hit(&format!("{}.{}", sn.name, t.name));
                let cols: Vec<_> = t.columns.iter().filter(|c| searching && hit(&c.name)).collect();
                (name_hit || !cols.is_empty()).then_some((t, name_hit, cols))
            })
            .collect();
        if searching && tables.is_empty() {
            continue;
        }
        let open = searching || !toggled.contains(&schema_key(&sn.name));
        out.push(DbRow::Schema { name: sn.name.clone(), open, tables: tables.len() });
        if !open {
            continue;
        }
        for (t, name_hit, cols) in tables {
            let open = toggled.contains(&table_key(&sn.name, &t.name)) || !name_hit;
            out.push(DbRow::Table { schema: sn.name.clone(), name: t.name.clone(), view: t.kind == TableKind::View, open, columns: t.columns.len() });
            if !open {
                continue;
            }
            let shown: Vec<_> = if name_hit { t.columns.iter().collect() } else { cols };
            out.extend(shown.into_iter().map(|c| DbRow::Column { name: c.name.clone(), ty: c.data_type.clone(), pk: c.primary_key, not_null: !c.nullable }));
            if name_hit {
                out.extend(t.indexes.iter().map(|i| DbRow::Index { name: i.name.clone(), columns: i.columns.join(", "), unique: i.unique, primary: i.primary }));
            }
        }
    }
    out
}

/// Opens the Connections page.
fn manage_button(cx: &mut Context<Siesta>) -> Button {
    Button::new("manage-connections").icon(IconName::Network).ghost().small().tooltip("Manage connections").on_click(cx.listener(|this, _, window, cx| this.open_connections(window, cx)))
}

impl Siesta {
    fn toggle_db(&mut self, key: String) {
        if !self.db_toggled.remove(&key) {
            self.db_toggled.insert(key);
        }
    }

    /// A click on a table row: one click opens / closes it, a double click inserts and runs the `SELECT`.
    pub(crate) fn click_table(&mut self, schema: &str, table: &str, clicks: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_db(table_key(schema, table));
        if clicks >= 2 {
            self.toggle_db(table_key(schema, table)); // undo the first click's toggle
            if self.insert_select(schema, table, window, cx) {
                self.run_current(window, cx);
            }
        }
        cx.notify();
    }

    /// `table` or `schema.table` as the current file's connection wants it written.
    fn table_sql(&self, schema: &str, table: &str) -> Option<String> {
        let d = self.doc()?;
        let ctx = d.lang.borrow();
        let known = ctx.connection.as_ref().and_then(|c| self.schemas.borrow().schemas.get(c).cloned());
        let prefixed = known.is_none_or(|s| crate::complete::needs_prefix(&s, schema, ctx.default_schema.as_deref()));
        Some(crate::complete::qualified(d.dialect, schema, table, prefixed))
    }

    /// `SELECT * FROM table;` on a new line after the statement under the cursor, with the cursor
    /// on it. A statement without `;` gets one, or the new one would join it. `false`: nothing inserted.
    pub(crate) fn insert_select(&mut self, schema: &str, table: &str, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(ix) = self.current.filter(|&i| self.docs[i].sql) else {
            window.push_notification("Open a .sql file first", cx);
            return false;
        };
        let Some(name) = self.table_sql(schema, table) else { return false };
        let sql = format!("SELECT * FROM {name};");
        let d = &self.docs[ix];
        let src = d.editor.read(cx).value().to_string();
        let cursor = d.cursor_offset(cx).min(src.len());
        // The statement under the cursor, else the last one before it.
        let st = d.active_statement().or_else(|| d.file.statements.iter().rev().find(|s| s.range.start <= cursor));
        let (edits, at) = match st {
            Some(st) => {
                let end = st.range.end;
                let rest = &src[end..];
                let semi = rest.trim_start_matches([' ', '\t']).starts_with(';');
                let after = if semi { end + rest.find(';').unwrap_or(0) + 1 } else { end };
                let line_end = after + src[after..].find('\n').unwrap_or(src.len() - after);
                let mut edits = vec![];
                if !semi {
                    edits.push((end, ";".to_string()));
                }
                edits.push((line_end, format!("\n{sql}")));
                // `;` added before the new text moves it by one.
                (edits, line_end + usize::from(!semi) + 1)
            }
            None if src.trim().is_empty() => (vec![(0, format!("{sql}\n"))], 0),
            None => (vec![(src.len(), format!("{}{sql}", if src.ends_with('\n') { "" } else { "\n" }))], src.len() + usize::from(!src.ends_with('\n'))),
        };
        // Two inserts at one position would be ambiguous: merge them.
        let mut merged: Vec<(usize, String)> = vec![];
        for (at, text) in edits {
            match merged.last_mut() {
                Some((p, t)) if *p == at => t.push_str(&text),
                _ => merged.push((at, text)),
            }
        }
        d.editor.update(cx, |s, cx| {
            let rope = s.text().clone();
            let edits: Vec<_> = merged
                .into_iter()
                .map(|(p, new_text)| {
                    let pos = rope.offset_to_position(p);
                    lsp_types::TextEdit { range: lsp_types::Range::new(pos, pos), new_text }
                })
                .collect();
            s.apply_lsp_edits(&edits, window, cx);
            let p = s.text().offset_to_position(at);
            s.set_cursor_position(Position::new(p.line, p.character), window, cx);
            s.focus(window, cx);
        });
        self.docs[ix].reparse(cx);
        true
    }

    /// "Insert name": the table's name replaces the selection (or goes at the cursor).
    pub(crate) fn insert_name(&mut self, schema: &str, table: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.current.filter(|&i| self.docs[i].sql) else { return window.push_notification("Open a .sql file first", cx) };
        let Some(name) = self.table_sql(schema, table) else { return };
        self.docs[ix].editor.update(cx, |s, cx| {
            let (rope, sel) = (s.text().clone(), s.selected_range());
            let range = lsp_types::Range::new(rope.offset_to_position(sel.start), rope.offset_to_position(sel.end));
            s.apply_lsp_edits(&vec![lsp_types::TextEdit { range, new_text: name.clone() }], window, cx);
            let p = s.text().offset_to_position(sel.start + name.len());
            s.set_cursor_position(Position::new(p.line, p.character), window, cx);
            s.focus(window, cx);
        });
    }

    fn render_db_row(&self, ix: usize, cx: &mut Context<Self>) -> AnyElement {
        let t = cx.theme();
        let (muted, hover, guide) = (t.muted_foreground, t.list_hover, t.sidebar_border);
        let row = &self.db_rows[ix];
        let depth = row.depth();
        let guides = (0..depth)
            .map(move |i| div().absolute().top_0().bottom_0().left(px(PAD + i as f32 * INDENT + CHEVRON / 2.)).w(px(1.)).bg(guide));
        let base = div()
            .id(("db-row", ix))
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
            .hover(move |s| s.bg(hover))
            .children(guides);
        let spacer = || div().w(px(CHEVRON)).flex_none();
        let svg = |data: &'static [u8]| Icon::default().data(data).size_3p5().flex_none().text_color(muted);
        let badge = |text: &'static str| div().flex_none().text_xs().text_color(muted).child(text);
        let el = match row {
            DbRow::Schema { name, open, tables } => {
                let key = schema_key(name);
                base.child(chevron(*open, muted))
                    .child(svg(include_bytes!("../assets/database.svg")))
                    .child(div().flex_1().truncate().child(name.clone()))
                    .child(div().flex_none().text_xs().text_color(muted).child(tables.to_string()))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.toggle_db(key.clone());
                        cx.notify();
                    }))
                    .into_any_element()
            }
            DbRow::Table { schema, name, view, open, columns } => {
                let (s, n, view_model) = (schema.clone(), name.clone(), cx.entity().downgrade());
                let (ds, dn) = (schema.clone(), name.clone());
                base.child(chevron(*open, muted))
                    .child(if *view { Icon::new(IconName::Eye).size_3p5().flex_none().text_color(muted) } else { svg(include_bytes!("../assets/table.svg")) })
                    .child(div().flex_1().truncate().child(name.clone()))
                    .child(div().flex_none().text_xs().text_color(muted).child(columns.to_string()))
                    .on_click(cx.listener(move |this, ev: &ClickEvent, window, cx| this.click_table(&s, &n, ev.click_count(), window, cx)))
                    .context_menu(move |menu, _, _| {
                        let (a, b, v1, v2) = (ds.clone(), dn.clone(), view_model.clone(), view_model.clone());
                        let (c, d) = (ds.clone(), dn.clone());
                        menu.item(PopupMenuItem::new("Copy DDL").on_click(move |_, window, cx| {
                            v1.update(cx, |this, cx| this.copy_ddl(&a, &b, window, cx)).ok();
                        }))
                        .item(PopupMenuItem::new("Insert name").on_click(move |_, window, cx| {
                            v2.update(cx, |this, cx| this.insert_name(&c, &d, window, cx)).ok();
                        }))
                    })
                    .into_any_element()
            }
            DbRow::Column { name, ty, pk, not_null } => base
                .cursor_default()
                .child(spacer())
                .child(div().flex_1().truncate().child(name.clone()))
                .child(div().flex_none().text_xs().text_color(muted).truncate().max_w(px(90.)).child(ty.clone()))
                .when(*pk, |r| r.child(badge("PK")))
                .when(*not_null, |r| r.child(badge("NOT NULL")))
                .into_any_element(),
            DbRow::Index { name, columns, unique, primary } => base
                .cursor_default()
                .child(spacer())
                .child(div().flex_1().truncate().child(format!("{name} ({columns})")))
                .when(*primary, |r| r.child(badge("PRIMARY")))
                .when(*unique && !*primary, |r| r.child(badge("UNIQUE")))
                .into_any_element(),
        };
        div().w_full().px_1().child(el).into_any_element()
    }

    /// The DB tab's content: the tree, or why there is none yet.
    pub(crate) fn db_body(&mut self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let note = |text: String| div().flex_1().px_3().pt_2().text_sm().text_color(muted).child(text).into_any_element();
        if self.conns.connections.is_empty() {
            // First run: say so and offer the form right here (any file picks it up from the picker).
            let add = Button::new("db-add-connection")
                .icon(IconName::Plus)
                .label("Add connection")
                .small()
                .primary()
                .on_click(cx.listener(|this, _, window, cx| {
                    let pick = this.doc().is_some_and(|d| d.sql);
                    this.open_conn_form_for(None, false, pick, window, cx)
                }));
            return div()
                .flex_1()
                .flex()
                .flex_col()
                .gap_2()
                .child(note("No connections yet. Add one (Postgres or MySQL / MariaDB) to run queries and browse its schema.".into()))
                .child(div().px_3().flex().child(add))
                .into_any_element();
        }
        let Some(d) = self.doc().filter(|d| d.sql) else { return note("Open a .sql file to see its connection's schema".into()) };
        let Some(conn) = d.connection().map(str::to_string) else {
            let new = Button::new("db-new-connection").label("New connection…").small().on_click(cx.listener(|this, _, window, cx| this.open_conn_form(None, window, cx)));
            return div().flex_1().flex().flex_col().gap_2().child(note("No connection: pick one in the top-right corner".into())).child(div().px_3().flex().gap_2().child(new).child(manage_button(cx))).into_any_element();
        };
        let key = d.schema_key().unwrap_or_default();
        let shown_db = d.shown_database();
        let (schema, loading, error) = {
            let c = self.schemas.borrow();
            (c.schemas.get(&key).cloned(), c.loading.contains(&key), c.errors.get(&key).cloned())
        };
        let action = |id: &'static str, label: &'static str, cx: &mut Context<Self>| {
            Button::new(id).label(label).small().on_click(cx.listener(|this, _, window, cx| this.refresh_schema(window, cx)))
        };
        let Some(schema) = schema else {
            let text = if loading {
                format!("Loading the schema of '{conn}'…")
            } else if let Some(e) = &error {
                format!("Could not read the schema of '{conn}': {e}")
            } else {
                // It loads by itself when a connection is picked; nothing to click yet.
                format!("The schema of '{conn}' is not loaded.")
            };
            return div()
                .flex_1()
                .flex()
                .flex_col()
                .gap_2()
                .child(note(text))
                .when(!loading, |el| el.child(div().px_3().child(action("retry-schema", if error.is_some() { "Retry" } else { "Refresh" }, cx))))
                .into_any_element();
        };
        let query = self.db_filter.read(cx).value().trim().to_lowercase();
        self.db_rows = rows(&schema, &query, &self.db_toggled);
        let list = uniform_list("db-tree", self.db_rows.len(), cx.processor(|this, range: Range<usize>, _, cx| {
            range.map(|ix| this.render_db_row(ix, cx)).collect::<Vec<_>>()
        }));
        let conn_name = conn.clone();
        let title = db_title(&conn, shown_db.as_deref(), loading);
        let refresh = Button::new("refresh-schema")
            .icon(IconName::RefreshCw)
            .ghost()
            .small()
            .tooltip("Refresh schema")
            .on_click(cx.listener(|this, _, window, cx| this.refresh_schema(window, cx)));
        let edit_name = conn_name.clone();
        let edit = Button::new("edit-connection")
            .icon(IconName::Settings)
            .ghost()
            .small()
            .tooltip("Edit connection")
            .on_click(cx.listener(move |this, _, window, cx| this.open_conn_form(Some(edit_name.clone()), window, cx)));
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
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(div().text_xs().font_weight(FontWeight::SEMIBOLD).text_color(muted).truncate().child(title.to_uppercase()))
                    .child(div().flex().gap_1().child(manage_button(cx)).child(edit).child(refresh)),
            )
            .child(div().flex_none().px_2().pb_1p5().child(Input::new(&self.db_filter).small().cleanable(true)))
            .when(self.db_rows.is_empty(), |el| el.child(note("Nothing matches".into())))
            .child(div().flex_1().min_h_0().child(list.size_full()))
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    // Not `super::*`: it brings `gpui::test`, which shadows `#[test]`.
    use super::{DbRow, db_title, rows, schema_key, table_key};
    use db_exec::{ColumnInfo, Index, Schema, SchemaNode, Table, TableKind};
    use std::collections::HashSet;

    #[test]
    fn header_names_connection_and_database() {
        assert_eq!(db_title("dev", Some("app"), false), "dev · app");
        assert_eq!(db_title("dev", None, true), "dev · loading…");
        assert_eq!(db_title("dev", Some("app"), true), "dev · app · loading…");
    }

    fn schema() -> Schema {
        let col = |n: &str, pk: bool| ColumnInfo { name: n.into(), data_type: "integer".into(), nullable: !pk, primary_key: pk, ordinal: 1 };
        let table = |n: &str, kind, cols: &[(&str, bool)]| Table {
            name: n.into(),
            kind,
            columns: cols.iter().map(|(c, pk)| col(c, *pk)).collect(),
            indexes: vec![Index { name: format!("{n}_pkey"), columns: vec!["id".into()], unique: true, primary: true }],
        };
        Schema {
            schemas: vec![
                SchemaNode { name: "analytics".into(), tables: vec![table("events", TableKind::Table, &[("id", true)])] },
                SchemaNode {
                    name: "public".into(),
                    tables: vec![table("orders", TableKind::Table, &[("id", true), ("user_id", false)]), table("v_users", TableKind::View, &[("name", false)])],
                },
            ],
        }
    }

    fn kinds(rows: &[DbRow]) -> Vec<String> {
        rows.iter()
            .map(|r| match r {
                DbRow::Schema { name, open, .. } => format!("S {name} {open}"),
                DbRow::Table { name, view, open, .. } => format!("T {name}{} {open}", if *view { " view" } else { "" }),
                DbRow::Column { name, pk, not_null, .. } => format!("C {name}{}{}", if *pk { " pk" } else { "" }, if *not_null { " nn" } else { "" }),
                DbRow::Index { name, .. } => format!("I {name}"),
            })
            .collect()
    }

    #[test]
    fn tree_opens_by_schema_then_table() {
        let mut toggled = HashSet::new();
        let r = rows(&schema(), "", &toggled);
        assert_eq!(kinds(&r), ["S analytics true", "T events false", "S public true", "T orders false", "T v_users view false"]);
        toggled.insert(table_key("public", "orders"));
        toggled.insert(schema_key("analytics"));
        let r = rows(&schema(), "", &toggled);
        assert_eq!(
            kinds(&r),
            ["S analytics false", "S public true", "T orders true", "C id pk nn", "C user_id", "I orders_pkey", "T v_users view false"]
        );
    }

    #[test]
    fn filter_matches_tables_and_columns() {
        let r = rows(&schema(), "user", &HashSet::new());
        // `v_users` by name (closed), `orders` by its `user_id` column (open, only that column).
        assert_eq!(kinds(&r), ["S public true", "T orders true", "C user_id", "T v_users view false"]);
        assert_eq!(kinds(&rows(&schema(), "events", &HashSet::new())), ["S analytics true", "T events false"]);
        assert!(rows(&schema(), "zzz", &HashSet::new()).is_empty());
    }
}
