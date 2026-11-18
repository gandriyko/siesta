//! Pure formatting of results: cell text, TSV / JSON / INSERT copies, hex dump, sorting, labels.

use std::{cmp::Ordering, time::Duration};

use db_exec::{Column, Outcome, Value};
use sql_split::Dialect;

pub fn size_label(n: usize) -> String {
    match n {
        0..1024 => format!("{n} B"),
        1024..1_048_576 => format!("{:.1} KB", n as f64 / 1024.),
        _ => format!("{:.1} MB", n as f64 / 1_048_576.),
    }
}

/// What a table cell draws: NULL as `null_text`, bytes as `<1.2 KB binary>`, JSON compact, text
/// on one line; only the first `max_bytes` of a value are used, the rest is `…`.
pub fn cell_display(v: &Value, null_text: &str, max_bytes: usize) -> String {
    let s = match v {
        Value::Null => return null_text.to_string(),
        Value::Bytes(b) => return format!("<{} binary>", size_label(b.len())),
        Value::Json(j) => j.to_string(),
        other => other.display(),
    };
    let cut = s.len() > max_bytes;
    let end = if cut { (0..=max_bytes).rev().find(|&i| s.is_char_boundary(i)).unwrap_or(0) } else { s.len() };
    let mut out: String = s[..end].chars().map(|c| if matches!(c, '\n' | '\r' | '\t') { ' ' } else { c }).collect();
    if cut {
        out.push('…');
    }
    out
}

/// The JSON of a cell, if it is one: a JSON value, or text that parses as an object / array.
pub fn cell_json(v: &Value) -> Option<std::borrow::Cow<'_, serde_json::Value>> {
    use std::borrow::Cow;
    match v {
        Value::Json(j) => Some(Cow::Borrowed(j)),
        Value::Text(s) if s.trim_start().starts_with(['{', '[']) => match serde_json::from_str::<serde_json::Value>(s) {
            Ok(j @ (serde_json::Value::Object(_) | serde_json::Value::Array(_))) => Some(Cow::Owned(j)),
            _ => None,
        },
        _ => None,
    }
}

/// The first `n` lines of the pretty-printed JSON; when it is longer, the last line ends with `…`.
/// (`n == 0` gives nothing.) Printing the whole document first is fine at page size; a huge document
/// per cell would want a streaming printer.
pub fn json_preview(j: &serde_json::Value, n: usize) -> Vec<String> {
    let pretty = serde_json::to_string_pretty(j).unwrap_or_default();
    let mut lines: Vec<String> = pretty.lines().take(n + 1).map(str::to_string).collect();
    if lines.len() > n {
        lines.truncate(n);
        if let Some(last) = lines.last_mut() {
            last.push('…');
        }
    }
    lines
}

/// Numbers are right-aligned: every non-null value among the first rows is a number.
pub fn is_numeric_column(rows: &[Vec<Value>], col: usize) -> bool {
    let mut seen = false;
    for v in rows.iter().take(200).map(|r| &r[col]) {
        match v {
            Value::Null => {}
            Value::Int(_) | Value::Float(_) | Value::Decimal(_) => seen = true,
            _ => return false,
        }
    }
    seen
}

fn tsv_field(s: &str) -> String {
    if s.contains(['\t', '\n', '\r', '"']) { format!("\"{}\"", s.replace('"', "\"\"")) } else { s.to_string() }
}

/// One cell for pasting into a spreadsheet: NULL is empty, awkward characters are quoted.
pub fn tsv_cell(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        v => tsv_field(&v.display()),
    }
}

pub fn tsv<'a>(rows: impl IntoIterator<Item = &'a [Value]>) -> String {
    rows.into_iter().map(|r| r.iter().map(tsv_cell).collect::<Vec<_>>().join("\t")).collect::<Vec<_>>().join("\n")
}

