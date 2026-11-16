//! Database structure from `information_schema` (plus `pg_index` for Postgres indexes), and table DDL.
//!
//! [`Schema`] is plain serde data: the GUI's DB tab, autocomplete, the on-disk cache and
//! `siesta schema` all read it.

use serde::{Deserialize, Serialize};
use sql_split::{Dialect, Kind};

use crate::{DbError, Page, Session, Value, quote_ident};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schema {
    /// Sorted by name. Postgres: every non-system schema; MySQL: the current database, or all
    /// non-system databases when the connection has none.
    pub schemas: Vec<SchemaNode>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaNode {
    pub name: String,
    pub tables: Vec<Table>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TableKind {
    Table,
    View,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Table {
    pub name: String,
    pub kind: TableKind,
    pub columns: Vec<ColumnInfo>,
    pub indexes: Vec<Index>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnInfo {
    pub name: String,
    /// Postgres: `integer`, `character varying`, `jsonb`; MySQL: `int(11)`, `varchar(255)`.
    pub data_type: String,
    pub nullable: bool,
    pub primary_key: bool,
    /// 1-based position in the table.
    pub ordinal: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Index {
    pub name: String,
    pub columns: Vec<String>,
    pub unique: bool,
    pub primary: bool,
}

impl Schema {
    /// `table` or `schema.table`. A bare name must be unambiguous across schemas, or the first
    /// match wins (schemas are sorted).
    pub fn find(&self, name: &str) -> Option<(&str, &Table)> {
        let (schema, table) = match name.split_once('.') {
            Some((s, t)) => (Some(s), t),
            None => (None, name),
        };
        self.schemas
            .iter()
            .filter(|s| schema.is_none_or(|q| q == s.name))
            .find_map(|s| s.tables.iter().find(|t| t.name == table).map(|t| (s.name.as_str(), t)))
    }
}

const MY_SYSTEM: &str = "'mysql','information_schema','performance_schema','sys'";

fn text(v: &Value) -> String {
    v.display()
}

fn flag(v: &Value) -> bool {
    match v {
        Value::Bool(b) => *b,
        Value::Int(i) => *i != 0,
        other => matches!(other.display().as_str(), "t" | "true" | "1" | "YES"),
    }
}

fn int(v: &Value) -> u32 {
    match v {
        Value::Int(i) => *i as u32,
        other => other.display().parse().unwrap_or(0),
    }
}

/// SQL string literal (`'` doubled; MySQL also needs `\` doubled).
fn lit(d: Dialect, s: &str) -> String {
    let s = s.replace('\'', "''");
    format!("'{}'", if d == Dialect::MySql { s.replace('\\', "\\\\") } else { s })
}

impl Session {
    /// Reads the whole structure with four catalog queries.
    pub async fn schema(&mut self) -> Result<Schema, DbError> {
        let pg = self.dialect() == Dialect::Postgres;
        let (tables, columns, indexes) = if pg {
            let w = "table_schema NOT IN ('pg_catalog','information_schema') AND table_schema NOT LIKE 'pg\\_%'";
            (
                format!("SELECT table_schema, table_name, table_type FROM information_schema.tables WHERE {w} ORDER BY 1, 2"),
                format!(
                    "SELECT table_schema, table_name, column_name, \
                     CASE data_type WHEN 'USER-DEFINED' THEN udt_name WHEN 'ARRAY' THEN ltrim(udt_name, '_') || '[]' ELSE data_type END, \
                     is_nullable, ordinal_position FROM information_schema.columns WHERE {w} ORDER BY 1, 2, ordinal_position"
                ),
                "SELECT n.nspname, t.relname, i.relname, ix.indisunique, ix.indisprimary, a.attname \
                 FROM pg_index ix JOIN pg_class t ON t.oid = ix.indrelid JOIN pg_class i ON i.oid = ix.indexrelid \
                 JOIN pg_namespace n ON n.oid = t.relnamespace \
                 CROSS JOIN LATERAL unnest(ix.indkey::int2[]) WITH ORDINALITY AS k(attnum, ord) \
                 JOIN pg_attribute a ON a.attrelid = t.oid AND a.attnum = k.attnum \
                 WHERE n.nspname NOT IN ('pg_catalog','information_schema') AND n.nspname NOT LIKE 'pg\\_%' \
                 ORDER BY 1, 2, 3, k.ord"
                    .to_string(),
            )
        } else {
            let w = format!(
                "(DATABASE() IS NOT NULL AND table_schema = DATABASE()) OR (DATABASE() IS NULL AND table_schema NOT IN ({MY_SYSTEM}))"
            );
            (
                format!("SELECT CAST(table_schema AS CHAR), CAST(table_name AS CHAR), CAST(table_type AS CHAR) FROM information_schema.tables WHERE {w} ORDER BY 1, 2"),
                format!(
                    "SELECT CAST(table_schema AS CHAR), CAST(table_name AS CHAR), CAST(column_name AS CHAR), CAST(column_type AS CHAR), \
                     CAST(is_nullable AS CHAR), CAST(ordinal_position AS CHAR) FROM information_schema.columns WHERE {w} ORDER BY 1, 2, ordinal_position"
                ),
                format!(
                    "SELECT CAST(table_schema AS CHAR), CAST(table_name AS CHAR), CAST(index_name AS CHAR), CAST(non_unique = 0 AS CHAR), \
                     CAST(index_name = 'PRIMARY' AS CHAR), CAST(column_name AS CHAR) FROM information_schema.statistics WHERE {w} \
                     AND column_name IS NOT NULL ORDER BY 1, 2, 3, seq_in_index"
                ),
            )
        };
        let mut out = Schema::default();
        for r in self.catalog(&tables).await? {
            let kind = if text(&r[2]).contains("VIEW") { TableKind::View } else { TableKind::Table };
            node(&mut out, &text(&r[0])).tables.push(Table { name: text(&r[1]), kind, columns: vec![], indexes: vec![] });
        }
        for r in self.catalog(&columns).await? {
            if let Some(t) = table(&mut out, &text(&r[0]), &text(&r[1])) {
                let ordinal = int(&r[5]);
                t.columns.push(ColumnInfo { name: text(&r[2]), data_type: text(&r[3]), nullable: flag(&r[4]), primary_key: false, ordinal });
            }
        }
        for r in self.catalog(&indexes).await? {
            let Some(t) = table(&mut out, &text(&r[0]), &text(&r[1])) else { continue };
            let (name, col) = (text(&r[2]), text(&r[5]));
            match t.indexes.last_mut().filter(|i| i.name == name) {
                Some(i) => i.columns.push(col),
                None => t.indexes.push(Index { name, columns: vec![col], unique: flag(&r[3]), primary: flag(&r[4]) }),
            }
        }
        for t in out.schemas.iter_mut().flat_map(|s| &mut s.tables) {
            for c in &mut t.columns {
                c.primary_key = t.indexes.iter().any(|i| i.primary && i.columns.contains(&c.name));
            }
        }
        Ok(out)
    }

    /// `CREATE TABLE` / `CREATE VIEW` text. MySQL: `SHOW CREATE`; Postgres: assembled from the
    /// catalog (columns, defaults, identity, constraints, secondary indexes; no storage options,
    /// ownership, partitions or comments).
    pub async fn table_ddl(&mut self, schema: &str, table: &str) -> Result<String, DbError> {
        let d = self.dialect();
        let qualified = format!("{}.{}", quote_ident(d, schema), quote_ident(d, table));
        if d == Dialect::MySql {
            let rows = self.catalog(&format!("SHOW CREATE TABLE {qualified}")).await?;
            return rows.first().and_then(|r| r.get(1)).map(text).ok_or_else(|| DbError { message: format!("no such table: {schema}.{table}"), ..Default::default() });
        }
        let rel = format!("(quote_ident({}) || '.' || quote_ident({}))::regclass", lit(d, schema), lit(d, table));
        let missing = || DbError { message: format!("no such table: {schema}.{table}"), ..Default::default() };
        let kind = self.catalog(&format!("SELECT relkind::text FROM pg_class WHERE oid = to_regclass(quote_ident({}) || '.' || quote_ident({}))", lit(d, schema), lit(d, table))).await?;
        let kind = kind.first().map(|r| text(&r[0])).ok_or_else(missing)?;
        if kind == "v" || kind == "m" {
            let def = self.catalog(&format!("SELECT pg_get_viewdef({rel}, true)")).await?;
            let what = if kind == "m" { "MATERIALIZED VIEW" } else { "VIEW" };
            return Ok(format!("CREATE {what} {qualified} AS\n{}", def.first().map(|r| text(&r[0])).unwrap_or_default().trim_end()));
        }
        let cols = self
            .catalog(&format!(
                "SELECT a.attname, format_type(a.atttypid, a.atttypmod), a.attnotnull, pg_get_expr(d.adbin, d.adrelid), a.attidentity::text, a.attgenerated::text \
                 FROM pg_attribute a LEFT JOIN pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum \
                 WHERE a.attrelid = {rel} AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum"
            ))
            .await?;
        let mut lines: Vec<String> = cols
            .iter()
            .map(|r| {
                let mut l = format!("    {} {}", quote_ident(d, &text(&r[0])), text(&r[1]));
                let default = (!matches!(r[3], Value::Null)).then(|| text(&r[3]));
                match (text(&r[4]).as_str(), text(&r[5]).as_str(), default) {
                    ("a", _, _) => l += " GENERATED ALWAYS AS IDENTITY",
                    ("d", _, _) => l += " GENERATED BY DEFAULT AS IDENTITY",
                    (_, "s", Some(e)) => l += &format!(" GENERATED ALWAYS AS ({e}) STORED"),
                    (_, _, Some(e)) => l += &format!(" DEFAULT {e}"),
                    _ => {}
                }
                if flag(&r[2]) {
                    l += " NOT NULL";
                }
                l
            })
            .collect();
        let cons = self
            .catalog(&format!(
                "SELECT conname, pg_get_constraintdef(oid) FROM pg_constraint WHERE conrelid = {rel} AND contype IN ('p','u','c','f') \
                 ORDER BY CASE contype WHEN 'p' THEN 0 WHEN 'u' THEN 1 WHEN 'f' THEN 2 ELSE 3 END, conname"
            ))
            .await?;
        lines.extend(cons.iter().map(|r| format!("    CONSTRAINT {} {}", quote_ident(d, &text(&r[0])), text(&r[1]))));
        let mut ddl = format!("CREATE TABLE {qualified} (\n{}\n);", lines.join(",\n"));
        let idx = self
            .catalog(&format!(
                "SELECT pg_get_indexdef(indexrelid) FROM pg_index WHERE indrelid = {rel} AND NOT indisprimary \
                 AND NOT EXISTS (SELECT 1 FROM pg_constraint c WHERE c.conindid = indexrelid) ORDER BY 1"
            ))
            .await?;
        for r in idx {
            ddl += &format!("\n{};", text(&r[0]));
        }
        Ok(ddl)
    }

    /// Runs a catalog query without the row cap, returning raw values.
    async fn catalog(&mut self, sql: &str) -> Result<Vec<Vec<Value>>, DbError> {
        let cap = std::mem::replace(&mut self.max_rows, usize::MAX);
        let r = self.run(sql, Kind::Read, &Page::default()).await;
        self.max_rows = cap;
        r.map(|o| o.rows)
    }
}

fn node<'a>(s: &'a mut Schema, name: &str) -> &'a mut SchemaNode {
    // Rows arrive sorted, so the schema is the last node or a new one.
    if s.schemas.last().is_none_or(|n| n.name != name) {
        s.schemas.push(SchemaNode { name: name.to_string(), tables: vec![] });
    }
    s.schemas.last_mut().expect("just pushed")
}

fn table<'a>(s: &'a mut Schema, schema: &str, name: &str) -> Option<&'a mut Table> {
    s.schemas.iter_mut().find(|n| n.name == schema)?.tables.iter_mut().find(|t| t.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_by_bare_or_qualified_name() {
        let t = |n: &str| Table { name: n.into(), kind: TableKind::Table, columns: vec![], indexes: vec![] };
        let s = Schema { schemas: vec![SchemaNode { name: "a".into(), tables: vec![t("x")] }, SchemaNode { name: "b".into(), tables: vec![t("x"), t("y")] }] };
        assert_eq!(s.find("x").unwrap().0, "a");
        assert_eq!(s.find("b.x").unwrap().0, "b");
        assert_eq!(s.find("y").unwrap().0, "b");
        assert!(s.find("a.y").is_none());
        assert_eq!(lit(Dialect::MySql, "a'b\\"), "'a''b\\\\'");
    }
}
