//! Output formats: aligned table, JSON (`siesta.run/v1`), JSON lines, CSV.

use std::io::{IsTerminal, Write};

use clap::ValueEnum;
use db_exec::{DbError, Outcome};
use serde_json::{Value, json};

/// Table cells longer than this are cut (JSON and CSV keep the full value).
const MAX_CELL: usize = 60;

#[derive(ValueEnum, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Format {
    Table,
    Json,
    Jsonl,
    Csv,
}

impl Format {
    /// `table` on a terminal, `json` in a pipe.
    pub fn resolve(f: Option<Format>) -> Format {
        f.unwrap_or(if std::io::stdout().is_terminal() { Format::Table } else { Format::Json })
    }

    /// Errors go to stderr as text for the human formats, to stdout as JSON otherwise.
    pub fn human(self) -> bool {
        matches!(self, Format::Table | Format::Csv)
    }
}

pub(crate) const RED: &str = "31";
pub(crate) const YELLOW: &str = "33";
pub(crate) const DIM: &str = "2";

/// ANSI colour when writing to a terminal (stderr when `err`).
pub(crate) fn paint(s: &str, code: &str, err: bool) -> String {
    let tty = if err { std::io::stderr().is_terminal() } else { std::io::stdout().is_terminal() };
    if tty && std::env::var_os("NO_COLOR").is_none() { format!("\x1b[{code}m{s}\x1b[0m") } else { s.to_string() }
}

/// One cell for the table: single line, cut to [`MAX_CELL`] characters.
fn shorten(s: &str) -> String {
    let one: String = s.chars().filter(|&c| c != '\r').map(|c| if c == '\n' { '⏎' } else if c == '\t' { ' ' } else { c }).collect();
    if one.chars().count() > MAX_CELL { format!("{}…", one.chars().take(MAX_CELL - 1).collect::<String>()) } else { one }
}

/// Aligned columns, `|` between them, a rule under the header.
pub(crate) fn render_table(cols: &[String], rows: &[Vec<String>]) -> String {
    let cells: Vec<Vec<String>> = rows.iter().map(|r| r.iter().map(|c| shorten(c)).collect()).collect();
    let widths: Vec<usize> =
        cols.iter().enumerate().map(|(i, c)| cells.iter().filter_map(|r| r.get(i)).map(|c| c.chars().count()).max().unwrap_or(0).max(c.chars().count())).collect();
    let line = |r: &[String]| r.iter().zip(&widths).map(|(c, w)| format!("{c:<w$}")).collect::<Vec<_>>().join(" | ").trim_end().to_string();
    let rule = widths.iter().map(|w| "-".repeat(*w)).collect::<Vec<_>>().join("-+-");
    let mut out = vec![line(cols), rule];
    out.extend(cells.iter().map(|r| line(r)));
    out.join("\n")
}

/// RFC 4180: fields with `,` `"` or a line break are quoted.
pub(crate) fn render_csv(cols: &[String], rows: &[Vec<String>]) -> String {
    let field = |s: &str| if s.contains([',', '"', '\n', '\r']) { format!("\"{}\"", s.replace('"', "\"\"")) } else { s.to_string() };
    let line = |r: &[String]| r.iter().map(|c| field(c)).collect::<Vec<_>>().join(",");
    std::iter::once(line(cols)).chain(rows.iter().map(|r| line(r))).collect::<Vec<_>>().join("\n")
}

/// Plain-text writes: a closed pipe (`| head`) is not an error worth reporting.
pub(crate) fn put(out: &mut dyn Write, s: impl AsRef<str>) {
    let _ = writeln!(out, "{}", s.as_ref());
}

/// A list of JSON objects as a table / CSV (columns = `keys` that have a value anywhere),
/// a JSON array, or JSON lines.
pub(crate) fn emit_objects(out: &mut dyn Write, format: Format, keys: &[&str], objs: &[Value]) {
    match format {
        Format::Json => put(out, Value::Array(objs.to_vec()).to_string()),
        Format::Jsonl => objs.iter().for_each(|o| put(out, o.to_string())),
        Format::Table | Format::Csv => {
            let keys: Vec<&str> = keys.iter().copied().filter(|k| objs.iter().any(|o| !o[*k].is_null())).collect();
            let null = if format == Format::Csv { "" } else { "NULL" };
            let cols: Vec<String> = keys.iter().map(|k| k.to_string()).collect();
            let rows: Vec<Vec<String>> = objs
                .iter()
                .map(|o| {
                    keys.iter()
                        .map(|k| match &o[*k] {
                            Value::Null => null.to_string(),
                            Value::String(s) => s.clone(),
                            v => v.to_string(),
                        })
                        .collect()
                })
                .collect();
            if cols.is_empty() {
                return;
            }
            put(out, if format == Format::Csv { render_csv(&cols, &rows) } else { render_table(&cols, &rows) });
        }
    }
}

