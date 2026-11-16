//! Splits a `.sql` file into statements with positions (Postgres / MySQL dialects).
//!
//! Splitting never fails: problems go to `diagnostics`. Offsets are byte offsets into the
//! source, lines are 1-based.

use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Dialect {
    #[default]
    Postgres,
    /// MySQL and MariaDB.
    MySql,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// SELECT-like: can be wrapped in `SELECT * FROM (…) AS q` for paging, sorting and filters.
    Query,
    /// Returns rows but can't be wrapped: SHOW, EXPLAIN, DESCRIBE.
    Read,
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement {
    /// Text sent to the server: first token up to the delimiter, without leading or trailing
    /// comments and whitespace.
    pub range: Range<usize>,
    pub line: usize,
    /// `-- @name`
    pub name: Option<String>,
    /// `-- @page-size`
    pub page_size: Option<usize>,
    /// `-- @confirm`
    pub confirm: bool,
    pub kind: Kind,
}

impl Statement {
    pub fn text<'a>(&self, src: &'a str) -> &'a str {
        &src[self.range.clone()]
    }

    /// `@name`, or the first line of the statement.
    pub fn label<'a>(&'a self, src: &'a str) -> &'a str {
        self.name.as_deref().unwrap_or_else(|| self.text(src).lines().next().unwrap_or("").trim_end())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub range: Range<usize>,
    pub severity: Severity,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SqlFile {
    /// `-- @connection` before the first statement.
    pub connection: Option<String>,
    pub statements: Vec<Statement>,
    pub diagnostics: Vec<Diagnostic>,
}

impl SqlFile {
    /// The statement the cursor belongs to: a cursor between statements (blank line, comment)
    /// belongs to the previous one, a cursor before the first one to the first.
    pub fn statement_at(&self, offset: usize) -> Option<&Statement> {
        let i = self.statements.partition_point(|s| s.range.start <= offset);
        self.statements.get(i.saturating_sub(1))
    }
}

pub const DIRECTIVES: &[&str] = &["connection", "name", "page-size", "confirm"];

pub fn split(src: &str, dialect: Dialect) -> SqlFile {
    split_words(src, dialect, false).0
}

type Words<'a> = Vec<(u32, &'a str)>;

/// `split`, plus (with `keep`) the unquoted words with their parenthesis depth of every statement.
fn split_words(src: &str, dialect: Dialect, keep: bool) -> (SqlFile, Vec<Words<'_>>) {
    Splitter {
        src,
        b: src.as_bytes(),
        pg: dialect == Dialect::Postgres,
        out: SqlFile::default(),
        delim: ";".into(),
        start: None,
        last: 0,
        words: Vec::new(),
        depth: 0,
        pending: Pending::default(),
        lines: (0, 1),
        keep,
        kept: Vec::new(),
    }
    .run()
}

#[derive(Default)]
struct Pending {
    name: Option<String>,
    page_size: Option<usize>,
    confirm: bool,
}

struct Splitter<'a> {
    src: &'a str,
    b: &'a [u8],
    pg: bool,
    out: SqlFile,
    delim: String,
    /// First token of the current statement.
    start: Option<usize>,
    /// End of the current statement's last token.
    last: usize,
    /// Unquoted words of the current statement with their parenthesis depth, for `classify`.
    words: Vec<(u32, &'a str)>,
    depth: u32,
    /// Directives seen since the previous statement.
    pending: Pending,
    /// (offset, line) of the last line lookup: lines are counted incrementally.
    lines: (usize, usize),
    keep: bool,
    kept: Vec<Words<'a>>,
}

