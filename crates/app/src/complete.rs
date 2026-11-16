//! Autocomplete: where the cursor is (statement start, table position, `alias.`, expression,
//! `-- @` directive) and what to offer there. GUI-free: text, cursor offset, dialect and the
//! cached [`Schema`] in, candidates out.
//!
//! The context comes from a token scan of the statement up to the cursor (strings, comments,
//! quoted names and `$$` are skipped), not from a parser; aliases come from the `FROM` / `JOIN` /
//! `UPDATE` / `INTO` clauses of the whole statement.

use db_exec::{ColumnInfo, Schema, Table, TableKind, quote_ident};
use sql_split::{DIRECTIVES, Dialect};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Keyword,
    Snippet,
    Table,
    View,
    Column,
    Function,
    Directive,
}

impl Kind {
    /// Menu order of equally good matches.
    fn rank(self) -> u8 {
        match self {
            Kind::Column | Kind::Table | Kind::View | Kind::Directive => 0,
            Kind::Keyword => 1,
            Kind::Snippet => 2,
            Kind::Function => 3,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub label: String,
    /// Replaces `start..cursor`.
    pub insert: String,
    pub detail: String,
    pub kind: Kind,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Completion {
    /// Byte offset where the word being completed starts.
    pub start: usize,
    pub items: Vec<Item>,
}

const START: &[&str] = &[
    "SELECT", "INSERT INTO", "UPDATE", "DELETE FROM", "WITH", "CREATE TABLE", "CREATE INDEX", "CREATE VIEW", "ALTER TABLE", "DROP TABLE", "TRUNCATE",
    "EXPLAIN", "EXPLAIN ANALYZE", "BEGIN", "COMMIT", "ROLLBACK", "SET", "VALUES",
];
const START_PG: &[&str] = &["TABLE", "VACUUM", "ANALYZE", "SHOW", "COPY"];
const START_MY: &[&str] = &["SHOW", "DESCRIBE", "USE", "START TRANSACTION"];
const SNIPPETS: &[(&str, &str)] = &[
    ("SELECT * FROM", "SELECT * FROM "),
    ("SELECT count(*) FROM", "SELECT count(*) FROM "),
    ("EXPLAIN ANALYZE SELECT * FROM", "EXPLAIN ANALYZE SELECT * FROM "),
];
/// What follows a table name or alias.
const CONTINUE: &[&str] = &[
    "WHERE", "JOIN", "LEFT JOIN", "RIGHT JOIN", "INNER JOIN", "CROSS JOIN", "FULL JOIN", "ON", "USING", "GROUP BY", "ORDER BY", "HAVING", "LIMIT", "UNION",
    "SET", "VALUES", "SELECT", "RETURNING",
];
const CLAUSES: &[&str] = &["FROM", "WHERE", "GROUP BY", "ORDER BY", "HAVING", "LIMIT", "OFFSET", "JOIN", "LEFT JOIN", "UNION", "AS", "DISTINCT"];
const EXPR: &[&str] = &["AND", "OR", "NOT", "IN", "LIKE", "IS NULL", "IS NOT NULL", "BETWEEN", "EXISTS", "CASE", "WHEN", "THEN", "ELSE", "END", "ASC", "DESC", "NULL"];
const FUNCS: &[&str] = &["count", "sum", "avg", "min", "max", "coalesce", "nullif", "lower", "upper", "length", "substring", "concat", "round", "abs", "cast"];
const FUNCS_PG: &[&str] = &[
    "now", "date_trunc", "extract", "to_char", "string_agg", "array_agg", "jsonb_build_object", "jsonb_agg", "generate_series", "row_number", "age", "left",
];
const FUNCS_MY: &[&str] = &["now", "curdate", "ifnull", "if", "concat_ws", "date_format", "date_add", "group_concat", "json_extract", "row_number", "left"];

/// Names that must be quoted even though they look plain.
const RESERVED: &[&str] = &[
    "all", "analyse", "and", "any", "as", "asc", "both", "case", "check", "column", "constraint", "create", "default", "desc", "distinct", "end", "from", "grant",
    "group", "having", "in", "index", "into", "join", "key", "left", "like", "limit", "not", "null", "offset", "on", "or", "order", "primary", "references",
    "right", "select", "table", "then", "to", "union", "unique", "user", "using", "when", "where", "with",
];

/// Postgres folds unquoted names to lower case, MySQL keeps them as written.
fn plain(d: Dialect, name: &str) -> bool {
    let ok = |i: usize, c: char| match d {
        Dialect::Postgres => c.is_ascii_lowercase() || c == '_' || (i > 0 && (c.is_ascii_digit() || c == '$')),
        Dialect::MySql => c.is_ascii_alphabetic() || c == '_' || (i > 0 && (c.is_ascii_digit() || c == '$')),
    };
    !name.is_empty() && name.chars().enumerate().all(|(i, c)| ok(i, c)) && !RESERVED.contains(&name.to_ascii_lowercase().as_str())
}

/// The name as it is written in SQL: quoted only when it has to be.
pub fn ident(d: Dialect, name: &str) -> String {
    if plain(d, name) { name.to_string() } else { quote_ident(d, name) }
}

/// `schema.table` (`prefix`) or `table`.
pub fn qualified(d: Dialect, schema: &str, table: &str, prefix: bool) -> String {
    if prefix { format!("{}.{}", ident(d, schema), ident(d, table)) } else { ident(d, table) }
}

/// A schema prefix is needed unless the database has one schema or it is the default one.
pub fn needs_prefix(schema: &Schema, name: &str, default: Option<&str>) -> bool {
    schema.schemas.len() > 1 && default.is_none_or(|d| !d.eq_ignore_ascii_case(name))
}

// ---- tokens ----

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum T {
    Word,
    /// `s..e` is the name without quotes.
    Quoted,
    Punct,
    /// Numbers, string literals, parameters.
    Other,
}

#[derive(Clone, Copy, Debug)]
struct Tok {
    t: T,
    s: usize,
    e: usize,
}

/// Where the scanned text ends.
#[derive(Debug, PartialEq, Eq)]
enum End {
    Code,
    /// Inside a string, `$$` or block comment.
    Text,
    /// Inside a line comment that starts at the offset.
    Line(usize),
    /// Inside a quoted name opened at the offset with that quote.
    Quote(usize, u8),
}

fn lex(src: &str, d: Dialect) -> (Vec<Tok>, End) {
    let b = src.as_bytes();
    let my = d == Dialect::MySql;
    let (mut i, mut toks) = (0, vec![]);
    let push = |toks: &mut Vec<Tok>, t, s, e| toks.push(Tok { t, s, e });
    while i < b.len() {
        let c = b[i];
        match c {
            _ if c.is_ascii_whitespace() => i += 1,
            b'-' if b.get(i + 1) == Some(&b'-') => match src[i..].find('\n') {
                Some(n) => i += n,
                None => return (toks, End::Line(i)),
            },
            b'#' if my => match src[i..].find('\n') {
                Some(n) => i += n,
                None => return (toks, End::Line(i)),
            },
            b'/' if b.get(i + 1) == Some(&b'*') => match src[i + 2..].find("*/") {
                Some(n) => i += n + 4,
                None => return (toks, End::Text),
            },
            b'\'' | b'"' | b'`' => {
                let ident = c == b'`' && my || c == b'"' && !my;
                let mut j = i + 1;
                loop {
                    if j >= b.len() {
                        return (toks, if ident { End::Quote(i, c) } else { End::Text });
                    }
                    if my && !ident && b[j] == b'\\' {
                        j += 2;
                    } else if b[j] == c {
                        if b.get(j + 1) == Some(&c) {
                            j += 2;
                        } else {
                            break;
                        }
                    } else {
                        j += 1;
                    }
                }
                if ident { push(&mut toks, T::Quoted, i + 1, j) } else { push(&mut toks, T::Other, i, j + 1) }
                i = j + 1;
            }
            b'$' if !my => {
                let n = b[i + 1..].iter().take_while(|c| c.is_ascii_alphanumeric() || **c == b'_').count();
                if b.get(i + 1 + n) == Some(&b'$') && !b.get(i + 1).is_some_and(u8::is_ascii_digit) {
                    let tag = &src[i..i + n + 2];
                    match src[i + tag.len()..].find(tag) {
                        Some(p) => i += tag.len() + p + tag.len(),
                        None => return (toks, End::Text),
                    }
                    push(&mut toks, T::Other, i, i);
                } else {
                    let e = i + 1 + b[i + 1..].iter().take_while(|c| c.is_ascii_digit()).count();
                    push(&mut toks, T::Other, i, e);
                    i = e;
                }
            }
            _ if c.is_ascii_alphabetic() || c == b'_' || c >= 0x80 => {
                let e = i + b[i..].iter().take_while(|c| c.is_ascii_alphanumeric() || **c == b'_' || **c == b'$' || **c >= 0x80).count();
                push(&mut toks, T::Word, i, e);
                i = e;
            }
            _ if c.is_ascii_digit() => {
                let e = i + b[i..].iter().take_while(|c| c.is_ascii_alphanumeric() || **c == b'_').count();
                push(&mut toks, T::Other, i, e);
                i = e;
            }
            _ => {
                push(&mut toks, T::Punct, i, i + 1);
                i += 1;
            }
        }
    }
    (toks, End::Code)
}

/// A name token's text: the word, or the quoted name.
fn name(src: &str, t: Tok) -> &str {
    &src[t.s..t.e]
}

fn is_kw(src: &str, t: Tok, kws: &[&str]) -> bool {
    t.t == T::Word && kws.iter().any(|k| name(src, t).eq_ignore_ascii_case(k))
}

fn is_punct(src: &str, t: Tok, c: char) -> bool {
    t.t == T::Punct && src[t.s..].starts_with(c)
}

// ---- tables of the statement ----

/// A table named in `FROM` / `JOIN` / `UPDATE` / `INTO`.
#[derive(Debug, PartialEq, Eq)]
struct TableRef {
    schema: Option<String>,
    table: String,
    alias: Option<String>,
}

/// Words that end a table reference (so they are not its alias).
const NOT_ALIAS: &[&str] = &[
    "where", "on", "join", "left", "right", "inner", "outer", "cross", "full", "natural", "group", "order", "having", "limit", "union", "using", "set", "values",
    "select", "returning", "offset", "fetch", "for", "window", "except", "intersect", "lateral", "straight_join", "as", "default",
];

fn refs(src: &str, d: Dialect) -> Vec<TableRef> {
    let (toks, _) = lex(src, d);
    let mut out = vec![];
    let mut i = 0;
    let mut in_from = false;
    while i < toks.len() {
        let t = toks[i];
        let next_ref = is_kw(src, t, &["from", "join", "update", "into"]) || (in_from && is_punct(src, t, ',') && !out.is_empty());
        if is_kw(src, t, &["from"]) {
            in_from = true;
        } else if is_kw(src, t, &["where", "group", "order", "having", "limit", "union", "set", "select", "on", "using", "returning"]) {
            in_from = false;
        }
        i += 1;
        if !next_ref {
            continue;
        }
        let ident = |at: usize| toks.get(at).filter(|t| matches!(t.t, T::Word | T::Quoted)).copied();
        // `[schema .] table [[AS] alias]`; `(` after the name is a function or a subquery
        // (but `INTO t (a, b)` is a column list).
        let Some(first) = ident(i) else { continue };
        let (mut schema, mut table, mut at) = (None, name(src, first), i + 1);
        if toks.get(at).is_some_and(|t| is_punct(src, *t, '.'))
            && let Some(second) = ident(at + 1)
        {
            (schema, table, at) = (Some(name(src, first)), name(src, second), at + 2);
        }
        if toks.get(at).is_some_and(|t| is_punct(src, *t, '(')) {
            if is_kw(src, t, &["into"]) {
                out.push(TableRef { schema: schema.map(str::to_string), table: table.to_string(), alias: None });
                i = at;
            }
            continue;
        }
        if is_kw(src, first, &["select", "lateral"]) {
            continue;
        }
        let mut alias = None;
        if toks.get(at).is_some_and(|t| is_kw(src, *t, &["as"])) {
            at += 1;
            alias = ident(at);
        } else if let Some(a) = ident(at)
            && (a.t == T::Quoted || !is_kw(src, a, NOT_ALIAS))
        {
            alias = Some(a);
        }
        out.push(TableRef { schema: schema.map(str::to_string), table: table.to_string(), alias: alias.map(|a| name(src, a).to_string()) });
        i = at;
    }
    out
}

fn find_table<'a>(schema: &'a Schema, sch: Option<&str>, table: &str, default: Option<&str>) -> Option<(&'a str, &'a Table)> {
    let mut found = schema
        .schemas
        .iter()
        .filter(|s| sch.is_none_or(|q| q.eq_ignore_ascii_case(&s.name)))
        .filter_map(|s| s.tables.iter().find(|t| t.name.eq_ignore_ascii_case(table)).map(|t| (s.name.as_str(), t)));
    // Several schemas may have it: the default one wins.
    let all: Vec<_> = found.by_ref().collect();
    all.iter().find(|(s, _)| default.is_some_and(|d| d.eq_ignore_ascii_case(s))).or(all.first()).copied()
}

/// What a qualifier (`u` of `u.`, or `public.users`) names among the statement's tables.
fn resolve<'a>(schema: &'a Schema, refs: &[TableRef], q: &[&str], default: Option<&str>) -> Option<(&'a str, &'a Table)> {
    match q {
        [one] => {
            if let Some(r) = refs.iter().find(|r| r.alias.as_deref().is_some_and(|a| a.eq_ignore_ascii_case(one))) {
                return find_table(schema, r.schema.as_deref(), &r.table, default);
            }
            // `users.` — a table of the statement, or any table of the schema.
            let r = refs.iter().find(|r| r.alias.is_none() && r.table.eq_ignore_ascii_case(one));
            find_table(schema, r.and_then(|r| r.schema.as_deref()), one, default)
        }
        [s, t] => find_table(schema, Some(s), t, default),
        _ => None,
    }
}

// ---- candidates ----

fn column_item(c: &ColumnInfo, table: Option<&str>) -> Item {
    let mut detail = c.data_type.clone();
    if c.primary_key {
        detail += " · PK";
    }
    if !c.nullable {
        detail += " · NOT NULL";
    }
    if let Some(t) = table {
        detail = format!("{t} · {detail}");
    }
    Item { label: c.name.clone(), insert: String::new(), detail, kind: Kind::Column }
}

fn words(list: &[&str], kind: Kind, detail: &str) -> Vec<Item> {
    list.iter().map(|w| Item { label: w.to_string(), insert: w.to_string(), detail: detail.into(), kind }).collect()
}

fn functions(d: Dialect) -> Vec<Item> {
    let own = if d == Dialect::MySql { FUNCS_MY } else { FUNCS_PG };
    FUNCS.iter().chain(own).map(|f| Item { label: format!("{f}()"), insert: format!("{f}("), detail: "function".into(), kind: Kind::Function }).collect()
}

/// Lower is better; `None` = no match. Prefix, then a word start, then anywhere, then scattered.
fn fuzzy(label: &str, query: &str) -> Option<usize> {
    if query.is_empty() {
        return Some(0);
    }
    let (hay, q) = (label.to_lowercase(), query.to_lowercase());
    if hay.starts_with(&q) {
        return Some(0);
    }
    if let Some(at) = hay.match_indices(&q).map(|(i, _)| i).find(|&i| hay[..i].ends_with(['_', '.', ' '])) {
        return Some(1 + at);
    }
    if let Some(at) = hay.find(&q) {
        return Some(100 + at);
    }
    let mut chars = hay.char_indices();
    let mut last = 0;
    for qc in q.chars() {
        last = chars.find(|(_, h)| *h == qc)?.0;
    }
    Some(1000 + last)
}

const MAX_ITEMS: usize = 100;

fn finish(start: usize, word: &str, mut items: Vec<Item>) -> Completion {
    let mut scored: Vec<(usize, u8, Item)> = items
        .drain(..)
        .filter(|i| !i.label.eq_ignore_ascii_case(word))
        .filter_map(|i| Some((fuzzy(&i.label, word)?, i.kind.rank(), i)))
        .collect();
    scored.sort_by(|a, b| (a.0, a.1, &a.2.label).cmp(&(b.0, b.1, &b.2.label)));
    Completion { start, items: scored.into_iter().take(MAX_ITEMS).map(|(.., i)| i).collect() }
}

fn directives(comment: &str, at: usize) -> Completion {
    // `-- @word` (or `#`): once there is a space, the directive's value is typed.
    let rest = comment.trim_start_matches(['-', '#']).trim_start();
    let Some(word) = rest.strip_prefix('@').filter(|w| w.chars().all(|c| c.is_ascii_alphabetic() || c == '-')) else { return Completion::default() };
    let info = |n: &str| match n {
        "connection" => "default connection of the file (at its start)",
        "database" => "database of the file on the connection's server (at its start)",
        "name" => "name of the statement in the tree, search and CLI",
        "page-size" => "result page size of the statement",
        "confirm" => "always ask before running",
        _ => "",
    };
    let items = DIRECTIVES
        .iter()
        .map(|n| Item { label: n.to_string(), insert: if *n == "confirm" { n.to_string() } else { format!("{n} ") }, detail: info(n).into(), kind: Kind::Directive })
        .collect();
    finish(at - word.len(), word, items)
}

/// Candidates at byte offset `cursor` of `text`.
///
/// `default_schema` is the one whose tables are written without a prefix (Postgres: the
/// connection's `schema` or `public`; MySQL: the database).
pub fn complete(text: &str, cursor: usize, d: Dialect, schema: Option<&Schema>, default_schema: Option<&str>) -> Completion {
    let none = Completion::default();
    if cursor > text.len() || !text.is_char_boundary(cursor) {
        return none;
    }
    // The statement under the cursor starts at its first token, or just after the `;` before it.
    let file = sql_split::split(text, d);
    let (mut start, mut end) = (0, cursor);
    for st in &file.statements {
        if st.range.start > cursor {
            break;
        }
        if st.range.end >= cursor {
            (start, end) = (st.range.start, st.range.end);
            break;
        }
        match text[st.range.end..cursor].find(';') {
            Some(p) => start = st.range.end + p + 1,
            None => start = st.range.start,
        }
    }
    let (prefix, full) = (&text[start..cursor], &text[start..end.max(cursor)]);
    let (mut toks, state) = lex(prefix, d);
    let mut quote = None;
    let word_at = match state {
        End::Text => return none,
        End::Line(at) => return directives(&prefix[at..], start + prefix.len()),
        End::Quote(at, q) => {
            quote = Some(q as char);
            at + 1
        }
        End::Code => match toks.last() {
            Some(t) if t.t == T::Word && t.e == prefix.len() => toks.pop().map_or(prefix.len(), |t| t.s),
            // Right after a literal or a closed quoted name: nothing to complete.
            Some(t) if matches!(t.t, T::Other | T::Quoted) && t.e + usize::from(t.t == T::Quoted) == prefix.len() => return none,
            _ => prefix.len(),
        },
    };
    let (word, at) = (&prefix[word_at..], start + word_at);
    // The name being typed may be unterminated: its quotes would swallow the rest of the statement.
    let unquoted;
    let full = if quote.is_some() {
        let mut s = full.to_string();
        s.replace_range(word_at - 1..word_at, " ");
        if let Some(q) = quote.filter(|q| text[cursor..].starts_with(*q))
            && s.len() >= prefix.len() + q.len_utf8()
        {
            s.replace_range(prefix.len()..prefix.len() + q.len_utf8(), " ");
        }
        unquoted = s;
        &unquoted
    } else {
        full
    };
    let default = default_schema;

    // `alias.` / `table.` / `schema.`
    if toks.last().is_some_and(|t| is_punct(prefix, *t, '.')) {
        toks.pop();
        let mut q = vec![];
        while let Some(t) = toks.last().filter(|t| matches!(t.t, T::Word | T::Quoted)).copied() {
            toks.pop();
            q.insert(0, name(prefix, t));
            if q.len() == 2 || !toks.last().is_some_and(|t| is_punct(prefix, *t, '.')) {
                break;
            }
            toks.pop();
        }
        let Some(schema) = schema.filter(|_| !q.is_empty()) else { return none };
        let items = if let Some((_, t)) = resolve(schema, &refs(full, d), &q, default) {
            t.columns.iter().map(|c| column_item(c, None)).collect()
        } else if let [s] = q[..]
            && let Some(sn) = schema.schemas.iter().find(|n| n.name.eq_ignore_ascii_case(s))
        {
            sn.tables.iter().map(|t| table_item(d, &sn.name, t, false, quote)).collect()
        } else {
            vec![]
        };
        return finish(at, word, close(d, items, quote, text, cursor));
    }

    let before = toks.last().copied();
    let in_kw = |t: Option<Tok>, kws: &[&str]| t.is_some_and(|t| is_kw(prefix, t, kws));
    let depth = toks.iter().fold(0i32, |n, t| n + i32::from(is_punct(prefix, *t, '(')) - i32::from(is_punct(prefix, *t, ')')));
    let last_kw = toks
        .iter()
        .rev()
        .copied()
        .find(|t| is_kw(prefix, *t, &["select", "from", "join", "where", "on", "using", "group", "order", "having", "set", "into", "update", "values", "limit", "returning"]));
    let last_is = |kws: &[&str]| last_kw.is_some_and(|t| is_kw(prefix, t, kws));

    let Some(before) = before else {
        let mut items = words(START, Kind::Keyword, "");
        items.extend(words(if d == Dialect::MySql { START_MY } else { START_PG }, Kind::Keyword, ""));
        items.extend(SNIPPETS.iter().map(|(l, i)| Item { label: l.to_string(), insert: i.to_string(), detail: "snippet".into(), kind: Kind::Snippet }));
        return finish(at, word, items);
    };

    let after_create = toks.len() >= 2 && is_kw(prefix, toks[toks.len() - 2], &["create"]);
    let table_position = in_kw(Some(before), &["from", "join", "update", "into", "truncate"])
        || (in_kw(Some(before), &["table"]) && !after_create)
        || (is_punct(prefix, before, ',') && last_is(&["from"]));
    if after_create && in_kw(Some(before), &["table"]) {
        return none; // a new name goes there
    }
    let mut items;
    if table_position {
        let Some(schema) = schema else { return none };
        items = vec![];
        for sn in &schema.schemas {
            let prefixed = needs_prefix(schema, &sn.name, default);
            items.extend(sn.tables.iter().map(|t| table_item(d, &sn.name, t, prefixed, quote)));
        }
    } else if matches!(before.t, T::Word | T::Quoted) && last_is(&["from", "join", "into", "update"]) && depth == 0 {
        // After `FROM users` or `FROM users u`: what continues the clause.
        items = words(CONTINUE, Kind::Keyword, "");
    } else {
        items = vec![];
        if let Some(schema) = schema {
            let rs = refs(full, d);
            let many = rs.len() > 1;
            for r in &rs {
                if let Some((_, t)) = find_table(schema, r.schema.as_deref(), &r.table, default) {
                    items.extend(t.columns.iter().map(|c| column_item(c, many.then_some(t.name.as_str()))));
                }
            }
        }
        // `INSERT INTO t (` takes columns only.
        if !(depth > 0 && last_is(&["into"])) {
            items.extend(functions(d));
            items.extend(words(CLAUSES, Kind::Keyword, ""));
            items.extend(words(EXPR, Kind::Keyword, ""));
        }
    }
    finish(at, word, close(d, items, quote, text, cursor))
}

fn table_item(d: Dialect, schema: &str, t: &Table, prefixed: bool, quote: Option<char>) -> Item {
    let label = if prefixed { format!("{schema}.{}", t.name) } else { t.name.clone() };
    let (what, kind) = match t.kind {
        TableKind::Table => ("table", Kind::Table),
        TableKind::View => ("view", Kind::View),
    };
    // In an open quote only the bare name fits (`close` strips the prefix from the label too).
    let detail = if prefixed || quote.is_some() { what.to_string() } else { format!("{what} · {schema}") };
    let insert = if quote.is_some() { t.name.clone() } else { qualified(d, schema, &t.name, prefixed) };
    Item { label, insert, detail, kind }
}

/// Fills in the `insert` text of columns, and closes an open quote.
fn close(d: Dialect, mut items: Vec<Item>, quote: Option<char>, text: &str, cursor: usize) -> Vec<Item> {
    for i in &mut items {
        if i.kind == Kind::Column {
            i.insert = if quote.is_some() { i.label.clone() } else { ident(d, &i.label) };
        }
    }
    if let Some(q) = quote {
        items.retain(|i| matches!(i.kind, Kind::Column | Kind::Table | Kind::View));
        let closed = text[cursor..].starts_with(q);
        for i in &mut items {
            // The bare name: a schema prefix cannot sit inside one pair of quotes.
            if let Some(bare) = i.label.split_once('.').filter(|_| i.kind != Kind::Column).map(|(_, b)| b.to_string()) {
                i.label = bare.clone();
                i.insert = bare;
            }
            if !closed {
                i.insert.push(q);
            }
        }
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use db_exec::{ColumnInfo, SchemaNode};
    use sql_split::Dialect::{MySql as MY, Postgres as PG};

    fn col(name: &str, ty: &str, pk: bool, nullable: bool, n: u32) -> ColumnInfo {
        ColumnInfo { name: name.into(), data_type: ty.into(), nullable, primary_key: pk, ordinal: n }
    }

    fn table(name: &str, kind: TableKind, cols: &[(&str, &str)]) -> Table {
        let columns = cols.iter().enumerate().map(|(i, (n, t))| col(n, t, i == 0, i != 0, i as u32 + 1)).collect();
        Table { name: name.into(), kind, columns, indexes: vec![] }
    }

    fn schema() -> Schema {
        Schema {
            schemas: vec![
                SchemaNode {
                    name: "analytics".into(),
                    tables: vec![table("events", TableKind::Table, &[("id", "bigint"), ("payload", "jsonb")])],
                },
                SchemaNode {
                    name: "public".into(),
                    tables: vec![
                        table("Users", TableKind::Table, &[("id", "integer"), ("name", "text"), ("email", "text")]),
                        table("orders", TableKind::Table, &[("id", "integer"), ("user_id", "integer"), ("total", "numeric")]),
                        table("user_orders", TableKind::View, &[("user_id", "integer")]),
                        table("user", TableKind::Table, &[("id", "integer")]),
                    ],
                },
            ],
        }
    }

    /// `text` with `|` marking the cursor.
    fn at(text: &str, d: Dialect, s: Option<&Schema>) -> Completion {
        let cursor = text.find('|').expect("cursor");
        complete(&text.replacen('|', "", 1), cursor, d, s, Some("public"))
    }

    fn labels(c: &Completion) -> Vec<&str> {
        c.items.iter().map(|i| i.label.as_str()).collect()
    }

    #[test]
    fn statement_start_offers_keywords_and_snippets() {
        let c = at("sel|", PG, None);
        assert_eq!(c.items[0].label, "SELECT");
        assert!(labels(&c).contains(&"SELECT * FROM"));
        assert_eq!(c.start, 0);
        let all = at("|", PG, None);
        for k in ["SELECT", "INSERT INTO", "UPDATE", "DELETE FROM", "WITH", "CREATE TABLE"] {
            assert!(labels(&all).contains(&k), "{k}");
        }
        assert!(labels(&at("|", MY, None)).contains(&"DESCRIBE"));
        // After a `;` a new statement starts, even on the same line.
        let c = at("SELECT 1; del|", PG, None);
        assert_eq!((c.items[0].label.as_str(), c.start), ("DELETE FROM", 10));
        let c = at("SELECT 1;\n-- note\nup|", PG, None);
        assert_eq!(c.items[0].label, "UPDATE");
    }

    #[test]
    fn fuzzy_matching() {
        assert_eq!(at("dlt|", PG, None).items[0].label, "DELETE FROM");
        let s = schema();
        let c = at("SELECT * FROM uo|", PG, Some(&s));
        assert_eq!(labels(&c), ["user_orders"]);
        // Prefix beats word start beats scattered.
        let c = at("SELECT * FROM ord|", PG, Some(&s));
        assert_eq!(labels(&c), ["orders", "user_orders"]);
        // An exact match is not offered again.
        assert!(at("SELECT * FROM orders|", PG, Some(&s)).items.iter().all(|i| i.label != "orders"));
    }

    #[test]
    fn tables_after_from_join_into_update() {
        let s = schema();
        for sql in ["SELECT * FROM |", "SELECT * FROM a JOIN |", "INSERT INTO |", "UPDATE |", "DELETE FROM |", "SELECT * FROM a, |", "TRUNCATE |", "DROP TABLE |"] {
            let c = at(sql, PG, Some(&s));
            assert!(labels(&c).contains(&"orders"), "{sql}: {:?}", labels(&c));
            assert!(c.items.iter().all(|i| matches!(i.kind, Kind::Table | Kind::View)), "{sql}");
        }
        // Not after `CREATE TABLE` (a new name goes there).
        assert!(at("CREATE TABLE |", PG, Some(&s)).items.is_empty());
        // No schema yet: nothing, not a crash.
        assert!(at("SELECT * FROM |", PG, None).items.is_empty());
        let c = at("SELECT * FROM user_o|", PG, Some(&s));
        assert_eq!((c.items[0].kind, c.items[0].detail.as_str()), (Kind::View, "view · public"));
    }

    #[test]
    fn schema_prefix_only_outside_the_default_schema() {
        let s = schema();
        let c = at("SELECT * FROM |", PG, Some(&s));
        let l = labels(&c);
        assert!(l.contains(&"analytics.events") && l.contains(&"orders") && !l.contains(&"public.orders"));
        // Names that need quotes are quoted in the inserted text, not in the label.
        let users = c.items.iter().find(|i| i.label == "Users").unwrap();
        assert_eq!(users.insert, "\"Users\"");
        assert_eq!(c.items.iter().find(|i| i.label == "user").unwrap().insert, "\"user\"");
        // One schema: never a prefix.
        let one = Schema { schemas: vec![schema().schemas.remove(0)] };
        assert!(labels(&at("SELECT * FROM |", PG, Some(&one))).contains(&"events"));
        // MySQL: the database is the default.
        let c = complete("SELECT * FROM ", 14, MY, Some(&schema()), Some("analytics"));
        assert!(labels(&c).contains(&"events") && labels(&c).contains(&"public.orders"));
    }

    #[test]
    fn columns_after_alias_or_table_name() {
        let s = schema();
        let c = at("SELECT u.| FROM public.\"Users\" u", PG, Some(&s));
        assert_eq!(labels(&c), ["email", "id", "name"]);
        let id = c.items.iter().find(|i| i.label == "id").unwrap();
        assert_eq!(id.detail, "integer · PK · NOT NULL");
        let name = c.items.iter().find(|i| i.label == "name").unwrap();
        assert_eq!(name.detail, "text");
        // `AS`, joins, comma lists.
        let c = at("SELECT * FROM \"Users\" AS u JOIN orders o ON o.| = u.id", PG, Some(&s));
        assert_eq!(labels(&c), ["id", "total", "user_id"]);
        let c = at("SELECT * FROM \"Users\" u, orders o WHERE u.id = o.us|", PG, Some(&s));
        assert_eq!(labels(&c), ["user_id"]);
        // The table's own name, in the statement or not.
        assert_eq!(labels(&at("SELECT orders.t| FROM orders", PG, Some(&s))), ["total"]);
        assert_eq!(labels(&at("SELECT orders.| FROM x", PG, Some(&s))).len(), 3);
        // `schema.table.`
        assert_eq!(labels(&at("SELECT analytics.events.| FROM analytics.events", PG, Some(&s))), ["id", "payload"]);
        // A schema before the dot: its tables.
        let c = at("SELECT * FROM analytics.|", PG, Some(&s));
        assert_eq!(labels(&c), ["events"]);
        // An unknown qualifier: nothing.
        assert!(at("SELECT z.| FROM orders", PG, Some(&s)).items.is_empty());
        assert!(at("SELECT u.| FROM orders u", PG, None).items.is_empty());
    }

    #[test]
    fn columns_and_functions_in_expressions() {
        let s = schema();
        for sql in [
            "SELECT | FROM orders",
            "SELECT * FROM orders WHERE |",
            "SELECT * FROM orders ORDER BY |",
            "SELECT total FROM orders GROUP BY |",
            "SELECT * FROM orders WHERE total > 1 AND |",
        ] {
            let c = at(sql, PG, Some(&s));
            let l = labels(&c);
            assert!(l.contains(&"total") && l.contains(&"count()"), "{sql}");
        }
        let c = at("SELECT cou| FROM orders", PG, Some(&s));
        assert_eq!((c.items[0].label.as_str(), c.items[0].insert.as_str()), ("count()", "count("));
        // Several tables: the table says whose column it is.
        let c = at("SELECT | FROM orders o JOIN \"Users\" u ON 1 = 1", PG, Some(&s));
        assert!(c.items.iter().any(|i| i.label == "email" && i.detail.starts_with("Users · text")));
        // Dialect functions.
        assert!(labels(&at("SELECT ifn| FROM orders", MY, Some(&s))).contains(&"ifnull()"));
        assert!(!labels(&at("SELECT ifn| FROM orders", PG, Some(&s))).contains(&"ifnull()"));
        // INSERT column list.
        let c = at("INSERT INTO orders (|", PG, Some(&s));
        assert_eq!(labels(&c), ["id", "total", "user_id"]);
        // After a table: how the clause goes on.
        let c = at("SELECT * FROM orders w|", PG, Some(&s));
        assert_eq!(c.items[0].label, "WHERE");
        assert!(at("UPDATE orders |", PG, Some(&s)).items.iter().any(|i| i.label == "SET"));
    }

    #[test]
    fn quoted_identifiers() {
        let s = schema();
        // An open quote: names without the quote, which gets closed.
        let c = at("SELECT * FROM \"Us|", PG, Some(&s));
        assert_eq!((labels(&c), c.items[0].insert.as_str(), c.start), (vec!["Users", "user", "user_orders"], "Users\"", 15));
        let c = at("SELECT u.\"na| FROM \"Users\" u", PG, Some(&s));
        assert_eq!(c.items[0].insert, "name\"");
        // Already closed after the cursor: not closed twice.
        let c = at("SELECT u.\"na|\" FROM \"Users\" u", PG, Some(&s));
        assert_eq!(c.items[0].insert, "name");
        // Keywords do not belong in a quoted name.
        assert!(at("SELECT \"sel|", PG, Some(&s)).items.iter().all(|i| i.kind == Kind::Column));
        // MySQL backticks, and a prefix-less name that needs them.
        let c = complete("SELECT * FROM `ord", 18, MY, Some(&schema()), Some("public"));
        assert_eq!(c.items[0].insert, "orders`");
        assert_eq!(ident(MY, "Order Items"), "`Order Items`");
        assert_eq!(ident(MY, "Orders"), "Orders");
        assert_eq!(ident(PG, "Orders"), "\"Orders\"");
        assert_eq!(ident(PG, "a\"b"), "\"a\"\"b\"");
        // A quoted alias.
        let c = at("SELECT \"U\".| FROM orders \"U\"", PG, Some(&s));
        assert_eq!(labels(&c), ["id", "total", "user_id"]);
    }

    #[test]
    fn nothing_inside_strings_and_comments() {
        let s = schema();
        for sql in [
            "SELECT 'abc|",
            "SELECT 'it''s fr|",
            "SELECT * FROM orders WHERE x = 'a|b'",
            "-- plain comment |",
            "SELECT 1 -- note fr|",
            "/* block fr|",
            "SELECT 1 /* a */ /* fr|",
            "SELECT $$ body fr|",
            "SELECT $q$ body $$ fr|",
            "SELECT 1;\n-- comment fr|",
            "SELECT 'a'|",
            "SELECT 12|",
            "SELECT \"x\"|",
        ] {
            assert!(at(sql, PG, Some(&s)).items.is_empty(), "{sql}");
        }
        // MySQL: `#` comments and backslash escapes.
        assert!(at("SELECT 1 # fr|", MY, Some(&s)).items.is_empty());
        assert!(at("SELECT 'it\\'s fr|", MY, Some(&s)).items.is_empty());
        // Closed ones do not hide what follows.
        assert!(!at("SELECT 'a', /* c */ fr| FROM orders", PG, Some(&s)).items.is_empty());
        assert!(!at("SELECT $$x$$, tot| FROM orders", PG, Some(&s)).items.is_empty());
        assert!(!at("SELECT $1, tot| FROM orders", PG, Some(&s)).items.is_empty());
    }

    #[test]
    fn directives_work_without_a_connection() {
        let c = at("-- @|", PG, None);
        assert_eq!(labels(&c), ["confirm", "connection", "database", "name", "page-size"]);
        assert!(c.items.iter().all(|i| i.kind == Kind::Directive && !i.detail.is_empty()));
        assert_eq!(c.start, 4);
        let c = at("-- @pa|", PG, None);
        assert_eq!((labels(&c), c.items[0].insert.as_str()), (vec!["page-size"], "page-size "));
        assert_eq!(at("-- @conf|\nSELECT 1", PG, None).items[0].insert, "confirm");
        let c = at("SELECT 1;\n-- @na|\nSELECT 2", PG, None);
        assert_eq!((labels(&c), c.start), (vec!["name"], 14));
        assert_eq!(at("--@pa|", PG, None).items[0].label, "page-size");
        // A value is being typed, or it is not a directive.
        assert!(at("-- @name us|", PG, None).items.is_empty());
        assert!(at("-- name|", PG, None).items.is_empty());
        // Inside a statement `-- @` is just a comment, but completing it is harmless and still useful.
        assert_eq!(at("SELECT 1 -- @co|", MY, None).items[0].label, "confirm");
    }

    #[test]
    fn second_statement_ignores_the_first() {
        let s = schema();
        let c = at("SELECT * FROM orders;\nSELECT | FROM \"Users\";", PG, Some(&s));
        assert!(labels(&c).contains(&"email") && !labels(&c).contains(&"total"));
        let c = at("SELECT 1;\nSELECT * FROM |", PG, Some(&s));
        assert!(labels(&c).contains(&"orders"));
        // Cursor in the middle of a statement.
        let c = at("SELECT o|, 1 FROM orders", PG, Some(&s));
        assert!(labels(&c).contains(&"total") || labels(&c).contains(&"user_id"));
    }

    #[test]
    fn refs_scan() {
        let r = refs("SELECT 1 FROM public.a x, \"B\" AS y JOIN c ON c.id = x.id LEFT JOIN d USING (id) WHERE 1 = 1", PG);
        let got: Vec<_> = r.iter().map(|r| (r.schema.as_deref(), r.table.as_str(), r.alias.as_deref())).collect();
        assert_eq!(got, [(Some("public"), "a", Some("x")), (None, "B", Some("y")), (None, "c", None), (None, "d", None)]);
        // Subqueries and functions are not tables; their own FROM is.
        let r = refs("SELECT * FROM (SELECT 1 FROM t) q, generate_series(1, 2) g", PG);
        assert_eq!(r.iter().map(|r| r.table.as_str()).collect::<Vec<_>>(), ["t"]);
        let r = refs("UPDATE t SET a = 1 FROM u WHERE t.id = u.id; INSERT INTO v (a) VALUES (1)", PG);
        assert_eq!(r.iter().map(|r| r.table.as_str()).collect::<Vec<_>>(), ["t", "u", "v"]);
    }

    #[test]
    fn bad_offsets_do_not_panic() {
        assert!(complete("SELECT é", 8, PG, None, None).items.iter().all(|_| true));
        assert!(complete("SELECT é", 100, PG, None, None).items.is_empty());
        assert!(complete("é", 1, PG, None, None).items.is_empty()); // inside a character
        assert!(complete("", 0, PG, None, None).items.len() > 5);
    }
}
