//! Where the columns of a result come from: the table and column of each one, and the primary
//! key of those tables. What editing a result needs to write `UPDATE … WHERE <key>`.
//!
//! The server says it, not a parser: the statement is prepared (not run) and the row description
//! names each column's table — Postgres by table OID and attribute number, MySQL / MariaDB by
//! `org_table` / `org_name`. Expressions, aggregates and derived tables have no table.

use std::collections::HashMap;

use sqlx::{Column as _, Executor as _, Statement as _};

use crate::{Conn, DbError, Session, Value, schema::{lit, text}, sql_str, strip};

/// A table the result's columns come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceTable {
    pub schema: String,
    pub name: String,
    /// Column names in key order; empty: the table has none (or it is a view).
    pub primary_key: Vec<String>,
}

/// One entry per result column: `(index into tables, the table's column name)`, or `None` for an
/// expression.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Source {
    pub columns: Vec<Option<(usize, String)>>,
    pub tables: Vec<SourceTable>,
}

fn int(v: &Value) -> i64 {
    match v {
        Value::Int(i) => *i,
        other => other.display().parse().unwrap_or(0),
    }
}

impl Session {
    /// The source of every column of `sql` (one SELECT-like statement), without running it.
    pub async fn source(&mut self, sql: &str) -> Result<Source, DbError> {
        let inner = strip(sql);
        let len = inner.chars().count();
        let e = |e| DbError::from_sqlx(e, 0, len);
        match &mut self.conn {
            Conn::Pg(c) => {
                let st = c.prepare(sql_str(inner)).await.map_err(e)?;
                let cols: Vec<Option<(u32, i64)>> = st.columns().iter().map(|c| c.relation_id().zip(c.relation_attribute_no()).map(|(o, a)| (o.0, a as i64))).collect();
                self.pg_source(cols).await
            }
            Conn::My(c) => {
                let st = c.prepare(sql_str(inner)).await.map_err(e)?;
                // ponytail: sqlx joins `schema.table`; a schema name with a dot would split wrong.
                let cols: Vec<Option<(String, String, String)>> = st
                    .columns()
                    .iter()
                    .map(|c| {
                        let o = c.origin();
                        let t = o.table_column()?;
                        let (schema, table) = t.table.split_once('.').unwrap_or(("", &t.table));
                        Some((schema.to_string(), table.to_string(), t.name.to_string()))
                    })
                    .collect();
                self.my_source(cols).await
            }
        }
    }

    async fn pg_source(&mut self, cols: Vec<Option<(u32, i64)>>) -> Result<Source, DbError> {
        let mut oids: Vec<u32> = cols.iter().flatten().map(|c| c.0).collect();
        oids.sort();
        oids.dedup();
        if oids.is_empty() {
            return Ok(Source { columns: vec![None; cols.len()], tables: vec![] });
        }
        let list = oids.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
        let names = self
            .catalog(&format!(
                "SELECT c.oid::int8, n.nspname::text, c.relname::text, a.attnum::int8, a.attname::text \
                 FROM pg_attribute a JOIN pg_class c ON c.oid = a.attrelid JOIN pg_namespace n ON n.oid = c.relnamespace \
                 WHERE a.attrelid IN ({list}) AND a.attnum > 0 AND NOT a.attisdropped"
            ))
            .await?;
        let keys = self
            .catalog(&format!(
                "SELECT i.indrelid::int8, a.attname::text FROM pg_index i \
                 CROSS JOIN LATERAL unnest(i.indkey::int2[]) WITH ORDINALITY AS k(attnum, ord) \
                 JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = k.attnum \
                 WHERE i.indisprimary AND i.indrelid IN ({list}) ORDER BY 1, k.ord"
            ))
            .await?;
        let mut tables: Vec<SourceTable> = vec![];
        let mut index: HashMap<i64, usize> = HashMap::new();
        let mut attrs: HashMap<(i64, i64), String> = HashMap::new();
        for r in &names {
            let oid = int(&r[0]);
            index.entry(oid).or_insert_with(|| {
                tables.push(SourceTable { schema: text(&r[1]), name: text(&r[2]), primary_key: vec![] });
                tables.len() - 1
            });
            attrs.insert((oid, int(&r[3])), text(&r[4]));
        }
        for r in &keys {
            if let Some(&i) = index.get(&int(&r[0])) {
                tables[i].primary_key.push(text(&r[1]));
            }
        }
        let columns = cols
            .iter()
            .map(|c| {
                let (oid, att) = c.as_ref()?;
                let oid = *oid as i64;
                Some((*index.get(&oid)?, attrs.get(&(oid, *att))?.clone()))
            })
            .collect();
        Ok(Source { columns, tables })
    }

    async fn my_source(&mut self, cols: Vec<Option<(String, String, String)>>) -> Result<Source, DbError> {
        let mut tables: Vec<SourceTable> = vec![];
        let mut columns = vec![];
        for c in cols {
            let Some((schema, table, column)) = c else {
                columns.push(None);
                continue;
            };
            let i = match tables.iter().position(|t| t.schema == schema && t.name == table) {
                Some(i) => i,
                None => {
                    tables.push(SourceTable { schema, name: table, primary_key: vec![] });
                    tables.len() - 1
                }
            };
            columns.push(Some((i, column)));
        }
        let d = self.dialect();
        for t in &mut tables {
            // An empty schema (no database in the column definition) is the current database.
            let rows = self
                .catalog(&format!(
                    "SELECT CAST(column_name AS CHAR) FROM information_schema.key_column_usage \
                     WHERE constraint_name = 'PRIMARY' AND table_schema = COALESCE(NULLIF({}, ''), DATABASE()) AND table_name = {} \
                     ORDER BY ordinal_position",
                    lit(d, &t.schema),
                    lit(d, &t.name)
                ))
                .await?;
            t.primary_key = rows.iter().map(|r| text(&r[0])).collect();
            if t.schema.is_empty() {
                t.schema = self.catalog("SELECT CAST(DATABASE() AS CHAR)").await?.first().map(|r| text(&r[0])).unwrap_or_default();
            }
        }
        Ok(Source { columns, tables })
    }
}