impl<'a> Splitter<'a> {
    fn run(mut self) -> (SqlFile, Vec<Words<'a>>) {
        let b = self.b;
        let (mut i, mut line_start) = (0, true);
        while i < b.len() {
            let c = b[i];
            if c.is_ascii_whitespace() {
                line_start |= c == b'\n';
                i += 1;
                continue;
            }
            let at_line_start = std::mem::replace(&mut line_start, false);
            if b[i..].starts_with(self.delim.as_bytes()) {
                self.finish();
                i += self.delim.len();
                continue;
            }
            if c == b'-'
                && b.get(i + 1) == Some(&b'-')
                // MySQL: `--` starts a comment only before whitespace or a control char.
                && (self.pg || b.get(i + 2).is_none_or(|c| c.is_ascii_whitespace() || c.is_ascii_control()))
            {
                let end = line_end(b, i);
                self.directive(i..end);
                i = end;
                continue;
            }
            if c == b'#' && !self.pg {
                i = line_end(b, i);
                continue;
            }
            if c == b'/' && b.get(i + 1) == Some(&b'*') {
                i = self.block_comment(i);
                continue;
            }
            if at_line_start && !self.pg && is_word_start(c) {
                let end = word_end(b, i);
                if self.src[i..end].eq_ignore_ascii_case("delimiter") {
                    i = self.delimiter_command(i, end);
                    continue;
                }
            }
            self.touch(i);
            i = match c {
                b'\'' => self.quoted(i, b'\'', !self.pg),
                b'"' => self.quoted(i, b'"', !self.pg),
                b'`' if !self.pg => self.quoted(i, b'`', false),
                b'$' if self.pg => self.dollar(i),
                b'(' => {
                    self.depth += 1;
                    i + 1
                }
                b')' => {
                    self.depth = self.depth.saturating_sub(1);
                    i + 1
                }
                _ if is_word_start(c) => {
                    let end = word_end(b, i);
                    let word = &self.src[i..end];
                    self.words.push((self.depth, word));
                    if self.pg && word.eq_ignore_ascii_case("e") && b.get(end) == Some(&b'\'') {
                        self.quoted(end, b'\'', true) // E'…' with backslash escapes
                    } else {
                        end
                    }
                }
                _ => i + 1,
            };
            self.last = i;
        }
        self.finish();
        (self.out, self.kept)
    }

    /// A statement token at `i`: starts a statement if none is open.
    fn touch(&mut self, i: usize) {
        if self.start.is_none() {
            self.start = Some(i);
            let (off, line) = self.lines;
            self.lines = (i, line + self.b[off..i].iter().filter(|&&c| c == b'\n').count());
        }
    }

    fn finish(&mut self) {
        let pending = std::mem::take(&mut self.pending);
        if let Some(start) = self.start.take() {
            self.out.statements.push(Statement {
                range: start..self.last,
                line: self.lines.1,
                name: pending.name,
                page_size: pending.page_size,
                confirm: pending.confirm,
                kind: classify(&self.words),
            });
            if let (Some(reason @ ("UPDATE without WHERE" | "DELETE without WHERE")), Some(&(_, first))) = (reason(&self.words), self.words.first()) {
                let what = if reason.starts_with("UPDATE") { "changes" } else { "deletes" };
                self.diag(start..start + first.len(), Severity::Warning, format!("{reason}: every row {what}"));
            }
        }
        if self.keep {
            self.kept.push(std::mem::take(&mut self.words));
        }
        self.words.clear();
        self.depth = 0;
    }

    fn diag(&mut self, range: Range<usize>, severity: Severity, message: impl Into<String>) {
        self.out.diagnostics.push(Diagnostic { range, severity, message: message.into() });
    }

    /// Returns the offset after the closing quote. A doubled quote is an escaped quote.
    fn quoted(&mut self, i: usize, quote: u8, backslash: bool) -> usize {
        let mut j = i + 1;
        while let Some(&c) = self.b.get(j) {
            if (backslash && c == b'\\') || (c == quote && self.b.get(j + 1) == Some(&quote)) {
                j += 2;
            } else if c == quote {
                return j + 1;
            } else {
                j += 1;
            }
        }
        self.diag(i..self.b.len(), Severity::Error, format!("Unclosed {}", quote as char));
        self.b.len()
    }

    fn block_comment(&mut self, i: usize) -> usize {
        let (b, mut j, mut depth) = (self.b, i + 2, 1);
        while j < b.len() {
            if self.pg && b[j..].starts_with(b"/*") {
                depth += 1; // Postgres comments nest
                j += 2;
            } else if b[j..].starts_with(b"*/") {
                depth -= 1;
                j += 2;
                if depth == 0 {
                    return j;
                }
            } else {
                j += 1;
            }
        }
        self.diag(i..b.len(), Severity::Error, "Unclosed /* comment");
        b.len()
    }