/// One executed statement.
pub(crate) struct Item<'a> {
    pub file: Option<&'a str>,
    pub name: Option<&'a str>,
    pub line: usize,
    pub connection: &'a str,
    /// The effective database; `None` when it is the server's default, which is not known here.
    pub database: Option<&'a str>,
    pub result: &'a Result<Outcome, DbError>,
    /// The row limit that was applied.
    pub max_rows: usize,
}

pub(crate) fn error_json(e: &DbError) -> Value {
    json!({
        "code": if e.connection_lost { "connection_lost" } else if e.refused { "read_only_refused" } else { "sql_error" },
        "sqlstate": e.code, "message": e.message, "detail": e.detail, "hint": e.hint, "position": e.position,
    })
}

/// `siesta.run/v1`.
pub(crate) fn item_json(i: &Item) -> Value {
    let mut v = json!({
        "schema": "siesta.run/v1",
        "file": i.file,
        "statement": { "name": i.name, "line": i.line },
        "connection": i.connection,
        "database": i.database,
    });
    match i.result {
        Err(e) => v["error"] = error_json(e),
        Ok(o) => {
            v["columns"] = o.columns.iter().map(|c| json!({ "name": c.name, "type": c.type_name })).collect();
            v["rows"] = o.rows.iter().map(|r| r.iter().map(|c| c.to_json()).collect::<Value>()).collect();
            v["rows_truncated"] = json!(o.has_more || o.truncated);
            v["rows_affected"] = json!(o.rows_affected);
            v["duration_ms"] = json!(o.duration.as_millis() as u64);
            v["notices"] = json!(o.notices);
        }
    }
    v
}

/// Table or CSV. `heading` (several statements) goes above the result. Errors are printed
/// by the caller to stderr.
pub(crate) fn item_text(i: &Item, format: Format, heading: Option<String>, out: &mut dyn Write) {
    if let Some(h) = heading {
        put(out, paint(&h, DIM, false));
    }
    let Ok(o) = i.result else { return };
    if !o.columns.is_empty() {
        let cols: Vec<String> = o.columns.iter().map(|c| c.name.clone()).collect();
        let null = if format == Format::Csv { "" } else { "NULL" };
        let rows: Vec<Vec<String>> = o.rows.iter().map(|r| r.iter().map(|v| if matches!(v, db_exec::Value::Null) { null.to_string() } else { v.display() }).collect()).collect();
        put(out, if format == Format::Csv { render_csv(&cols, &rows) } else { render_table(&cols, &rows) });
    }
    if format == Format::Csv {
        return;
    }
    let ms = o.duration.as_millis();
    let cut = if o.has_more || o.truncated { format!(", truncated at --max-rows {}", i.max_rows) } else { String::new() };
    let summary = match (o.columns.is_empty(), o.rows_affected) {
        (false, _) => format!("({} row{}{cut}, {ms} ms)", o.rows.len(), if o.rows.len() == 1 { "" } else { "s" }),
        (true, Some(n)) => format!("{n} row{} affected, {ms} ms", if n == 1 { "" } else { "s" }),
        (true, None) => format!("OK, {ms} ms"),
    };
    put(out, paint(&summary, DIM, false));
    for n in &o.notices {
        eprintln!("{} {n}", paint("notice:", YELLOW, true));
    }
}