/// The row as a JSON object, columns in result order (a map would sort them).
pub fn row_json(cols: &[Column], row: &[Value]) -> String {
    let fields: Vec<String> = cols
        .iter()
        .zip(row)
        .map(|(c, v)| {
            let value = serde_json::to_string_pretty(&v.to_json()).unwrap_or_default().replace('\n', "\n  ");
            format!("  {}: {value}", serde_json::Value::String(c.name.clone()))
        })
        .collect();
    format!("{{\n{}\n}}", fields.join(",\n"))
}

pub fn literal(v: &Value, d: Dialect) -> String {
    let quote = |s: &str| {
        let s = s.replace('\'', "''");
        format!("'{}'", if d == Dialect::MySql { s.replace('\\', "\\\\") } else { s })
    };
    match v {
        Value::Null => "NULL".into(),
        Value::Bool(b) => b.to_string().to_uppercase(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) if f.is_finite() => f.to_string(),
        Value::Decimal(s) if s.parse::<f64>().is_ok_and(f64::is_finite) => s.clone(),
        Value::Json(j) => quote(&j.to_string()),
        Value::Bytes(b) if d == Dialect::MySql => format!("X'{}'", db_exec_hex(b)),
        Value::Bytes(b) => format!("'\\x{}'", db_exec_hex(b)),
        other => quote(&other.display()),
    }
}

fn db_exec_hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// `INSERT INTO table (cols) VALUES (…);` per row. The statement's table is not known, so the
/// caller names it.
pub fn insert_sql<'a>(table: &str, cols: &[Column], rows: impl IntoIterator<Item = &'a [Value]>, d: Dialect) -> String {
    let names = cols.iter().map(|c| db_exec::quote_ident(d, &c.name)).collect::<Vec<_>>().join(", ");
    rows.into_iter()
        .map(|r| format!("INSERT INTO {table} ({names}) VALUES ({});", r.iter().map(|v| literal(v, d)).collect::<Vec<_>>().join(", ")))
        .collect::<Vec<_>>()
        .join("\n")
}

/// `00000000  48 65 6c 6c 6f …  |Hello|` per 16 bytes, of the first `limit` bytes.
pub fn hexdump(b: &[u8], limit: usize) -> String {
    let shown = &b[..b.len().min(limit)];
    let mut out: String = shown
        .chunks(16)
        .enumerate()
        .map(|(i, line)| {
            let hex: Vec<String> = (0..16).map(|j| line.get(j).map_or("  ".into(), |x| format!("{x:02x}"))).collect();
            let text: String = line.iter().map(|&x| if (0x20..0x7f).contains(&x) { x as char } else { '.' }).collect();
            format!("{:08x}  {}  {}  |{text}|\n", i * 16, hex[..8].join(" "), hex[8..].join(" "))
        })
        .collect();
    if b.len() > limit {
        out.push_str(&format!("… {} more bytes\n", b.len() - limit));
    }
    out
}

fn number(v: &Value) -> Option<f64> {
    match v {
        Value::Int(i) => Some(*i as f64),
        Value::Float(f) => Some(*f),
        Value::Decimal(s) => s.parse().ok(),
        _ => None,
    }
}

/// Order of values of one column: numbers numerically, anything else as text; NULL is greatest.
pub fn cmp_values(a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Greater,
        (_, Value::Null) => Ordering::Less,
        _ => match (number(a), number(b)) {
            (Some(x), Some(y)) => x.partial_cmp(&y).unwrap_or(Ordering::Equal),
            _ => a.display().cmp(&b.display()),
        },
    }
}

/// Row order for an in-memory ("local") sort by `sort` = `(column name, descending)`.
pub fn sort_order(rows: &[Vec<Value>], cols: &[Column], sort: &[(String, bool)]) -> Vec<usize> {
    let keys: Vec<(usize, bool)> = sort.iter().filter_map(|(n, desc)| Some((cols.iter().position(|c| c.name == *n)?, *desc))).collect();
    let mut order: Vec<usize> = (0..rows.len()).collect();
    order.sort_by(|&x, &y| {
        keys.iter()
            .map(|&(c, desc)| {
                let o = cmp_values(&rows[x][c], &rows[y][c]);
                if desc { o.reverse() } else { o }
            })
            .find(|o| o.is_ne())
            .unwrap_or(Ordering::Equal)
    });
    order
}