    /// `$$ … $$` / `$tag$ … $tag$`; a lone `$` or `$1` is just a token.
    fn dollar(&mut self, i: usize) -> usize {
        let b = self.b;
        let mut j = i + 1;
        if b.get(j).is_some_and(|&c| c.is_ascii_alphabetic() || c == b'_') {
            while b.get(j).is_some_and(|&c| c.is_ascii_alphanumeric() || c == b'_') {
                j += 1;
            }
        }
        if b.get(j) != Some(&b'$') {
            return i + 1;
        }
        let tag = &self.src[i..=j];
        match self.src[j + 1..].find(tag) {
            Some(pos) => j + 1 + pos + tag.len(),
            None => {
                self.diag(i..b.len(), Severity::Error, format!("Unclosed {tag}"));
                b.len()
            }
        }
    }

    /// MySQL client command `DELIMITER x`: ends the open statement, never reaches the server.
    fn delimiter_command(&mut self, i: usize, word_end: usize) -> usize {
        self.finish();
        let end = line_end(self.b, i);
        match self.src[word_end..end].split_whitespace().next() {
            Some(d) => self.delim = d.into(),
            None => self.diag(i..end, Severity::Error, "DELIMITER needs a value"),
        }
        end
    }

    /// `-- @word value` between statements; other comments are plain comments.
    fn directive(&mut self, comment: Range<usize>) {
        if self.start.is_some() {
            return;
        }
        let Some(rest) = self.src[comment.start + 2..comment.end].trim().strip_prefix('@') else {
            return;
        };
        let (word, value) = rest.split_once(char::is_whitespace).map_or((rest, ""), |(w, v)| (w, v.trim()));
        let warn = |s: &mut Self, msg: String| s.diag(comment.clone(), Severity::Warning, msg);
        match (word, value) {
            ("connection" | "name", "") => warn(self, format!("`@{word}` needs a value")),
            ("connection", _) if self.out.statements.is_empty() => self.out.connection = Some(value.into()),
            ("connection", _) => warn(self, "`@connection` applies only at the top of the file".into()),
            ("name", _) => self.pending.name = Some(value.into()),
            ("page-size", _) => match value.parse() {
                Ok(n) if n > 0 => self.pending.page_size = Some(n),
                _ => warn(self, "`@page-size` needs a positive number".into()),
            },
            ("confirm", _) => self.pending.confirm = true,
            _ => warn(self, format!("Unknown directive `@{word}`")),
        }
    }
}

fn is(w: &str, list: &[&str]) -> bool {
    list.iter().any(|k| w.eq_ignore_ascii_case(k))
}

/// The statement an `EXPLAIN [ANALYZE | (options)] …` or MariaDB `ANALYZE …` wraps, and whether
/// it actually runs (`ANALYZE`). `EXPLAIN` without it only plans.
fn explain_inner<'w, 'a>(words: &'w [(u32, &'a str)]) -> Option<(&'w [(u32, &'a str)], bool)> {
    let first = words.first()?.1;
    let analyze_first = is(first, &["analyze", "analyse"]);
    if !analyze_first && !is(first, &["explain"]) {
        return None;
    }
    let options = ["analyze", "analyse", "verbose", "extended", "partitions", "format", "json", "tree", "traditional"];
    let at = 1 + words[1..].iter().position(|(d, w)| *d == 0 && !is(w, &options))?;
    let runs = analyze_first || words[1..at].iter().any(|(_, w)| is(w, &["analyze", "analyse"]));
    Some((&words[at..], runs))
}