/// `error: 42P01: message` plus detail / hint, for the human formats.
pub(crate) fn print_sql_error(e: &DbError) {
    eprintln!("{} {e}", paint("error:", RED, true));
    if let Some(d) = &e.detail {
        eprintln!("  detail: {d}");
    }
    if let Some(h) = &e.hint {
        eprintln!("  hint: {h}");
    }
    if let Some(p) = e.position {
        eprintln!("  position: {p}");
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use db_exec::{Column, Value};

    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    fn outcome() -> Outcome {
        Outcome {
            columns: vec![Column { name: "id".into(), type_name: "int8".into() }, Column { name: "payload".into(), type_name: "jsonb".into() }],
            rows: vec![vec![Value::Int(1), Value::Json(json!({"ip": "10.0.0.1", "tags": [1, 2]}))], vec![Value::Int(2), Value::Null]],
            wrapped: true,
            has_more: true,
            truncated: false,
            rows_affected: None,
            notices: vec![],
            executed_sql: String::new(),
            params: vec![],
            in_transaction: false,
            duration: Duration::from_millis(38),
            raw: None,
        }
    }

    #[test]
    fn table_aligns_and_cuts() {
        let long = "x".repeat(100);
        let t = render_table(&s(&["id", "name"]), &[s(&["1", "ann"]), s(&["22", &long]), s(&["3", "a\nb"])]);
        let lines: Vec<&str> = t.lines().collect();
        assert_eq!(lines[0], "id | name");
        assert_eq!(lines[1], format!("---+-{}", "-".repeat(MAX_CELL)));
        assert_eq!(lines[2], "1  | ann");
        assert_eq!(lines[3].chars().count(), "22 | ".len() + MAX_CELL);
        assert!(lines[3].ends_with('…'));
        assert_eq!(lines[4], "3  | a⏎b");
    }

    #[test]
    fn csv_quotes() {
        assert_eq!(render_csv(&s(&["a", "b"]), &[s(&["x,y", "q\"r"]), s(&["", "l1\nl2"])]), "a,b\n\"x,y\",\"q\"\"r\"\n,\"l1\nl2\"");
    }

    #[test]
    fn json_item_nests_json_values() {
        let r = Ok(outcome());
        let v = item_json(&Item { file: Some("a.sql"), name: Some("q"), line: 3, connection: "dev", database: Some("app"), result: &r, max_rows: 2 });
        assert_eq!(v["schema"], "siesta.run/v1");
        assert_eq!(v["database"], "app");
        assert_eq!(v["statement"], json!({"name": "q", "line": 3}));
        assert_eq!(v["columns"][1], json!({"name": "payload", "type": "jsonb"}));
        assert_eq!(v["rows"], json!([[1, {"ip": "10.0.0.1", "tags": [1, 2]}], [2, null]]));
        assert_eq!((&v["rows_truncated"], &v["rows_affected"], &v["duration_ms"]), (&json!(true), &serde_json::Value::Null, &json!(38)));
    }

    #[test]
    fn json_item_error() {
        let r = Err(DbError { code: Some("42P01".into()), message: "no table".into(), ..Default::default() });
        let v = item_json(&Item { file: None, name: None, line: 1, connection: "dev", database: None, result: &r, max_rows: 1 });
        assert_eq!(v["error"]["sqlstate"], "42P01");
        assert_eq!(v["error"]["code"], "sql_error");
        assert!(v.get("rows").is_none());
        let r = Err(DbError { message: "no".into(), refused: true, ..Default::default() });
        let v = item_json(&Item { file: None, name: None, line: 1, connection: "dev", database: None, result: &r, max_rows: 1 });
        assert_eq!(v["error"]["code"], "read_only_refused");
    }

    #[test]
    fn text_item_shows_null_and_truncation() {
        let r = Ok(outcome());
        let item = Item { file: None, name: None, line: 1, connection: "dev", database: None, result: &r, max_rows: 2 };
        let mut buf = vec![];
        item_text(&item, Format::Table, None, &mut buf);
        let text = String::from_utf8(buf).unwrap();
        assert!(text.contains("2  | NULL"), "{text}");
        assert!(text.contains("(2 rows, truncated at --max-rows 2, 38 ms)"), "{text}");
        let mut buf = vec![];
        item_text(&item, Format::Csv, None, &mut buf);
        assert_eq!(String::from_utf8(buf).unwrap(), "id,payload\n1,\"{\"\"ip\"\":\"\"10.0.0.1\"\",\"\"tags\"\":[1,2]}\"\n2,\n");
    }

    #[test]
    fn objects_drop_empty_columns() {
        let objs = [json!({"a": 1, "b": null}), json!({"a": "x", "b": null})];
        let mut buf = vec![];
        emit_objects(&mut buf, Format::Table, &["a", "b"], &objs);
        assert_eq!(String::from_utf8(buf).unwrap(), "a\n-\n1\nx\n");
        let mut buf = vec![];
        emit_objects(&mut buf, Format::Jsonl, &["a"], &objs);
        assert_eq!(String::from_utf8(buf).unwrap().lines().count(), 2);
    }
}