/// Header click: ↑ → ↓ → none; with Shift the column is added to (or cycled within) the sort.
pub fn next_sort(sort: &[(String, bool)], col: &str, shift: bool) -> Vec<(String, bool)> {
    let at = sort.iter().position(|(c, _)| c == col);
    let mut out: Vec<(String, bool)> = if shift { sort.to_vec() } else { sort.iter().filter(|(c, _)| c == col).cloned().collect() };
    match at.map(|i| sort[i].1) {
        None => out.push((col.to_string(), false)),
        Some(false) => out.iter_mut().filter(|(c, _)| c == col).for_each(|s| s.1 = true),
        Some(true) => out.retain(|(c, _)| c != col),
    }
    out
}

pub fn duration_label(d: Duration) -> String {
    match d.as_millis() {
        0..1000 => format!("{} ms", d.as_millis()),
        _ => format!("{:.2} s", d.as_secs_f64()),
    }
}

/// `12 rows`, `3 rows affected`; a page with more behind it or a cut result says so.
pub fn rows_label(o: &Outcome) -> String {
    let plural = |n: usize| if n == 1 { "" } else { "s" };
    if let (Some(n), true) = (o.rows_affected, o.columns.is_empty()) {
        return format!("{n} row{} affected", plural(n as usize));
    }
    let n = o.rows.len();
    let more = if o.has_more { " (more)" } else if o.truncated { " (cut at the row limit)" } else { "" };
    format!("{n} row{}{more}", plural(n))
}

/// The footer's `1 / ?`.
pub fn page_label(offset: u64, size: u64, count: Option<u64>) -> String {
    let page = offset / size.max(1) + 1;
    match count {
        Some(n) => format!("{page} / {}", n.div_ceil(size.max(1)).max(1)),
        None => format!("{page} / ?"),
    }
}

/// The connection colours the form offers: (name, on a dark theme, on a light theme). One family of
/// steps (Tailwind 400 on dark, 600 on light), so they sit together and each reads on its background.
pub const COLORS: [(&str, u32, u32); 16] = [
    ("red", 0xf87171, 0xdc2626),
    ("orange", 0xfb923c, 0xea580c),
    ("amber", 0xfbbf24, 0xd97706),
    ("yellow", 0xfacc15, 0xca8a04),
    ("lime", 0xa3e635, 0x65a30d),
    ("green", 0x4ade80, 0x16a34a),
    ("emerald", 0x34d399, 0x059669),
    ("teal", 0x2dd4bf, 0x0d9488),
    ("cyan", 0x22d3ee, 0x0891b2),
    ("sky", 0x38bdf8, 0x0284c7),
    ("blue", 0x60a5fa, 0x2563eb),
    ("indigo", 0x818cf8, 0x4f46e5),
    ("purple", 0xc084fc, 0x9333ea),
    ("fuchsia", 0xe879f9, 0xc026d3),
    ("pink", 0xf472b6, 0xdb2777),
    ("gray", 0x9ca3af, 0x6b7280),
];