fn classify(words: &[(u32, &str)]) -> Kind {
    let Some(&(_, first)) = words.first() else { return Kind::Other };
    if is(first, &["explain"]) {
        // `EXPLAIN ANALYZE <DML>` executes the DML: never "just a read".
        return match explain_inner(words) {
            Some((inner, true)) if classify(inner) != Kind::Query => Kind::Other,
            _ => Kind::Read,
        };
    }
    if is(first, &["show", "describe", "desc"]) {
        return Kind::Read;
    }
    if is(first, &["with"]) {
        // ponytail: a column named `delete` / `update` in a CTE makes it Other (just not wrapped).
        // Postgres can't nest a data-modifying WITH in a subquery, so any DML word disqualifies.
        if words.iter().any(|(_, w)| is(w, &["insert", "update", "delete", "merge"])) {
            return Kind::Other;
        }
    } else if !is(first, &["select", "table", "values"]) {
        return Kind::Other;
    }
    let top: Vec<&str> = words.iter().filter(|(d, _)| *d == 0).map(|(_, w)| *w).collect();
    let locks = top.windows(2).any(|p| is(p[0], &["for"]) && is(p[1], &["update", "share", "no", "key"]));
    if locks || top.iter().any(|w| is(w, &["into"])) { Kind::Other } else { Kind::Query }
}

/// The statement (text as returned by `Statement::text`) needs a confirmation on production:
/// `DROP`, `TRUNCATE`, `ALTER`, and `UPDATE` / `DELETE` without a top-level `WHERE`.
/// A token check, not a parser: comments, strings and quoted names are skipped, and a `WHERE`
/// inside a subquery does not count.
// ponytail: `WITH … DELETE` is looked up by its first top-level UPDATE / DELETE; a parser would be exact.
pub fn is_destructive(text: &str, dialect: Dialect) -> bool {
    destructive_reason(text, dialect).is_some()
}

/// Why [`is_destructive`] says yes (`"DROP"`, `"UPDATE without WHERE"`, …).
pub fn destructive_reason(text: &str, dialect: Dialect) -> Option<&'static str> {
    // The splitter's own lexer (strings, comments, dollar quotes), so both agree on what is a word.
    split_words(text, dialect, true).1.iter().find_map(|w| reason(w))
}

/// What the top level of a SELECT-like statement has, for the page wrapper.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TopLevel {
    /// `LIMIT`, `OFFSET` or `FETCH` outside parentheses.
    pub limit: bool,
    pub order_by: bool,
    /// `LOCK IN SHARE MODE` / `FOR UPDATE|SHARE`: a clause that must stay last.
    pub lock: bool,
    /// A `VALUES …` statement.
    pub values: bool,
}

pub fn top_level(text: &str, dialect: Dialect) -> TopLevel {
    let (_, all) = split_words(text, dialect, true);
    let top: Vec<&str> = all.first().map(|w| w.iter().filter(|(d, _)| *d == 0).map(|(_, w)| *w).collect()).unwrap_or_default();
    TopLevel {
        limit: top.iter().any(|w| is(w, &["limit", "offset", "fetch"])),
        order_by: top.windows(2).any(|p| is(p[0], &["order"]) && is(p[1], &["by"])),
        lock: top.iter().any(|w| is(w, &["lock", "for"])),
        values: top.first().is_some_and(|w| is(w, &["values"])),
    }
}

fn reason(words: &[(u32, &str)]) -> Option<&'static str> {
    let &(_, first) = words.first()?;
    if let Some((inner, runs)) = explain_inner(words) {
        return if runs { reason(inner) } else { None };
    }
    if is(first, &["drop", "truncate", "alter"]) {
        return Some(match first.to_ascii_lowercase().as_str() {
            "drop" => "DROP",
            "truncate" => "TRUNCATE",
            _ => "ALTER",
        });
    }
    if !is(first, &["update", "delete", "with"]) {
        return None; // SELECT … FOR UPDATE and the like
    }
    let top: Vec<&str> = words.iter().filter(|(d, _)| *d == 0).map(|(_, w)| *w).collect();
    let at = top.iter().position(|w| is(w, &["update", "delete"]))?;
    if top[at..].iter().any(|w| is(w, &["where"])) {
        return None;
    }
    Some(if is(top[at], &["update"]) { "UPDATE without WHERE" } else { "DELETE without WHERE" })
}

fn line_end(b: &[u8], i: usize) -> usize {
    b[i..].iter().position(|&c| c == b'\n').map_or(b.len(), |p| i + p)
}

fn is_word_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c >= 0x80
}

