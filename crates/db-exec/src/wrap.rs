//! Pure SQL building: page wrapper (`SELECT * FROM (<sql>) AS q …`) and the column-filter syntax.
//! Filter values only ever travel as bind parameters (`$n` / `?`).

use sql_split::Dialect;

use crate::Column;

/// What the GUI asks for: one page of a wrapped query.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    pub offset: u64,
    pub size: u64,
    /// `(column, descending)`, in priority order.
    pub sort: Vec<(String, bool)>,
    /// `(column, filter text)`; see [`parse_filter`]. Empty texts are ignored.
    pub filters: Vec<(String, String)>,
}

impl Default for Page {
    fn default() -> Self {
        Page { offset: 0, size: 100, sort: Vec::new(), filters: Vec::new() }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cond {
    Contains(String),
    Cmp(Op, String),
    IsNull,
    NotNull,
    In(Vec<String>),
}

/// `foo` contains; `=42` `!=0` `>10` `>=` `<` `<=2026-01-01`; `null` / `!null`; `a,b,c` IN.
/// `None` for empty input (and for an operator with no value yet).
pub fn parse_filter(input: &str) -> Option<Cond> {
    let t = input.trim();
    if t.is_empty() {
        return None;
    }
    if t.eq_ignore_ascii_case("null") {
        return Some(Cond::IsNull);
    }
    if t.eq_ignore_ascii_case("!null") {
        return Some(Cond::NotNull);
    }
    for (prefix, op) in [("!=", Op::Ne), (">=", Op::Ge), ("<=", Op::Le), ("=", Op::Eq), (">", Op::Gt), ("<", Op::Lt)] {
        if let Some(rest) = t.strip_prefix(prefix) {
            let v = rest.trim();
            return (!v.is_empty()).then(|| Cond::Cmp(op, v.to_string()));
        }
    }
    if t.contains(',') {
        return Some(Cond::In(t.split(',').map(|v| v.trim().to_string()).collect()));
    }
    Some(Cond::Contains(t.to_string()))
}

/// Escapes `%`, `_` and the escape char itself (`|`) for `LIKE … ESCAPE '|'`.
fn like_contains(v: &str) -> String {
    let mut s = String::from("%");
    for c in v.chars() {
        if matches!(c, '%' | '_' | '|') {
            s.push('|');
        }
        s.push(c);
    }
    s.push('%');
    s
}

pub fn quote_ident(d: Dialect, name: &str) -> String {
    match d {
        Dialect::Postgres => format!("\"{}\"", name.replace('"', "\"\"")),
        Dialect::MySql => format!("`{}`", name.replace('`', "``")),
    }
}

/// Postgres types compared as themselves (the bind text is cast to the column type);
/// every other type (json, enums, arrays, text…) is compared as text.
const PG_TYPED: [&str; 12] = ["bool", "int2", "int4", "int8", "float4", "float8", "numeric", "date", "time", "timetz", "timestamp", "timestamptz"];

/// The statement without trailing whitespace and `;`.
pub fn strip(sql: &str) -> &str {
    let mut s = sql.trim_end();
    while let Some(r) = s.strip_suffix(';') {
        s = r.trim_end();
    }
    s.trim_start()
}

pub(crate) struct Wrapped {
    pub sql: String,
    pub params: Vec<String>,
    /// Chars before the inner statement (to map an error position back to the original text).
    pub prefix_chars: usize,
    pub inner_chars: usize,
}

fn where_clause(d: Dialect, filters: &[(String, String)], cols: &[Column], params: &mut Vec<String>) -> String {
    let mut parts = Vec::new();
    for (col, text) in filters {
        let Some(cond) = parse_filter(text) else { continue };
        let ty = cols.iter().find(|c| &c.name == col).map(|c| c.type_name.as_str()).unwrap_or("");
        let q = format!("q.{}", quote_ident(d, col));
        let pg = d == Dialect::Postgres;
        let textual = if pg { format!("{q}::text") } else { format!("CAST({q} AS CHAR)") };
        let typed = if pg { PG_TYPED.contains(&ty) } else { ty != "JSON" };
        let lhs = if typed { q.clone() } else { textual.clone() };
        // `cast`: compare as the column type (not for LIKE patterns, which are always text).
        let mut push = |v: String, cast: bool| -> String {
            params.push(v);
            let ph = if pg { format!("${}", params.len()) } else { "?".to_string() };
            if pg && typed && cast { format!("CAST({ph} AS {ty})") } else { ph }
        };
        parts.push(match cond {
            Cond::IsNull => format!("{q} IS NULL"),
            Cond::NotNull => format!("{q} IS NOT NULL"),
            Cond::Contains(v) => {
                let ph = push(like_contains(&v), false);
                format!("{textual} {} {ph} ESCAPE '|'", if pg { "ILIKE" } else { "LIKE" })
            }
            Cond::Cmp(op, v) => {
                let op = match op {
                    Op::Eq => "=",
                    Op::Ne => "<>",
                    Op::Gt => ">",
                    Op::Ge => ">=",
                    Op::Lt => "<",
                    Op::Le => "<=",
                };
                format!("{lhs} {op} {}", push(v, true))
            }
            Cond::In(vs) => format!("{lhs} IN ({})", vs.into_iter().map(|v| push(v, true)).collect::<Vec<_>>().join(", ")),
        });
    }
    if parts.is_empty() { String::new() } else { format!("\nWHERE {}", parts.join(" AND ")) }
}

/// Output columns for a filtered Postgres page: sqlx reads binary results, so every column is
/// cast to text and decoded by its original type name (see `value`).
fn pg_text_list(cols: &[Column]) -> String {
    if cols.is_empty() {
        return "*".into();
    }
    let list: Vec<String> = cols.iter().map(|c| format!("q.{0}::text AS {0}", quote_ident(Dialect::Postgres, &c.name))).collect();
    list.join(", ")
}

pub(crate) fn build(sql: &str, d: Dialect, page: &Page, cols: &[Column]) -> Wrapped {
    let inner = strip(sql);
    let mut params = Vec::new();
    let wh = where_clause(d, &page.filters, cols, &mut params);
    let list = if d == Dialect::Postgres && !params.is_empty() { pg_text_list(cols) } else { "*".into() };
    let prefix = format!("SELECT {list} FROM (");
    let order = if page.sort.is_empty() {
        String::new()
    } else {
        let items: Vec<String> = page.sort.iter().map(|(c, desc)| format!("q.{}{}", quote_ident(d, c), if *desc { " DESC" } else { "" })).collect();
        format!("\nORDER BY {}", items.join(", "))
    };
    // The newline keeps a trailing `-- comment` in the statement from swallowing `)`.
    let sql = format!("{prefix}{inner}\n) AS q{wh}{order}\nLIMIT {} OFFSET {}", page.size + 1, page.offset);
    Wrapped { sql, params, prefix_chars: prefix.chars().count(), inner_chars: inner.chars().count() }
}

/// `sql` wrapped for one page, plus the bind parameters (`$1…` / `?`).
/// `cols` (name + type of the inner query's columns) is only consulted for filters.
pub fn wrap(sql: &str, d: Dialect, page: &Page, cols: &[Column]) -> (String, Vec<String>) {
    let w = build(sql, d, page, cols);
    (w.sql, w.params)
}

pub(crate) fn build_count(sql: &str, d: Dialect, filters: &[(String, String)], cols: &[Column]) -> Wrapped {
    let inner = strip(sql);
    let mut params = Vec::new();
    let wh = where_clause(d, filters, cols, &mut params);
    let count = if d == Dialect::Postgres && !params.is_empty() { "count(*)::text" } else { "count(*)" };
    let prefix = format!("SELECT {count} FROM (");
    Wrapped { sql: format!("{prefix}{inner}\n) AS q{wh}"), params, prefix_chars: prefix.chars().count(), inner_chars: inner.chars().count() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ty: &str) -> Column {
        Column { name: name.into(), type_name: ty.into() }
    }
    fn f(c: &str, t: &str) -> (String, String) {
        (c.into(), t.into())
    }

    #[test]
    fn filter_syntax() {
        assert_eq!(parse_filter(""), None);
        assert_eq!(parse_filter("  "), None);
        assert_eq!(parse_filter("foo"), Some(Cond::Contains("foo".into())));
        assert_eq!(parse_filter("=42"), Some(Cond::Cmp(Op::Eq, "42".into())));
        assert_eq!(parse_filter("!=0"), Some(Cond::Cmp(Op::Ne, "0".into())));
        assert_eq!(parse_filter(">10"), Some(Cond::Cmp(Op::Gt, "10".into())));
        assert_eq!(parse_filter(">= 10"), Some(Cond::Cmp(Op::Ge, "10".into())));
        assert_eq!(parse_filter("<3"), Some(Cond::Cmp(Op::Lt, "3".into())));
        assert_eq!(parse_filter("<=2026-01-01"), Some(Cond::Cmp(Op::Le, "2026-01-01".into())));
        assert_eq!(parse_filter(">"), None);
        assert_eq!(parse_filter("null"), Some(Cond::IsNull));
        assert_eq!(parse_filter("NULL"), Some(Cond::IsNull));
        assert_eq!(parse_filter("!null"), Some(Cond::NotNull));
        assert_eq!(parse_filter("a, b,c"), Some(Cond::In(vec!["a".into(), "b".into(), "c".into()])));
        assert_eq!(parse_filter("=a,b"), Some(Cond::Cmp(Op::Eq, "a,b".into())));
        assert_eq!(parse_filter("nullable"), Some(Cond::Contains("nullable".into())));
    }

    #[test]
    fn like_escaping() {
        assert_eq!(like_contains("50%_|x"), "%50|%|_||x%");
    }

    #[test]
    fn quoting() {
        assert_eq!(quote_ident(Dialect::Postgres, "a\"b"), "\"a\"\"b\"");
        assert_eq!(quote_ident(Dialect::MySql, "a`b"), "`a``b`");
    }

    #[test]
    fn plain_page() {
        let (sql, p) = wrap("SELECT 1;  ", Dialect::Postgres, &Page::default(), &[]);
        assert_eq!(sql, "SELECT * FROM (SELECT 1\n) AS q\nLIMIT 101 OFFSET 0");
        assert!(p.is_empty());
        let page = Page { offset: 200, size: 50, ..Page::default() };
        assert_eq!(wrap("select 1;;", Dialect::MySql, &page, &[]).0, "SELECT * FROM (select 1\n) AS q\nLIMIT 51 OFFSET 200");
    }

    #[test]
    fn inner_limit_and_comment_stay_inside() {
        let (sql, _) = wrap("SELECT * FROM t LIMIT 5 -- hi", Dialect::Postgres, &Page::default(), &[]);
        assert_eq!(sql, "SELECT * FROM (SELECT * FROM t LIMIT 5 -- hi\n) AS q\nLIMIT 101 OFFSET 0");
    }

    #[test]
    fn sort() {
        let page = Page { sort: vec![("a".into(), true), ("b\"c".into(), false)], ..Page::default() };
        assert_eq!(wrap("SELECT 1", Dialect::Postgres, &page, &[]).0, "SELECT * FROM (SELECT 1\n) AS q\nORDER BY q.\"a\" DESC, q.\"b\"\"c\"\nLIMIT 101 OFFSET 0");
        assert_eq!(wrap("SELECT 1", Dialect::MySql, &page, &[]).0, "SELECT * FROM (SELECT 1\n) AS q\nORDER BY q.`a` DESC, q.`b\"c`\nLIMIT 101 OFFSET 0");
    }

    #[test]
    fn pg_filters() {
        let cols = [col("id", "int8"), col("name", "text"), col("doc", "jsonb")];
        let page = Page {
            filters: vec![f("id", ">10"), f("name", "50%"), f("doc", "=x"), f("id", "null"), f("name", "a,b"), f("x", "")],
            ..Page::default()
        };
        let (sql, p) = wrap("SELECT 1", Dialect::Postgres, &page, &cols);
        assert_eq!(
            sql,
            "SELECT q.\"id\"::text AS \"id\", q.\"name\"::text AS \"name\", q.\"doc\"::text AS \"doc\" FROM (SELECT 1\n) AS q\n\
             WHERE q.\"id\" > CAST($1 AS int8) AND q.\"name\"::text ILIKE $2 ESCAPE '|' AND q.\"doc\"::text = $3 AND q.\"id\" IS NULL AND q.\"name\"::text IN ($4, $5)\n\
             LIMIT 101 OFFSET 0"
        );
        assert_eq!(p, ["10", "%50|%%", "x", "a", "b"]);
    }

    #[test]
    fn pg_prefix_offset_maps_positions() {
        let cols = [col("id", "int8")];
        let page = Page { filters: vec![f("id", "=1")], ..Page::default() };
        let w = build("SELECT 1;", Dialect::Postgres, &page, &cols);
        let at = w.prefix_chars;
        assert_eq!(&w.sql[at..at + 8], "SELECT 1");
        assert_eq!(w.inner_chars, 8);
    }

    #[test]
    fn mysql_filters() {
        let cols = [col("id", "BIGINT"), col("name", "VARCHAR"), col("doc", "JSON")];
        let page = Page { filters: vec![f("id", "!=0"), f("name", "x_"), f("doc", ">a"), f("id", "!null"), f("id", "1,2")], ..Page::default() };
        let (sql, p) = wrap("SELECT 1", Dialect::MySql, &page, &cols);
        assert_eq!(
            sql,
            "SELECT * FROM (SELECT 1\n) AS q\n\
             WHERE q.`id` <> ? AND CAST(q.`name` AS CHAR) LIKE ? ESCAPE '|' AND CAST(q.`doc` AS CHAR) > ? AND q.`id` IS NOT NULL AND q.`id` IN (?, ?)\n\
             LIMIT 101 OFFSET 0"
        );
        assert_eq!(p, ["0", "%x|_%", "a", "1", "2"]);
    }

    #[test]
    fn unknown_filter_column_falls_back_to_text() {
        let page = Page { filters: vec![f("zz", "=1")], ..Page::default() };
        let (sql, _) = wrap("SELECT 1", Dialect::Postgres, &page, &[]);
        assert!(sql.contains("q.\"zz\"::text = $1"), "{sql}");
    }

    #[test]
    fn null_only_filter_has_no_params() {
        let page = Page { filters: vec![f("a", "null")], ..Page::default() };
        let (sql, p) = wrap("SELECT 1", Dialect::Postgres, &page, &[col("a", "int4")]);
        assert!(p.is_empty());
        assert!(sql.starts_with("SELECT * FROM"), "{sql}");
    }

    #[test]
    fn count() {
        let w = build_count("SELECT 1;", Dialect::Postgres, &[], &[]);
        assert_eq!(w.sql, "SELECT count(*) FROM (SELECT 1\n) AS q");
        let w = build_count("SELECT 1", Dialect::Postgres, &[f("a", "=1")], &[col("a", "int4")]);
        assert_eq!(w.sql, "SELECT count(*)::text FROM (SELECT 1\n) AS q\nWHERE q.\"a\" = CAST($1 AS int4)");
        let w = build_count("SELECT 1", Dialect::MySql, &[f("a", "=1")], &[col("a", "INT")]);
        assert_eq!(w.sql, "SELECT count(*) FROM (SELECT 1\n) AS q\nWHERE q.`a` = ?");
    }
}
