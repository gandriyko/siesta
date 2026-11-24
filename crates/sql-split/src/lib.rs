//! Splits a query file into statements with positions: `.sql` (Postgres / MySQL dialects),
//! Elasticsearch `.es` files (requests, see [`es`]) and MongoDB `.mongodb.js` files (mongosh
//! calls, see [`mongo`]).
//!
//! Splitting never fails: problems go to `diagnostics`. Offsets are byte offsets into the
//! source, lines are 1-based.

use std::ops::Range;

mod es;
mod mongo;
pub use es::{Request as EsRequest, kind as es_kind, normalize_path as es_normalize_path, request as es_request};
pub use mongo::{Call as MongoCall, kind as mongo_kind, parse as mongo_parse};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Dialect {
    #[default]
    Postgres,
    /// MySQL and MariaDB.
    MySql,
    /// Elasticsearch requests in Kibana console format (`.es` files).
    Elastic,
    /// MongoDB calls in mongosh syntax (`.mongodb.js` files).
    Mongo,
}

impl Dialect {
    /// What starts a directive comment: `--` in SQL, `#` in `.es` files, `//` in `.mongodb.js`.
    pub fn comment(self) -> &'static str {
        match self {
            Dialect::Elastic => "#",
            Dialect::Mongo => "//",
            Dialect::Postgres | Dialect::MySql => "--",
        }
    }

    /// Postgres or MySQL: statements are SQL.
    pub fn is_sql(self) -> bool {
        matches!(self, Dialect::Postgres | Dialect::MySql)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// SELECT-like: can be wrapped in `SELECT * FROM (…) AS q` for paging, sorting and filters.
    Query,
    /// Returns rows but can't be wrapped: SHOW, EXPLAIN, DESCRIBE.
    Read,
    Other,
}