fn word_end(b: &[u8], i: usize) -> usize {
    let n = b[i..].iter().position(|&c| !(c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80));
    n.map_or(b.len(), |n| i + n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use Dialect::{MySql as MY, Postgres as PG};

    fn texts(src: &str, d: Dialect) -> Vec<&str> {
        split(src, d).statements.iter().map(|s| s.text(src)).collect()
    }

    #[test]
    fn splits() {
        let cases: &[(&str, Dialect, &str, &[&str])] = &[
            ("basic", PG, "SELECT 1; SELECT 2;", &["SELECT 1", "SELECT 2"]),
            ("last without delimiter", PG, "SELECT 1;\nSELECT 2", &["SELECT 1", "SELECT 2"]),
            ("empty statements skipped", PG, ";;\n ; SELECT 1;;", &["SELECT 1"]),
            ("empty file", PG, " \n-- only a comment\n", &[]),
            ("semicolon in string", PG, "SELECT ';'; SELECT 2", &["SELECT ';'", "SELECT 2"]),
            ("doubled quote", PG, "SELECT 'it''s;'; SELECT 2", &["SELECT 'it''s;'", "SELECT 2"]),
            ("pg backslash is literal", PG, r"SELECT 'a\'; SELECT 2", &[r"SELECT 'a\'", "SELECT 2"]),
            ("pg E-string escapes", PG, r"SELECT E'a\';b'; SELECT 2", &[r"SELECT E'a\';b'", "SELECT 2"]),
            ("mysql backslash escapes", MY, r"SELECT 'a\';b'; SELECT 2", &[r"SELECT 'a\';b'", "SELECT 2"]),
            ("mysql double-quoted string", MY, r#"SELECT "a\";b"; SELECT 2"#, &[r#"SELECT "a\";b""#, "SELECT 2"]),
            ("pg quoted identifier", PG, r#"SELECT 1 AS "a;""b"; SELECT 2"#, &[r#"SELECT 1 AS "a;""b""#, "SELECT 2"]),
            ("backticks", MY, "SELECT `a;b` FROM t; SELECT 2", &["SELECT `a;b` FROM t", "SELECT 2"]),
            ("line comment", PG, "SELECT 1 -- x;\n; SELECT 2", &["SELECT 1", "SELECT 2"]),
            ("comment inside statement kept", PG, "SELECT 1 -- x;\nFROM t;", &["SELECT 1 -- x;\nFROM t"]),
            ("leading comments excluded", PG, "-- head\n/* c; */\nSELECT 1;", &["SELECT 1"]),
            ("trailing block comment excluded", PG, "SELECT 1 /* ; */;", &["SELECT 1"]),
            ("mysql hash comment", MY, "SELECT 1 # x;\n; SELECT 2", &["SELECT 1", "SELECT 2"]),
            ("hash is an operator in pg", PG, "SELECT a # b; SELECT 2", &["SELECT a # b", "SELECT 2"]),
            ("mysql -- needs a space", MY, "SELECT 1--1;\nSELECT 2", &["SELECT 1--1", "SELECT 2"]),
            ("pg nested block comment", PG, "/* a /* ; */ ; */ SELECT 1; SELECT 2", &["SELECT 1", "SELECT 2"]),
            ("mysql block comments don't nest", MY, "/* a /* ; */ SELECT 1; SELECT 2", &["SELECT 1", "SELECT 2"]),
            (
                "dollar quote",
                PG,
                "CREATE FUNCTION f() RETURNS int AS $$ SELECT 1; $$ LANGUAGE sql; SELECT 2",
                &["CREATE FUNCTION f() RETURNS int AS $$ SELECT 1; $$ LANGUAGE sql", "SELECT 2"],
            ),
            (
                "tagged dollar quote",
                PG,
                "DO $body$ BEGIN PERFORM '$$;'; END $body$; SELECT 2",
                &["DO $body$ BEGIN PERFORM '$$;'; END $body$", "SELECT 2"],
            ),
            ("params aren't dollar quotes", PG, "SELECT $1; SELECT $2", &["SELECT $1", "SELECT $2"]),
            ("dollar inside identifier", PG, "SELECT a$b$c; SELECT 2", &["SELECT a$b$c", "SELECT 2"]),
            (
                "delimiter",
                MY,
                "DELIMITER //\nCREATE PROCEDURE p() BEGIN SELECT 1; SELECT 2; END //\nDELIMITER ;\nSELECT 3;",
                &["CREATE PROCEDURE p() BEGIN SELECT 1; SELECT 2; END", "SELECT 3"],
            ),
            (
                "delimiter ends a pending statement, any case",
                MY,
                "SELECT 1\n  delimiter $$\nSELECT 2$$",
                &["SELECT 1", "SELECT 2"],
            ),
            ("delimiter is plain sql in pg", PG, "DELIMITER //\nSELECT 1;", &["DELIMITER //\nSELECT 1"]),
            (
                "crlf",
                PG,
                "SELECT 1;\r\n-- c\r\nSELECT\r\n  2;\r\n",
                &["SELECT 1", "SELECT\r\n  2"],
            ),
            ("utf-8", PG, "SELECT 'привіт;' AS назва; SELECT 2", &["SELECT 'привіт;' AS назва", "SELECT 2"]),
        ];
        for (name, d, src, want) in cases {
            assert_eq!(texts(src, *d), *want, "{name}");
        }
    }

    #[test]
    fn kinds() {
        let cases: &[(&str, Kind)] = &[
            ("select 1", Kind::Query),
            ("(SELECT 1) UNION (SELECT 2)", Kind::Query),
            ("TABLE t", Kind::Query),
            ("VALUES (1), (2)", Kind::Query),
            ("WITH a AS (SELECT 1) SELECT * FROM a", Kind::Query),
            ("WITH RECURSIVE t(n) AS (VALUES (1) UNION ALL SELECT n + 1 FROM t) SELECT * FROM t", Kind::Query),
            ("SELECT 'into', \"for\" FROM t -- into\n", Kind::Query),
            ("SELECT * FROM (SELECT 1 INTO x) q", Kind::Query),
            ("WITH a AS (DELETE FROM t RETURNING *) SELECT * FROM a", Kind::Other),
            ("WITH a AS (SELECT 1) INSERT INTO t SELECT * FROM a", Kind::Other),
            ("SELECT * INTO new_t FROM t", Kind::Other),
            ("SELECT * FROM t FOR UPDATE", Kind::Other),
            ("SELECT * FROM t FOR NO KEY UPDATE", Kind::Other),
            ("SHOW tables", Kind::Read),
            ("EXPLAIN SELECT 1", Kind::Read),
            ("EXPLAIN ANALYZE SELECT 1", Kind::Read),
            ("EXPLAIN (ANALYZE) WITH a AS (SELECT 1) SELECT * FROM a", Kind::Read),
            ("EXPLAIN DELETE FROM t", Kind::Read),
            ("EXPLAIN (VERBOSE, COSTS OFF) UPDATE t SET a = 1", Kind::Read),
            ("EXPLAIN ANALYZE DELETE FROM t", Kind::Other),
            ("EXPLAIN ANALYZE VERBOSE UPDATE t SET a = 1", Kind::Other),
            ("EXPLAIN (ANALYZE, BUFFERS) INSERT INTO t VALUES (1)", Kind::Other),
            ("ANALYZE SELECT 1", Kind::Other),
            ("DESCRIBE t", Kind::Read),
            ("INSERT INTO t VALUES (1) RETURNING id", Kind::Other),
            ("UPDATE t SET a = 1", Kind::Other),
            ("CALL p()", Kind::Other),
        ];
        for (src, want) in cases {
            assert_eq!(split(src, PG).statements[0].kind, *want, "{src}");
        }
    }

    #[test]
    fn lines_and_cursor() {
        let src = "-- head\n\nSELECT 1;\n\n-- between\nSELECT\n2; SELECT 3";
        let f = split(src, PG);
        assert_eq!(f.statements.iter().map(|s| s.line).collect::<Vec<_>>(), [3, 6, 7]);
        let at = |needle: &str| f.statement_at(src.find(needle).unwrap()).unwrap().text(src);
        assert_eq!(at("head"), "SELECT 1"); // before the first statement
        assert_eq!(at("SELECT 1"), "SELECT 1");
        assert_eq!(at("between"), "SELECT 1"); // comment between statements: the previous one
        assert_eq!(at("2;"), "SELECT\n2");
        assert_eq!(at("SELECT 3"), "SELECT 3");
        assert_eq!(f.statement_at(src.len()).unwrap().text(src), "SELECT 3");
        assert_eq!(split("", PG).statement_at(0), None);
    }

    #[test]
    fn directives() {
        let src = "-- @connection dev-pg\n\n-- @name users_by_day\n-- @page-size 50\n-- @confirm\n-- plain\nSELECT 1\n-- @name ignored\n;\nSELECT 2;";
        let f = split(src, PG);
        assert_eq!(f.connection.as_deref(), Some("dev-pg"));
        let s = &f.statements[0];
        assert_eq!((s.name.as_deref(), s.page_size, s.confirm), (Some("users_by_day"), Some(50), true));
        assert_eq!(s.label(src), "users_by_day");
        let s = &f.statements[1];
        assert_eq!((s.name.as_deref(), s.page_size, s.confirm), (None, None, false));
        assert_eq!(s.label(src), "SELECT 2");
        assert!(f.diagnostics.is_empty(), "{:?}", f.diagnostics);
    }

    #[test]
    fn directive_warnings() {
        let src = "SELECT 1;\n-- @connection x\n-- @page-size zero\n-- @nmae a\n-- @name\nSELECT 2;";
        let f = split(src, PG);
        let msgs: Vec<_> = f.diagnostics.iter().map(|d| (d.severity, src[d.range.clone()].to_string())).collect();
        let w = Severity::Warning;
        assert_eq!(
            msgs,
            [
                (w, "-- @connection x".into()),
                (w, "-- @page-size zero".into()),
                (w, "-- @nmae a".into()),
                (w, "-- @name".into())
            ]
        );
        assert_eq!(f.connection, None);
    }

    #[test]
    fn unclosed() {
        for (src, d, msg) in [
            ("SELECT 1; SELECT 'abc", PG, "Unclosed '"),
            ("SELECT 1; SELECT `abc", MY, "Unclosed `"),
            ("SELECT 1; /* abc", PG, "Unclosed /* comment"),
            ("SELECT 1; DO $f$ BEGIN", PG, "Unclosed $f$"),
            ("DELIMITER\nSELECT 1", MY, "DELIMITER needs a value"),
        ] {
            let f = split(src, d);
            assert_eq!(f.diagnostics.len(), 1, "{src}");
            assert_eq!((f.diagnostics[0].severity, f.diagnostics[0].message.as_str()), (Severity::Error, msg), "{src}");
        }
        // The rest of the file stays one statement.
        assert_eq!(texts("SELECT 'a; b", PG), ["SELECT 'a; b"]);
    }

    #[test]
    fn update_and_delete_without_where_warn() {
        let src = "UPDATE t SET a = 1;\nDELETE FROM t WHERE id = 1;\nupdate t set a = 1 where id = 1;\nDELETE FROM u";
        let f = split(src, PG);
        let got: Vec<_> = f.diagnostics.iter().map(|d| (d.severity, &src[d.range.clone()], d.message.as_str())).collect();
        assert_eq!(
            got,
            [
                (Severity::Warning, "UPDATE", "UPDATE without WHERE: every row changes"),
                (Severity::Warning, "DELETE", "DELETE without WHERE: every row deletes")
            ]
        );
        assert!(split("DROP TABLE t; EXPLAIN DELETE FROM t", PG).diagnostics.is_empty());
    }

    #[test]
    fn top_level_clauses() {
        let tl = |s: &str, d| top_level(s, d);
        assert_eq!(tl("SELECT 1", MY), TopLevel::default());
        assert!(tl("SELECT a FROM t ORDER BY a DESC", MY).order_by);
        assert!(!tl("SELECT a FROM t ORDER BY a DESC", MY).limit);
        assert!(tl("SELECT a FROM t LIMIT 5", MY).limit);
        assert!(tl("SELECT a FROM t LIMIT 5, 10 -- x", MY).limit);
        // Nested, quoted and commented ones are not top-level.
        let nested = tl("SELECT 'limit', `order` FROM (SELECT a FROM t ORDER BY a LIMIT 1) x /* limit */ -- order by", MY);
        assert_eq!((nested.limit, nested.order_by), (false, false));
        assert!(tl("SELECT a FROM t ORDER BY a FETCH FIRST 3 ROWS ONLY", MY).limit);
        assert!(tl("SELECT a FROM t LOCK IN SHARE MODE", MY).lock);
        assert!(tl("VALUES ROW(1)", MY).values);
        assert!(tl("(SELECT 1) UNION (SELECT 2) ORDER BY 1", MY).order_by);
    }

    #[test]
    fn destructive() {
        for (sql, d, want) in [
            ("DELETE FROM t", PG, true),
            ("delete from t where id = 1", PG, false),
            ("UPDATE t SET a = 1", PG, true),
            ("UPDATE t SET a = (SELECT 1 FROM u WHERE u.id = 1)", PG, true), // the WHERE is a subquery's
            ("UPDATE t SET a = 1 WHERE id IN (SELECT id FROM u)", PG, false),
            ("-- where\nDELETE FROM t /* where */", PG, true),
            ("UPDATE t SET a = 'where'", PG, true),
            ("UPDATE t SET a = $q$ where $q$", PG, true),
            ("WITH x AS (SELECT 1) DELETE FROM t", PG, true),
            ("WITH x AS (SELECT 1) DELETE FROM t WHERE id = 1", PG, false),
            ("WITH x AS (SELECT 1) SELECT * FROM x", PG, false),
            ("DROP TABLE t", PG, true),
            ("TRUNCATE t", MY, true),
            ("ALTER TABLE t ADD c int", MY, true),
            ("SELECT * FROM t FOR UPDATE", PG, false),
            ("SELECT 'drop table'", PG, false),
            ("INSERT INTO t VALUES (1)", PG, false),
            ("DELETE FROM `where`", MY, true),
            ("", PG, false),
            ("UPDATE t SET a = $1", PG, true),
            ("UPDATE t SET a = $1 WHERE id = $2", PG, false),
            ("INSERT INTO t VALUES (1) ON CONFLICT DO UPDATE SET a = 1", PG, false),
            (r"UPDATE t SET a = 'it\'s where' ", MY, true),
            ("DELETE FROM t # where\n", MY, true),
            ("DELETE FROM t WHERE a = `where`", MY, false),
            // The main lexer's string rules: Postgres E'…' and MySQL backslash escapes.
            (r"UPDATE t SET a = E'\' where '", PG, true),
            (r"UPDATE t SET a = e'\' where ' WHERE id = 1", PG, false),
            (r#"UPDATE t SET a = "\" where ""#, MY, true),
            (r"UPDATE t SET a = '\' where '", MY, true),
            (r"UPDATE t SET a = 'a\' where '", PG, false), // a backslash is a plain char in a Postgres string
            // EXPLAIN ANALYZE / MariaDB ANALYZE run the statement.
            ("EXPLAIN ANALYZE DELETE FROM t", PG, true),
            ("EXPLAIN (ANALYZE, BUFFERS) UPDATE t SET a = 1", PG, true),
            ("EXPLAIN ANALYZE UPDATE t SET a = 1 WHERE id = 1", PG, false),
            ("EXPLAIN DELETE FROM t", PG, false),
            ("EXPLAIN (VERBOSE) DELETE FROM t", PG, false),
            ("EXPLAIN ANALYZE SELECT 1", PG, false),
            ("EXPLAIN ANALYZE DELETE FROM t", MY, true),
            ("ANALYZE DELETE FROM t", MY, true),
            ("ANALYZE FORMAT=JSON UPDATE t SET a = 1", MY, true),
            ("ANALYZE TABLE t", MY, false),
        ] {
            assert_eq!(is_destructive(sql, d), want, "{sql}");
        }
        assert_eq!(destructive_reason("  truncate t", PG), Some("TRUNCATE"));
        assert_eq!(destructive_reason("alter table t add c int", PG), Some("ALTER"));
        assert_eq!(destructive_reason("update t set a = 1", PG), Some("UPDATE without WHERE"));
        assert_eq!(destructive_reason("DELETE FROM t", PG), Some("DELETE without WHERE"));
    }
}