/// A colour name from [`COLORS`] (in the shade for a `dark` or light theme) or `#rrggbb`, for the connection's `color`.
pub fn parse_color(s: &str, dark: bool) -> Option<gpui::Hsla> {
    let lower = s.trim().to_lowercase();
    let name = if lower == "grey" { "gray" } else { lower.as_str() };
    if let Some(&(_, d, l)) = COLORS.iter().find(|(n, ..)| *n == name) {
        return Some(gpui::rgb(if dark { d } else { l }).into());
    }
    let hex = name.trim_start_matches('#');
    (hex.len() == 6).then(|| u32::from_str_radix(hex, 16).ok()).flatten().map(|v| gpui::rgb(v).into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(n: &str) -> Column {
        Column { name: n.into(), type_name: "text".into() }
    }

    #[test]
    fn cell_text() {
        assert_eq!(cell_display(&Value::Null, "NULL", 100), "NULL");
        assert_eq!(cell_display(&Value::Null, "∅", 100), "∅");
        assert_eq!(cell_display(&Value::Int(42), "NULL", 100), "42");
        assert_eq!(cell_display(&Value::Bytes(vec![0; 1229]), "NULL", 100), "<1.2 KB binary>");
        assert_eq!(cell_display(&Value::Json(serde_json::json!({"a": [1, 2], "b": "x"})), "NULL", 100), r#"{"a":[1,2],"b":"x"}"#);
        assert_eq!(cell_display(&Value::Text("a\nb\tc".into()), "NULL", 100), "a b c");
        // Cut at a char boundary: `é` is two bytes.
        assert_eq!(cell_display(&Value::Text("aé".repeat(10)), "NULL", 4), "aéa…");
        assert!(is_numeric_column(&[vec![Value::Null], vec![Value::Int(1)], vec![Value::Decimal("1.5".into())]], 0));
        assert!(!is_numeric_column(&[vec![Value::Int(1)], vec![Value::Text("x".into())]], 0));
        assert!(!is_numeric_column(&[vec![Value::Null]], 0));
    }

    #[test]
    fn copy_formats() {
        let cols = [col("id"), col("note"), col("data")];
        let rows = [
            vec![Value::Int(1), Value::Text("it's".into()), Value::Json(serde_json::json!({"k": 1}))],
            vec![Value::Int(2), Value::Null, Value::Bytes(vec![0xde, 0xad])],
        ];
        let refs: Vec<&[Value]> = rows.iter().map(Vec::as_slice).collect();
        assert_eq!(tsv(refs.iter().copied()), "1\tit's\t\"{\"\"k\"\":1}\"\n2\t\t\\xdead"); // quotes in a field are doubled and the field quoted
        assert_eq!(tsv_cell(&Value::Text("a\tb \"q\"".into())), "\"a\tb \"\"q\"\"\"");
        let sql = insert_sql("t", &cols, refs.iter().copied(), Dialect::Postgres);
        assert_eq!(
            sql,
            "INSERT INTO t (\"id\", \"note\", \"data\") VALUES (1, 'it''s', '{\"k\":1}');\nINSERT INTO t (\"id\", \"note\", \"data\") VALUES (2, NULL, '\\xdead');"
        );
        assert!(insert_sql("t", &cols[..1], [&rows[1][..1]], Dialect::MySql).starts_with("INSERT INTO t (`id`) VALUES (2);"));
        assert_eq!(literal(&Value::Bytes(vec![1, 2]), Dialect::MySql), "X'0102'");
        assert_eq!(literal(&Value::Text("a\\b".into()), Dialect::MySql), "'a\\\\b'");
        assert_eq!(literal(&Value::Bool(true), Dialect::Postgres), "TRUE");
        // Columns keep result order, nested values are indented.
        let json = row_json(&cols, &rows[0]);
        assert_eq!(json, "{\n  \"id\": 1,\n  \"note\": \"it's\",\n  \"data\": {\n    \"k\": 1\n  }\n}");
        assert!(serde_json::from_str::<serde_json::Value>(&json).is_ok());
    }

    #[test]
    fn hex_dump() {
        let d = hexdump(b"Hello, world! 0123456789", 20);
        assert_eq!(d.lines().next().unwrap(), "00000000  48 65 6c 6c 6f 2c 20 77  6f 72 6c 64 21 20 30 31  |Hello, world! 01|");
        assert!(d.ends_with("… 4 more bytes\n"));
        assert_eq!(d.lines().count(), 3);
    }

    #[test]
    fn sort_cycle() {
        let s = |v: &[(&str, bool)]| v.iter().map(|(c, d)| (c.to_string(), *d)).collect::<Vec<_>>();
        let a = next_sort(&[], "a", false);
        assert_eq!(a, s(&[("a", false)]));
        let a = next_sort(&a, "a", false);
        assert_eq!(a, s(&[("a", true)]));
        assert_eq!(next_sort(&a, "a", false), s(&[]));
        // Shift adds a column, keeps the others; a plain click on another column replaces.
        let ab = next_sort(&s(&[("a", true)]), "b", true);
        assert_eq!(ab, s(&[("a", true), ("b", false)]));
        assert_eq!(next_sort(&ab, "b", true), s(&[("a", true), ("b", true)]));
        assert_eq!(next_sort(&s(&[("a", true), ("b", true)]), "b", true), s(&[("a", true)]));
        assert_eq!(next_sort(&ab, "b", false), s(&[("b", true)]));
    }

    #[test]
    fn local_sort() {
        let rows = vec![
            vec![Value::Int(10), Value::Text("b".into())],
            vec![Value::Null, Value::Text("a".into())],
            vec![Value::Int(9), Value::Text("a".into())],
        ];
        let cols = [col("n"), col("s")];
        assert_eq!(sort_order(&rows, &cols, &[("n".into(), false)]), [2, 0, 1]); // 9 < 10 (not as text), NULL last
        assert_eq!(sort_order(&rows, &cols, &[("n".into(), true)]), [1, 0, 2]);
        assert_eq!(sort_order(&rows, &cols, &[("s".into(), false), ("n".into(), true)]), [1, 2, 0]);
        assert_eq!(sort_order(&rows, &cols, &[]), [0, 1, 2]);
    }

    #[test]
    fn labels() {
        let mut o = Outcome {
            columns: vec![],
            rows: vec![],
            wrapped: false,
            has_more: false,
            truncated: false,
            rows_affected: Some(3),
            notices: vec![],
            executed_sql: String::new(),
            params: vec![],
            in_transaction: false,
            duration: Duration::ZERO,
        };
        assert_eq!(rows_label(&o), "3 rows affected");
        o.rows_affected = Some(1);
        assert_eq!(rows_label(&o), "1 row affected");
        o.rows_affected = None;
        o.columns = vec![col("a")];
        o.rows = vec![vec![Value::Int(1)]; 12];
        assert_eq!(rows_label(&o), "12 rows");
        o.has_more = true;
        assert_eq!(rows_label(&o), "12 rows (more)");
        assert_eq!(page_label(200, 100, None), "3 / ?");
        assert_eq!(page_label(0, 100, Some(250)), "1 / 3");
        assert_eq!(page_label(0, 100, Some(0)), "1 / 1");
        assert_eq!(duration_label(Duration::from_millis(12)), "12 ms");
        assert_eq!(duration_label(Duration::from_millis(3200)), "3.20 s");
        let doc = serde_json::json!({"a": [1, 2], "b": {"c": null}});
        assert_eq!(json_preview(&doc, 2), ["{", "  \"a\": […"]);
        assert_eq!(json_preview(&serde_json::json!([1]), 4), ["[", "  1", "]"]); // fits: no ellipsis
        assert_eq!(json_preview(&serde_json::json!([1]), 3), ["[", "  1", "]"]);
        assert_eq!(json_preview(&serde_json::json!([1]), 2), ["[", "  1…"]);
        assert!(json_preview(&doc, 0).is_empty());
        assert_eq!(cell_json(&Value::Text(" {\"a\":1}".into())).as_deref(), Some(&serde_json::json!({"a": 1})));
        assert!(cell_json(&Value::Text("123".into())).is_none() && cell_json(&Value::Text("{x".into())).is_none() && cell_json(&Value::Int(1)).is_none());
        assert!(cell_json(&Value::Json(serde_json::json!(5))).is_some());
        assert!(parse_color("#ff0000", true).is_some() && parse_color("Grey", false).is_some() && parse_color("nope", true).is_none());
        assert_ne!(parse_color("red", true), parse_color("red", false));
    }
}