impl Kind {
    /// Whether a statement of this kind asks for confirmation on a `prod` connection: everything
    /// that is not a plain read (`Other`: DML, DDL, `SELECT … INTO`, `FOR UPDATE`, SET, BEGIN, CALL…).
    pub fn needs_prod_confirm(self) -> bool {
        self == Kind::Other
    }
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
    /// Where that directive's comment sits in the source.
    pub connection_at: Option<Range<usize>>,
    /// `-- @database` before the first statement, and where it sits.
    pub database: Option<String>,
    pub database_at: Option<Range<usize>>,
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

pub const DIRECTIVES: &[&str] = &["connection", "database", "name", "page-size", "confirm"];

/// `src` with the value of its `-- @connection` directive replaced by `name`, or the whole
/// directive line removed for `None`. `None` back when the file has no directive.
pub fn rewrite_connection(src: &str, dialect: Dialect, name: Option<&str>) -> Option<String> {
    let at = split(src, dialect).connection_at?;
    rewrite_directive_at(src, at, "connection", name)
}

/// Same as [`rewrite_connection`] for the `-- @database` directive.
pub fn rewrite_database(src: &str, dialect: Dialect, name: Option<&str>) -> Option<String> {
    let at = split(src, dialect).database_at?;
    rewrite_directive_at(src, at, "database", name)
}

/// `src` running on connection `name` (`None`: none), as Respite's `# @env` line: the
/// `-- @connection` directive is replaced, inserted as the first line, or removed. A
/// `-- @database` line goes too when the connection changes (a database belongs to its server).
pub fn set_connection(src: &str, dialect: Dialect, name: Option<&str>) -> String {
    let file = split(src, dialect);
    if file.connection.as_deref() == name {
        return src.to_string();
    }
    let src = rewrite_database(src, dialect, None).unwrap_or_else(|| src.to_string());
    match (rewrite_connection(&src, dialect, name), name) {
        (Some(s), _) => s,
        (None, Some(n)) => format!("{} @connection {n}\n{src}", dialect.comment()),
        (None, None) => src,
    }
}

/// `src` running on database `name` (`None`: the connection's default): the `-- @database`
/// directive is replaced, removed, or inserted right after `-- @connection` (else first).
pub fn set_database(src: &str, dialect: Dialect, name: Option<&str>) -> String {
    let file = split(src, dialect);
    if file.database.as_deref() == name {
        return src.to_string();
    }
    match (rewrite_database(src, dialect, name), name) {
        (Some(s), _) => s,
        (None, Some(n)) => {
            // After the line holding `-- @connection`, so the two read together.
            let at = file.connection_at.map_or(0, |r| src[r.end..].find('\n').map_or(src.len(), |i| r.end + i + 1));
            let nl = if at == src.len() && !src.is_empty() && !src.ends_with('\n') { "\n" } else { "" };
            format!("{}{nl}{} @database {n}\n{}", &src[..at], dialect.comment(), &src[at..])
        }
        (None, None) => src.to_string(),
    }
}

fn rewrite_directive_at(src: &str, at: Range<usize>, word: &str, name: Option<&str>) -> Option<String> {
    let text = &src[at.clone()];
    let end = at.start + text.trim_end().len();
    if let Some(n) = name {
        let w = text.find(&format!("@{word}"))? + word.len() + 1;
        let from = at.start + w + (text[w..].len() - text[w..].trim_start().len());
        return Some(format!("{}{n}{}", &src[..from], &src[end..]));
    }
    // Removal takes the whole line (its newline too) unless something else shares it.
    let line_start = src[..at.start].rfind('\n').map_or(0, |i| i + 1);
    let alone = src[line_start..at.start].trim().is_empty();
    let (from, mut to) = if alone { (line_start, end) } else { (at.start, end) };
    if alone {
        match src[to..].find('\n') {
            Some(l) if src[to..to + l].trim().is_empty() => to += l + 1,
            None if src[to..].trim().is_empty() => to = src.len(),
            _ => {}
        }
    }
    Some(format!("{}{}", &src[..from], &src[to..]))
}

pub fn split(src: &str, dialect: Dialect) -> SqlFile {
    match dialect {
        Dialect::Elastic => return es::split(src),
        Dialect::Mongo => return mongo::split(src),
        Dialect::Postgres | Dialect::MySql => {}
    }
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
                // Postgres ends it at `\r` too: past it the server runs code this lexer would skip.
                let end = if self.pg { b[i..].iter().position(|&c| c == b'\n' || c == b'\r').map_or(b.len(), |p| i + p) } else { line_end(b, i) };
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
        // Postgres tags take non-ASCII letters too (`$é$`): read as a lone `$`, the string's text
        // would look like code here (a `WHERE` that is only text on the server).
        if b.get(j).is_some_and(|&c| c.is_ascii_alphabetic() || c == b'_' || c >= 0x80) {
            while b.get(j).is_some_and(|&c| c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80) {
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
        let top = self.out.statements.is_empty();
        directive_word(&mut self.out, &mut self.pending, rest, comment, top);
    }
}

/// A directive `rest` (after `@`) in the comment at `comment`; `top`: no statement before it.
fn directive_word(out: &mut SqlFile, pending: &mut Pending, rest: &str, comment: Range<usize>, top: bool) {
    let (word, value) = rest.split_once(char::is_whitespace).map_or((rest, ""), |(w, v)| (w, v.trim()));
    let mut warn = |msg: String| out.diagnostics.push(Diagnostic { range: comment.clone(), severity: Severity::Warning, message: msg });
    match (word, value) {
        ("connection" | "database" | "name", "") => warn(format!("`@{word}` needs a value")),
        ("database", _) if top => {
            out.database = Some(value.into());
            out.database_at = Some(comment);
        }
        ("database", _) => warn("`@database` applies only at the top of the file".into()),
        ("connection", _) if top => {
            out.connection = Some(value.into());
            out.connection_at = Some(comment);
        }
        ("connection", _) => warn("`@connection` applies only at the top of the file".into()),
        ("name", _) => pending.name = Some(value.into()),
        ("page-size", _) => match value.parse() {
            Ok(n) if n > 0 => pending.page_size = Some(n),
            _ => warn("`@page-size` needs a positive number".into()),
        },
        ("confirm", _) => pending.confirm = true,
        _ => warn(format!("Unknown directive `@{word}`")),
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
    if dialect == Dialect::Elastic {
        return es::requests(text).iter().find_map(es::reason);
    }
    if dialect == Dialect::Mongo {
        return mongo::split(text).statements.iter().filter_map(|s| mongo::parse(s.text(text)).ok()).find_map(|c| mongo::reason(&c));
    }
    // The splitter's own lexer (strings, comments, dollar quotes), so both agree on what is a word.
    split_words(text, dialect, true).1.iter().find_map(|w| reason(w))
}

/// Whether any statement of `text` changes the schema (`CREATE`, `ALTER`, `DROP`, `RENAME`,
/// `TRUNCATE`, `COMMENT ON`), so a cached schema is stale after it ran. Same top-level-token
/// approach as [`is_destructive`]; `EXPLAIN` without `ANALYZE` does not run its statement.
pub fn is_schema_changing(text: &str, dialect: Dialect) -> bool {
    if dialect == Dialect::Elastic {
        return es::requests(text).iter().any(es::schema_changing);
    }
    if dialect == Dialect::Mongo {
        return mongo::split(text).statements.iter().filter_map(|s| mongo::parse(s.text(text)).ok()).any(|c| mongo::schema_changing(&c));
    }
    fn changes(words: &[(u32, &str)]) -> bool {
        let Some(&(_, first)) = words.first() else { return false };
        if let Some((inner, runs)) = explain_inner(words) {
            return runs && changes(inner);
        }
        is(first, &["create", "alter", "drop", "rename", "truncate"]) || (is(first, &["comment"]) && words.get(1).is_some_and(|&(_, w)| is(w, &["on"])))
    }
    split_words(text, dialect, true).1.iter().any(|w| changes(w))
}

/// Why a `read_only` connection must not run `text` (`None`: allowed). Client-side check by
/// statement text: reads (`Query` / `Read`) and harmless session / transaction statements pass,
/// everything else (including `SELECT … INTO`, `FOR UPDATE`, data-modifying `WITH`,
/// `EXPLAIN ANALYZE <DML>`, `PREPARE`, unknown words) is refused. Switches to read-write are
/// the caller's separate guard. A function that writes inside a SELECT is not seen here;
/// the server's read-only session stops it.
// ponytail: token check, not a parser; a CTE column named `delete` is a false refusal.
pub fn read_only_violation(text: &str, dialect: Dialect) -> Option<String> {
    if dialect == Dialect::Elastic {
        return es::requests(text).iter().find_map(es::read_only_violation);
    }
    if dialect == Dialect::Mongo {
        return mongo::read_only_violation(text);
    }
    if let Some(m) = executable_comment(text, dialect) {
        return Some(format!("Read-only connection: {m}"));
    }
    split_words(text, dialect, true).1.iter().find_map(|w| {
        let &(_, first) = w.first()?;
        if classify(w) != Kind::Other {
            return None;
        }
        let second = w.get(1).map_or("", |&(_, s)| s);
        let ok = match first.to_ascii_lowercase().as_str() {
            // MariaDB `SET STATEMENT … FOR <any statement>` runs the statement after FOR.
            "set" => !w.iter().any(|(_, s)| is(s, &["password", "global", "persist", "persist_only", "statement"])),
            "start" => is(second, &["transaction"]),
            "reset" | "begin" | "commit" | "rollback" | "end" | "abort" | "savepoint" | "release" | "use" | "listen" | "unlisten" => true,
            _ => false,
        };
        (!ok).then(|| {
            let f = first.to_ascii_uppercase();
            if matches!(f.as_str(), "SELECT" | "WITH" | "TABLE" | "VALUES" | "EXPLAIN") {
                format!("Read-only connection: {f} that writes or takes locks (INTO, FOR UPDATE, data-modifying) is not allowed.")
            } else {
                format!("Read-only connection: {f} is not allowed.")
            }
        })
    })
}

/// Why an agent (MCP) must not run `text` on a `prod` connection (`None`: allowed). Only reads
/// (`Query` / `Read`) and `UPDATE` with a top-level `WHERE` pass; DELETE, INSERT, DROP, TRUNCATE,
/// ALTER, `WITH … UPDATE`, session statements and everything else are refused.
// ponytail: token check like `read_only_violation`; `WHERE 1=1` passes, a parser couldn't stop that either.
pub fn prod_agent_violation(text: &str, dialect: Dialect) -> Option<String> {
    if dialect == Dialect::Elastic {
        return es::requests(text).iter().find_map(es::prod_agent_violation);
    }
    if dialect == Dialect::Mongo {
        return mongo::prod_agent_violation(text);
    }
    if let Some(m) = executable_comment(text, dialect) {
        return Some(format!("Production connection: {m}"));
    }
    split_words(text, dialect, true).1.iter().find_map(|w| {
        let &(_, first) = w.first()?;
        if classify(w) != Kind::Other {
            return None;
        }
        if is(first, &["update"]) {
            let has_where = w.iter().any(|&(d, s)| d == 0 && is(s, &["where"]));
            return (!has_where).then(|| "Production connection: UPDATE without WHERE is not allowed.".to_string());
        }
        Some(format!("Production connection: {} is not allowed; only reads and UPDATE … WHERE.", first.to_ascii_uppercase()))
    })
}

/// MySQL / MariaDB run the text of `/*! … */` and `/*M! … */` as code, while the lexer here skips
/// it as a comment: the safety checks would not see a statement hidden there.
// ponytail: also catches `/*!` inside a string literal (a false refusal, never a miss).
fn executable_comment(text: &str, dialect: Dialect) -> Option<String> {
    (dialect == Dialect::MySql && (text.contains("/*!") || text.contains("/*M!") || text.contains("/*m!")))
        .then(|| "executable comments (/*! … */) are not allowed: the checks can't see the code inside.".into())
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
    if !dialect.is_sql() {
        return TopLevel::default();
    }
    let (_, all) = split_words(text, dialect, true);
    let top: Vec<&str> = all.first().map(|w| w.iter().filter(|(d, _)| *d == 0).map(|(_, w)| *w).collect()).unwrap_or_default();
    TopLevel {
        limit: top.iter().any(|w| is(w, &["limit", "offset", "fetch"])),
        order_by: top.windows(2).any(|p| is(p[0], &["order"]) && is(p[1], &["by"])),
        lock: top.iter().any(|w| is(w, &["lock", "for"])),
        values: top.first().is_some_and(|w| is(w, &["values"])),
    }
}

/// Byte range in `text` (one statement) of the select list of a plain `SELECT`: after `SELECT`
/// (and `DISTINCT` / `ALL`) up to the top-level clause that ends it (`FROM`, `WHERE`, …) or the
/// end, without surrounding whitespace (empty when the list is). `None` for any other statement.
pub fn select_list(text: &str, dialect: Dialect) -> Option<Range<usize>> {
    if !dialect.is_sql() {
        return None;
    }
    let (_, all) = split_words(text, dialect, true);
    // Words are slices of `text`: their offsets come from the pointers.
    let off = |w: &str| w.as_ptr() as usize - text.as_ptr() as usize;
    let top: Vec<&str> = all.first()?.iter().filter(|(d, _)| *d == 0).map(|(_, w)| *w).collect();
    if !is(top.first()?, &["select"]) {
        return None;
    }
    let lead = 1 + usize::from(top.get(1).is_some_and(|w| is(w, &["distinct", "all"])));
    let start = off(top[lead - 1]) + top[lead - 1].len();
    let ends = ["from", "into", "where", "group", "having", "window", "order", "limit", "offset", "fetch", "for", "union", "intersect", "except", "lock"];
    let end = top[lead..].iter().find(|w| is(w, &ends)).map_or(text.len(), |w| off(w));
    let list = &text[start..end];
    let stop = start + list.trim_end().len();
    Some((start + list.len() - list.trim_start().len()).min(stop)..stop)
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

    #[test]
    fn read_only_violations() {
        for (sql, d, want) in [
            ("SELECT 1", PG, true),
            ("WITH a AS (SELECT 1) SELECT * FROM a", PG, true),
            ("TABLE t", PG, true),
            ("VALUES (1)", PG, true),
            ("SHOW tables", MY, true),
            ("DESCRIBE t", MY, true),
            ("EXPLAIN UPDATE t SET a = 1", PG, true),
            ("EXPLAIN ANALYZE SELECT 1", PG, true),
            ("SET search_path = a", PG, true),
            ("SET NAMES utf8mb4", MY, true),
            ("RESET search_path", PG, true),
            ("BEGIN", PG, true),
            ("START TRANSACTION READ ONLY", MY, true),
            ("COMMIT; ROLLBACK; SAVEPOINT a; RELEASE a", PG, true),
            ("USE shop", MY, true),
            ("LISTEN ch", PG, true),
            ("-- c\nSELECT 'delete'; SELECT 2", PG, true),
            ("", PG, true),
            ("SELECT * INTO t2 FROM t", PG, false),
            ("SELECT * FROM t FOR UPDATE", PG, false),
            ("WITH d AS (DELETE FROM t RETURNING *) SELECT * FROM d", PG, false),
            ("EXPLAIN ANALYZE UPDATE t SET a = 1", PG, false),
            ("INSERT INTO t VALUES (1)", PG, false),
            ("UPDATE t SET a = 1", MY, false),
            ("DELETE FROM t", PG, false),
            ("REPLACE INTO t VALUES (1)", MY, false),
            ("MERGE INTO t USING s ON true WHEN MATCHED THEN DELETE", PG, false),
            ("DROP TABLE t", PG, false),
            ("CREATE TABLE t (a int)", PG, false),
            ("ALTER TABLE t ADD c int", PG, false),
            ("TRUNCATE t", PG, false),
            ("RENAME TABLE a TO b", MY, false),
            ("GRANT ALL ON t TO u", PG, false),
            ("COMMENT ON TABLE t IS 'x'", PG, false),
            ("CALL p()", MY, false),
            ("DO $$ begin end $$", PG, false),
            ("COPY t FROM '/x'", PG, false),
            ("LOAD DATA INFILE 'x' INTO TABLE t", MY, false),
            ("LOCK TABLES t WRITE", MY, false),
            ("VACUUM", PG, false),
            ("ANALYZE t", PG, false),
            ("REFRESH MATERIALIZED VIEW v", PG, false),
            ("PREPARE p AS DELETE FROM t", PG, false),
            ("EXECUTE p", PG, false),
            ("DISCARD ALL", PG, false),
            ("SET PASSWORD = 'x'", MY, false),
            ("SET GLOBAL max_connections = 1", MY, false),
            ("START REPLICA", MY, false),
            ("SELECT 1; UPDATE t SET a = 1", PG, false),
            ("FROBNICATE", PG, false),
        ] {
            assert_eq!(read_only_violation(sql, d).is_none(), want, "{sql}");
        }
        assert_eq!(read_only_violation("update t set a = 1", PG).unwrap(), "Read-only connection: UPDATE is not allowed.");
        assert!(read_only_violation("SET STATEMENT max_statement_time = 1 FOR DELETE FROM t", MY).is_some());
        assert!(read_only_violation("SET statement_timeout = 1000", PG).is_none());
        // MySQL runs executable comments; the lexer skips them, so the checks refuse them.
        for sql in ["SELECT 1 /*! ; DELETE FROM t */", "SHOW TABLES /*!50000 ; DROP TABLE t */", "SELECT 1 /*M! ; DELETE FROM t */"] {
            assert!(read_only_violation(sql, MY).unwrap().contains("executable comments"), "{sql}");
            assert!(prod_agent_violation(sql, MY).unwrap().contains("executable comments"), "{sql}");
        }
        assert!(read_only_violation("SELECT 1 /* plain */", MY).is_none());
        assert!(read_only_violation("SELECT 1 /*! not mysql */", PG).is_none());
    }

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
    fn prod_confirm_only_for_non_reads() {
        assert!(!Kind::Query.needs_prod_confirm() && !Kind::Read.needs_prod_confirm());
        assert!(Kind::Other.needs_prod_confirm());
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
    fn database_directive_parses_and_rewrites() {
        let f = split("-- @connection c\n-- @database app\nSELECT 1;\n-- @database late\nSELECT 2;", PG);
        assert_eq!(f.database.as_deref(), Some("app"));
        assert_eq!(f.connection.as_deref(), Some("c"));
        assert!(f.diagnostics.iter().any(|d| d.message.contains("`@database` applies only at the top")));
        let r = |src: &str, n| rewrite_database(src, PG, n);
        assert_eq!(r("-- @database a\nSELECT 1;", Some("b")).unwrap(), "-- @database b\nSELECT 1;");
        assert_eq!(r("--  @database  a \r\nSELECT 1;\r\n", Some("b")).unwrap(), "--  @database  b \r\nSELECT 1;\r\n");
        assert_eq!(r("-- @connection c\r\n-- @database a\r\nSELECT 1;", None).unwrap(), "-- @connection c\r\nSELECT 1;");
        assert_eq!(r("SELECT 1;\n-- @database a\nSELECT 2;", Some("b")), None);
        assert_eq!(r("-- @connection c\nSELECT 1;", Some("b")), None);
        assert_eq!(split("-- @database\nSELECT 1;", PG).database, None);
    }

    #[test]
    fn rewrite_connection_keeps_the_rest_of_the_line() {
        let r = |src: &str, n| rewrite_connection(src, PG, n);
        assert_eq!(r("-- @connection dev\nSELECT 1;", Some("prod")).unwrap(), "-- @connection prod\nSELECT 1;");
        assert_eq!(r("--   @connection   dev  \r\nSELECT 1;\r\n", Some("a-b")).unwrap(), "--   @connection   a-b  \r\nSELECT 1;\r\n");
        assert_eq!(r("-- hi\n\n  -- @connection dev\nSELECT 1;", Some("x")).unwrap(), "-- hi\n\n  -- @connection x\nSELECT 1;");
        assert_eq!(r("-- hi\n-- @connection dev\r\n-- @name n\nSELECT 1;", None).unwrap(), "-- hi\n-- @name n\nSELECT 1;");
        assert_eq!(r("-- @connection dev", None).unwrap(), "");
        // Not a top-of-file directive (after a statement): nothing to rewrite.
        assert_eq!(r("SELECT 1;\n-- @connection dev\nSELECT 2;", Some("x")), None);
        assert_eq!(r("SELECT 1;", None), None);
    }

    #[test]
    fn set_connection_and_database_write_the_directives() {
        let c = |src: &str, n| set_connection(src, PG, n);
        // Inserted first, replaced, removed; the same name changes nothing.
        assert_eq!(c("SELECT 1;\n", Some("dev")), "-- @connection dev\nSELECT 1;\n");
        assert_eq!(c("", Some("dev")), "-- @connection dev\n");
        assert_eq!(c("-- @connection dev\nSELECT 1;", Some("prod")), "-- @connection prod\nSELECT 1;");
        assert_eq!(c("-- @connection dev\nSELECT 1;", None), "SELECT 1;");
        assert_eq!(c("SELECT 1;", None), "SELECT 1;");
        // Another connection drops the database line (it belonged to the old server); the same one keeps it.
        assert_eq!(c("-- @connection dev\n-- @database app\nSELECT 1;", Some("prod")), "-- @connection prod\nSELECT 1;");
        assert_eq!(c("-- @connection dev\n-- @database app\nSELECT 1;", Some("dev")), "-- @connection dev\n-- @database app\nSELECT 1;");
        assert_eq!(c("-- @database app\nSELECT 1;", Some("dev")), "-- @connection dev\nSELECT 1;");

        let d = |src: &str, n| set_database(src, PG, n);
        // Right after `-- @connection` (CRLF kept), else first; replaced; removed.
        assert_eq!(d("-- @connection dev\nSELECT 1;", Some("app")), "-- @connection dev\n-- @database app\nSELECT 1;");
        assert_eq!(d("-- @name n\n-- @connection dev\r\nSELECT 1;", Some("app")), "-- @name n\n-- @connection dev\r\n-- @database app\nSELECT 1;");
        assert_eq!(d("-- @connection dev", Some("app")), "-- @connection dev\n-- @database app\n");
        assert_eq!(d("SELECT 1;", Some("app")), "-- @database app\nSELECT 1;");
        assert_eq!(d("-- @connection dev\n-- @database a\nSELECT 1;", Some("b")), "-- @connection dev\n-- @database b\nSELECT 1;");
        assert_eq!(d("-- @connection dev\n-- @database a\nSELECT 1;", None), "-- @connection dev\nSELECT 1;");
        assert_eq!(d("SELECT 1;", None), "SELECT 1;");
        // What the splitter then reads back.
        let f = split(&d(&c("SELECT 1;", Some("dev")), Some("app")), PG);
        assert_eq!((f.connection.as_deref(), f.database.as_deref(), f.statements.len()), (Some("dev"), Some("app"), 1));
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
    fn select_lists() {
        fn list(s: &str) -> Option<&str> {
            select_list(s, PG).map(|r| &s[r])
        }
        assert_eq!(list("SELECT * FROM t"), Some("*"));
        assert_eq!(list("select distinct a, b\n  from t where x"), Some("a, b"));
        // A FROM inside parentheses or a string is not the end.
        assert_eq!(list("SELECT (SELECT max(x) FROM u) AS m, 'from' FROM t"), Some("(SELECT max(x) FROM u) AS m, 'from'"));
        assert_eq!(list("SELECT 1"), Some("1"));
        assert_eq!(list("SELECT "), Some(""));
        assert_eq!(list("SELECT a ORDER BY a"), Some("a"));
        assert_eq!(list("UPDATE t SET a = 1"), None);
        assert_eq!(list("WITH x AS (SELECT 1) SELECT * FROM x"), None);
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

    #[test]
    fn prod_agent_allows_reads_and_update_where_only() {
        for (sql, d, ok) in [
            ("SELECT * FROM t", PG, true),
            ("EXPLAIN SELECT 1", PG, true),
            ("SHOW TABLES", MY, true),
            ("UPDATE t SET a = 1 WHERE id = 1", PG, true),
            ("update t set a = 'where' where id = 1 returning *", PG, true),
            ("UPDATE t SET a = 1", PG, false),
            ("UPDATE t SET a = (SELECT 1 FROM u WHERE u.id = 1)", PG, false), // the WHERE is a subquery's
            ("UPDATE t SET a = 1 -- WHERE id = 1", PG, false),
            ("DELETE FROM t WHERE id = 1", PG, false),
            ("INSERT INTO t VALUES (1)", PG, false),
            ("DROP TABLE t", PG, false),
            ("TRUNCATE t", MY, false),
            ("ALTER TABLE t ADD c int", PG, false),
            ("WITH x AS (SELECT 1) UPDATE t SET a = 1 WHERE id = 1", PG, false),
            ("EXPLAIN ANALYZE UPDATE t SET a = 1", PG, false),
            ("SELECT 1 INTO x", PG, false),
            ("SET search_path = x", PG, false),
            ("SELECT 1; DROP TABLE t", PG, false),
            // The lexer reads as the server does: a WHERE inside `$é$…$é$` is text, and `--` ends at `\r`.
            ("UPDATE t SET a = $é$ WHERE id = 1 $é$", PG, false),
            ("UPDATE t SET a = 1 --\r, b = 1; DELETE FROM t; SELECT 1\n WHERE true", PG, false),
            ("UPDATE t SET a = 1 -- note\n WHERE id = 1", PG, true),
        ] {
            assert_eq!(prod_agent_violation(sql, d).is_none(), ok, "{sql}");
        }
        assert!(prod_agent_violation("DROP TABLE t", PG).unwrap().contains("DROP"));
    }

    #[test]
    fn schema_changing() {
        for (sql, d, want) in [
            ("CREATE TABLE t (a int)", PG, true),
            ("  alter table t add c int", MY, true),
            ("DROP INDEX i", PG, true),
            ("RENAME TABLE a TO b", MY, true),
            ("TRUNCATE t", PG, true),
            ("COMMENT ON TABLE t IS 'x'", PG, true),
            ("-- c\nSELECT 1; CREATE VIEW v AS SELECT 1", PG, true),
            ("SELECT 'create table x'", PG, false),
            ("SELECT 1 -- drop table t", PG, false),
            ("INSERT INTO t VALUES (1)", PG, false),
            ("COMMENT 'x'", PG, false),
            ("EXPLAIN DROP TABLE t", PG, false),
            ("EXPLAIN ANALYZE CREATE TABLE a AS SELECT 1", PG, true),
            ("", PG, false),
        ] {
            assert_eq!(is_schema_changing(sql, d), want, "{sql}");
        }
    }
}
