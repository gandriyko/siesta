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

/// The rows as a pretty JSON array of [`row_json`] objects (the result panel's JSON tab).
pub fn rows_json(cols: &[Column], rows: &[Vec<Value>]) -> String {
    if rows.is_empty() {
        return "[]".into();
    }
    let items: Vec<String> = rows.iter().map(|r| format!("  {}", row_json(cols, r).replace('\n', "\n  "))).collect();
    format!("[\n{}\n]", items.join(",\n"))
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

/// The file with each statement laid out; comments, directives and blank lines between
/// statements stay as written. SQL goes through `sqlformat` (keyword case kept), except a
/// statement with a dollar quote (`$$ … $$`, `$tag$`): `sqlformat` rewrites the text inside it.
/// An `.es` request gets its JSON body indented (an NDJSON or commented body stays); a mongosh
/// call gets its arguments indented, kept only when it parses to the same call.
pub fn format_file(text: &str, dialect: Dialect) -> String {
    let options = sqlformat::FormatOptions {
        dialect: if dialect == Dialect::Postgres { sqlformat::Dialect::PostgreSql } else { sqlformat::Dialect::Generic },
        ..Default::default()
    };
    let dollar_quote = |s: &str| s.match_indices('$').any(|(i, _)| !s[i + 1..].starts_with(|c: char| c.is_ascii_digit()));
    let one = |st: &str| -> Option<String> {
        match dialect {
            Dialect::Postgres | Dialect::MySql => (!dollar_quote(st)).then(|| sqlformat::format(st, &sqlformat::QueryParams::None, &options).trim().to_string()),
            Dialect::Elastic => {
                let (line, body) = st.split_once('\n')?;
                // One JSON value only: NDJSON (`_bulk`), comments and `"""` stay as written.
                serde_json::from_str::<serde_json::Value>(body).ok()?;
                Some(format!("{}\n{}", line.trim_end(), pretty_js(body)?))
            }
            Dialect::Mongo => {
                let new = pretty_js(st)?;
                (sql_split::mongo_parse(&new).ok()? == sql_split::mongo_parse(st).ok()?).then_some(new)
            }
        }
    };
    let (mut out, mut at) = (String::with_capacity(text.len()), 0);
    for st in sql_split::split(text, dialect).statements {
        let src = st.text(text);
        out.push_str(&text[at..st.range.start]);
        out.push_str(&one(src).unwrap_or_else(|| src.to_string()));
        at = st.range.end;
    }
    out.push_str(&text[at..]);
    out
}

#[derive(Clone, Copy, PartialEq)]
enum Tok<'a> {
    /// `{ } [ ] ( ) , : ;`
    Punct(u8),
    /// A word, number, string or regex literal: written as is.
    Atom(&'a str),
    /// `// …` (to the line end) or `/* … */`.
    Comment(&'a str),
}

/// JavaScript / JSON literals split into tokens; whitespace only separates atoms. `None` for an
/// unterminated string, regex or comment.
fn js_tokens(s: &str) -> Option<Vec<(Tok<'_>, bool)>> {
    let b = s.as_bytes();
    // The bool: whitespace came before the token (two atoms keep one space between them).
    let (mut out, mut i, mut space) = (Vec::new(), 0, false);
    let close = |from: usize, end: u8| -> Option<usize> {
        let mut j = from;
        let mut class = false;
        while j < b.len() {
            match b[j] {
                b'\\' => j += 1,
                b'[' if end == b'/' => class = true,
                b']' if end == b'/' => class = false,
                c if c == end && !class => return Some(j + 1),
                _ => {}
            }
            j += 1;
        }
        None
    };
    while i < b.len() {
        let c = b[i];
        let start = i;
        let tok = match c {
            c if c.is_ascii_whitespace() => {
                space = true;
                i += 1;
                continue;
            }
            b'{' | b'}' | b'[' | b']' | b'(' | b')' | b',' | b':' | b';' => {
                i += 1;
                Tok::Punct(c)
            }
            b'/' if b.get(i + 1) == Some(&b'/') => {
                i = s[i..].find('\n').map_or(b.len(), |n| i + n);
                Tok::Comment(s[start..i].trim_end())
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i = s[i + 2..].find("*/").map(|n| i + n + 4)?;
                Tok::Comment(&s[start..i])
            }
            // A regex where a value starts: `{re: /a,b/i}`.
            b'/' if matches!(out.iter().rev().find(|(t, _)| !matches!(t, Tok::Comment(_))), None | Some((Tok::Punct(b'(' | b',' | b':' | b'[' | b'{'), _))) => {
                i = close(i + 1, b'/')?;
                while b.get(i).is_some_and(u8::is_ascii_alphabetic) {
                    i += 1;
                }
                Tok::Atom(&s[start..i])
            }
            b'"' | b'\'' | b'`' => {
                i = close(i + 1, c)?;
                Tok::Atom(&s[start..i])
            }
            _ => {
                while i < b.len() && !b[i].is_ascii_whitespace() && !b"{}[](),:;\"'`".contains(&b[i]) && !s[i..].starts_with("//") && !s[i..].starts_with("/*") {
                    i += 1;
                }
                Tok::Atom(&s[start..i])
            }
        };
        out.push((tok, space));
        space = false;
    }
    Some(out)
}

/// Objects and arrays that fit this many characters stay on one line.
const INLINE_WIDTH: usize = 72;

/// JavaScript / JSON literals re-indented by two spaces: an object or array that fits
/// [`INLINE_WIDTH`] stays on one line (`{ a: 1, b: [1, 2] }`), a longer one gets one entry per
/// line. Only whitespace outside strings, regexes and comments changes.
fn pretty_js(s: &str) -> Option<String> {
    let toks = js_tokens(s)?;
    // The matching close of each open bracket.
    let mut close = vec![0; toks.len()];
    let mut stack = Vec::new();
    for (i, (t, _)) in toks.iter().enumerate() {
        match t {
            Tok::Punct(b'{' | b'[' | b'(') => stack.push(i),
            Tok::Punct(c @ (b'}' | b']' | b')')) => {
                let o = stack.pop()?;
                (toks[o].0 == Tok::Punct(match c { b'}' => b'{', b']' => b'[', _ => b'(' })).then_some(())?;
                close[o] = i;
            }
            _ => {}
        }
    }
    stack.is_empty().then_some(())?;
    let mut out = String::new();
    write_js(&toks, &close, 0..toks.len(), None, &mut out, 0);
    Some(out.trim_end().to_string())
}

/// Writes `toks[range]`; `broken` is the indent of the enclosing multi-line object or array
/// (its commas break lines), `None` inside parentheses, at the top or on one line.
fn write_js(toks: &[(Tok, bool)], close: &[usize], range: std::ops::Range<usize>, broken: Option<usize>, out: &mut String, ind: usize) {
    let nl = |out: &mut String, ind: usize| {
        out.truncate(out.trim_end_matches(' ').len());
        out.push('\n');
        out.push_str(&"  ".repeat(ind));
    };
    let mut k = range.start;
    while k < range.end {
        let (t, space) = toks[k];
        match t {
            Tok::Punct(o @ (b'{' | b'[')) => {
                let m = close[k];
                let mut flat = String::new();
                let has_comment = toks[k..m].iter().any(|(t, _)| matches!(t, Tok::Comment(_)));
                if !has_comment {
                    write_js(toks, close, k + 1..m, None, &mut flat, 0);
                }
                let pad = if o == b'{' && !flat.is_empty() { " " } else { "" };
                let c = if o == b'{' { '}' } else { ']' };
                if !has_comment && !flat.contains('\n') && flat.len() <= INLINE_WIDTH {
                    out.push(o as char);
                    out.push_str(pad);
                    out.push_str(&flat);
                    out.push_str(pad);
                    out.push(c);
                } else {
                    out.push(o as char);
                    nl(out, ind + 1);
                    write_js(toks, close, k + 1..m, Some(ind + 1), out, ind + 1);
                    nl(out, ind);
                    out.push(c);
                }
                k = m;
            }
            Tok::Punct(b'(') => {
                out.push('(');
                write_js(toks, close, k + 1..close[k], None, out, ind);
                out.push(')');
                k = close[k];
            }
            Tok::Punct(b',') => match broken {
                Some(i) => {
                    out.push(',');
                    nl(out, i);
                }
                None => out.push_str(", "),
            },
            Tok::Punct(b':') => out.push_str(": "),
            Tok::Punct(c) => out.push(c as char),
            Tok::Atom(a) => {
                if space && k > range.start && matches!(toks[k - 1].0, Tok::Atom(_)) {
                    out.push(' ');
                }
                out.push_str(a);
            }
            Tok::Comment(c) => {
                if !out.is_empty() && !out.ends_with([' ', '\n']) {
                    out.push(' ');
                }
                out.push_str(c);
                if c.starts_with("//") {
                    nl(out, broken.unwrap_or(ind));
                }
            }
        }
        k += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_sql_file() {
        let src = "-- @connection dev\n-- @database app\nselect a, b from t where x = 1 and y in (select id from u);\n\n\n-- name: second\nupdate t set a = 'x;y' where id = $1;\nSELECT $$ a ; b $$, arr[1] FROM t";
        let out = format_file(src, Dialect::Postgres);
        assert!(out.starts_with("-- @connection dev\n-- @database app\nselect\n  a,\n  b\nfrom\n  t\nwhere"), "{out}");
        assert!(out.contains("\n\n\n-- name: second\nupdate\n") && out.contains("id = $1;\nSELECT $$ a ; b $$, arr[1] FROM t"), "{out}");
        assert_eq!(sql_split::split(&out, Dialect::Postgres).statements.len(), 3);
        assert_eq!(format_file(&out, Dialect::Postgres), out, "stable");
    }

    #[test]
    fn format_es_file() {
        let src = "# @connection es\nGET /i/_search\n{\"query\":{\"bool\":{\"must\":[{\"match\":{\"name\":\"a, b: {c}\"}},{\"range\":{\"age\":{\"gte\":40,\"lte\":50}}}]}},\"size\":5}\n\nPOST /_bulk\n{\"index\":{}}\n{\"a\":1}\n\nGET /_cat/indices\n";
        let out = format_file(src, Dialect::Elastic);
        let want = "# @connection es\nGET /i/_search\n{\n  \"query\": {\n    \"bool\": {\n      \"must\": [\n        { \"match\": { \"name\": \"a, b: {c}\" } },\n        { \"range\": { \"age\": { \"gte\": 40, \"lte\": 50 } } }\n      ]\n    }\n  },\n  \"size\": 5\n}\n\nPOST /_bulk\n{\"index\":{}}\n{\"a\":1}\n\nGET /_cat/indices\n";
        assert_eq!(out, want);
        assert_eq!(format_file(&out, Dialect::Elastic), out, "stable");
    }

    #[test]
    fn format_mongo_file() {
        let src = "// @connection m\ndb.users.find({name:'x, y',re:/a,b\\/[/]/i,age:{$gt:40},tags:{$in:['a','b','c','d','e','f','g','h','i','j','k','l']}} , {_id:0})\n  .sort({age:-1}) // newest\n  .limit(5)\n\ndb.users.aggregate([{$match:{a:1}},{$group:{_id:'$b',n:{$sum:1}}}]);\nshow collections\n";
        let out = format_file(src, Dialect::Mongo);
        let want = "// @connection m\ndb.users.find({\n  name: 'x, y',\n  re: /a,b\\/[/]/i,\n  age: { $gt: 40 },\n  tags: { $in: ['a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j', 'k', 'l'] }\n}, { _id: 0 }).sort({ age: -1 }) // newest\n.limit(5)\n\ndb.users.aggregate([{ $match: { a: 1 } }, { $group: { _id: '$b', n: { $sum: 1 } } }]);\nshow collections\n";
        assert_eq!(out, want);
        assert_eq!(format_file(&out, Dialect::Mongo), out, "stable");
    }

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
        let all = rows_json(&cols, &rows[..1]);
        assert_eq!(all, "[\n  {\n    \"id\": 1,\n    \"note\": \"it's\",\n    \"data\": {\n      \"k\": 1\n    }\n  }\n]");
        assert_eq!(serde_json::from_str::<serde_json::Value>(&rows_json(&cols, &rows)).unwrap().as_array().unwrap().len(), rows.len());
        assert_eq!(rows_json(&cols, &[]), "[]");
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
            raw: None,
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
