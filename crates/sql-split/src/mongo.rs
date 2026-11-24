//! MongoDB `.mongodb.js` files in mongosh syntax: `db.<collection>.<method>(…)` calls with cursor
//! methods after them (`.sort({…}).limit(5)`), `db.<method>(…)` and `show dbs | collections | users`.
//! Arguments are JavaScript literals: objects with bare keys, `'…'` strings, regexes,
//! `ObjectId("…")`, `ISODate("…")`, `NumberLong(…)` and the like. They parse into extended JSON,
//! which the executor turns into BSON. Variables, functions and loops are not run: a statement is
//! one call.
//!
//! A statement ends at `;`, or at a line break outside brackets unless the next line starts with
//! `.` (a chained method). `//` and `/* */` are comments; `// @word value` is a directive.
//!
//! The same parse ([`parse`]) serves the editor, the safety checks and the executor, so they agree
//! on what a statement does.

use serde_json::{Map, Value as J, json};

use crate::{Diagnostic, Kind, Severity, SqlFile, Statement};

/// One statement: `db.<collection>.<method>(<args>).<chain>…`, `db.<method>(<args>)`, or
/// `show <what>` (method `show`, the word as the one argument).
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    /// `None`: a database method or `show`.
    pub collection: Option<String>,
    pub method: String,
    pub args: Vec<J>,
    /// Cursor methods after the call, in order: `("sort", [{…}])`, `("limit", [5])`.
    pub chain: Vec<(String, Vec<J>)>,
}

impl Call {
    /// `db.users.find(…)`, `db.runCommand(…)`, `show dbs`: for messages.
    pub fn label(&self) -> String {
        match &self.collection {
            Some(c) => format!("db.{c}.{}(…)", self.method),
            None if self.method == "show" => format!("show {}", self.args.first().and_then(J::as_str).unwrap_or("")),
            None => format!("db.{}(…)", self.method),
        }
    }

    /// The command name of `db.runCommand(…)` / `db.adminCommand(…)`: the first key, or the string.
    pub fn command(&self) -> Option<&str> {
        if !matches!(self.method.as_str(), "runCommand" | "adminCommand") || self.collection.is_some() {
            return None;
        }
        match self.args.first()? {
            J::String(s) => Some(s),
            J::Object(m) => m.keys().next().map(String::as_str),
            _ => None,
        }
    }

    /// Arguments of the chained cursor method `m` (the last one wins).
    pub fn chained(&self, m: &str) -> Option<&[J]> {
        self.chain.iter().rev().find(|(n, _)| n == m).map(|(_, a)| a.as_slice())
    }
}

const COLLECTION_METHODS: &[&str] = &[
    "find", "findOne", "aggregate", "countDocuments", "estimatedDocumentCount", "count", "distinct", "getIndexes", "stats",
    "insertOne", "insertMany", "updateOne", "updateMany", "replaceOne", "deleteOne", "deleteMany",
    "findOneAndUpdate", "findOneAndReplace", "findOneAndDelete", "createIndex", "dropIndex", "dropIndexes", "drop", "renameCollection",
];
const DB_METHODS: &[&str] = &["runCommand", "adminCommand", "getCollectionNames", "getCollectionInfos", "createCollection", "dropDatabase", "stats", "version", "serverStatus"];
const SHOW: &[&str] = &["dbs", "databases", "collections", "tables", "users"];
/// Cursor methods `find` takes; `aggregate` takes the last three (and `pretty`).
const FIND_CHAIN: &[&str] = &["sort", "limit", "skip", "projection", "hint", "collation", "maxTimeMS", "comment", "batchSize", "allowDiskUse", "count", "toArray", "explain"];
/// `db.runCommand` commands that only read (with no `$out` / `$merge` stage anywhere).
const READ_COMMANDS: &[&str] = &[
    "ping", "buildInfo", "buildinfo", "serverStatus", "dbStats", "collStats", "listCollections", "listIndexes", "listDatabases",
    "hostInfo", "connectionStatus", "hello", "isMaster", "ismaster", "count", "distinct", "find", "aggregate", "getParameter", "usersInfo", "rolesInfo",
];

/// Parses one statement (the text `Statement::text` gives for a Mongo file). `Err`: byte offset
/// in `text` and a message.
pub fn parse(text: &str) -> Result<Call, (usize, String)> {
    let mut p = P { s: text, b: text.as_bytes(), i: 0 };
    let call = p.statement()?;
    p.eat(b';');
    match p.peek() {
        None => Ok(call),
        Some(_) => Err(p.err("Unexpected text after the call: one call per statement")),
    }
}

/// The kind of a parsed statement, as [`crate::split`] gives it.
pub fn kind(c: &Call) -> Kind {
    if writes_collection(c) {
        return Kind::Other;
    }
    let plain = !c.chain.iter().any(|(m, _)| m == "explain" || m == "count");
    match (c.collection.is_some(), c.method.as_str()) {
        (true, "find" | "aggregate") if plain => Kind::Query,
        (true, "find" | "aggregate" | "findOne" | "countDocuments" | "estimatedDocumentCount" | "count" | "distinct" | "getIndexes" | "stats") => Kind::Read,
        (false, "show" | "getCollectionNames" | "getCollectionInfos" | "stats" | "version" | "serverStatus") => Kind::Read,
        (false, "runCommand" | "adminCommand") if c.command().is_some_and(|n| READ_COMMANDS.contains(&n)) => Kind::Read,
        _ => Kind::Other,
    }
}

/// An `$out` / `$merge` stage anywhere in the arguments: the aggregation writes a collection.
// ponytail: any key of that name counts, also inside a literal; a false refusal, never a miss.
fn writes_collection(c: &Call) -> bool {
    fn has(v: &J) -> bool {
        match v {
            J::Object(m) => m.iter().any(|(k, v)| k == "$out" || k == "$merge" || has(v)),
            J::Array(a) => a.iter().any(has),
            _ => false,
        }
    }
    c.args.iter().chain(c.chain.iter().flat_map(|(_, a)| a)).any(has)
}

fn empty_filter(v: Option<&J>) -> bool {
    v.is_none_or(|v| v.as_object().is_some_and(Map::is_empty))
}

/// Why a statement needs a confirmation where destructive statements do: dropping a collection,
/// database or index, `deleteMany` / `updateMany` without a filter.
pub(crate) fn reason(c: &Call) -> Option<&'static str> {
    match (c.method.as_str(), c.command()) {
        ("drop", _) | (_, Some("drop")) => Some("drop"),
        ("dropDatabase", _) | (_, Some("dropDatabase")) => Some("dropDatabase"),
        ("dropIndex" | "dropIndexes", _) | (_, Some("dropIndexes")) => Some("dropIndex"),
        ("deleteMany", _) if empty_filter(c.args.first()) => Some("deleteMany without filter"),
        ("updateMany", _) if empty_filter(c.args.first()) => Some("updateMany without filter"),
        _ => None,
    }
}

/// Collections or indexes may have changed: the cached schema is stale.
pub(crate) fn schema_changing(c: &Call) -> bool {
    let cmd = ["create", "drop", "dropDatabase", "renameCollection", "createIndexes", "dropIndexes", "collMod"];
    matches!(c.method.as_str(), "createCollection" | "drop" | "dropDatabase" | "renameCollection" | "createIndex" | "dropIndex" | "dropIndexes")
        || c.command().is_some_and(|n| cmd.contains(&n))
        || writes_collection(c)
}

/// Each statement of `text` parsed, with its text for messages.
fn calls(text: &str) -> Vec<Result<Call, String>> {
    let file = split(text);
    file.statements.iter().map(|s| parse(s.text(text)).map_err(|(_, m)| format!("`{}` does not parse: {m}", s.label(text)))).collect()
}

pub(crate) fn read_only_violation(text: &str) -> Option<String> {
    calls(text).into_iter().find_map(|c| match c {
        Err(e) => Some(format!("Read-only connection: {e}")),
        Ok(c) => (kind(&c) == Kind::Other).then(|| format!("Read-only connection: {} is not allowed.", c.label())),
    })
}

/// An agent on `prod`: reads, and `updateOne` / `updateMany` with a filter and update operators
/// (the counterpart of SQL's `UPDATE … WHERE`); no upsert, no replacement, no pipeline.
pub(crate) fn prod_agent_violation(text: &str) -> Option<String> {
    calls(text).into_iter().find_map(|c| {
        let c = match c {
            Err(e) => return Some(format!("Production connection: {e}")),
            Ok(c) if kind(&c) != Kind::Other => return None,
            Ok(c) => c,
        };
        let label = c.label();
        if c.collection.is_none() || !matches!(c.method.as_str(), "updateOne" | "updateMany") {
            return Some(format!("Production connection: {label} is not allowed; only reads and updateOne / updateMany with a filter."));
        }
        if empty_filter(c.args.first()) || !c.args[0].is_object() {
            return Some(format!("Production connection: {label} without a filter is not allowed."));
        }
        let operators = c.args.get(1).and_then(J::as_object).is_some_and(|m| !m.is_empty() && m.keys().all(|k| k.starts_with('$')));
        let options = c.args.get(2).is_none_or(|o| o.as_object().is_some_and(|m| m.keys().all(|k| ["arrayFilters", "collation", "hint", "comment"].contains(&k.as_str()))));
        let ok = operators && options && c.args.len() <= 3 && c.chain.is_empty();
        (!ok).then(|| format!("Production connection: {label} only with update operators ({{$set: …}}); no upsert, replacement or pipeline."))
    })
}

pub(crate) fn split(src: &str) -> SqlFile {
    let b = src.as_bytes();
    let mut out = SqlFile::default();
    let mut pending = crate::Pending::default();
    let mut start: Option<usize> = None;
    let (mut last, mut depth, mut i) = (0usize, 0u32, 0usize);
    // (offset, line) of the last line lookup.
    let mut lines = (0usize, 1usize);
    let mut finish = |out: &mut SqlFile, start: &mut Option<usize>, last: usize, pending: &mut crate::Pending| {
        let p = std::mem::take(pending);
        let Some(s) = start.take() else { return };
        lines = (s, lines.1 + b[lines.0..s].iter().filter(|&&c| c == b'\n').count());
        let text = &src[s..last];
        let kind = match parse(text) {
            Ok(c) => {
                if let Some(r @ ("deleteMany without filter" | "updateMany without filter")) = reason(&c) {
                    let what = if r.starts_with("delete") { "every document deletes" } else { "every document changes" };
                    out.diagnostics.push(Diagnostic { range: s..last, severity: Severity::Warning, message: format!("{r}: {what}") });
                }
                kind(&c)
            }
            Err((at, message)) => {
                let at = (s + at).min(last.saturating_sub(1)).max(s);
                out.diagnostics.push(Diagnostic { range: at..last.max(at + 1).min(src.len()), severity: Severity::Error, message });
                Kind::Other
            }
        };
        out.statements.push(Statement { range: s..last, line: lines.1, name: p.name, page_size: p.page_size, confirm: p.confirm, kind });
    };
    while i < b.len() {
        let c = b[i];
        if c == b'\n' && start.is_some() && depth == 0 && b[last - 1] != b'.' && next_significant(b, i) != Some(b'.') {
            finish(&mut out, &mut start, last, &mut pending);
            i += 1;
            continue;
        }
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if b[i..].starts_with(b"//") {
            let end = b[i..].iter().position(|&c| c == b'\n').map_or(b.len(), |p| i + p);
            if start.is_none()
                && let Some(rest) = src[i + 2..end].trim().strip_prefix('@')
            {
                let top = out.statements.is_empty();
                crate::directive_word(&mut out, &mut pending, rest, i..end, top);
            }
            i = end;
            continue;
        }
        if b[i..].starts_with(b"/*") {
            match src[i + 2..].find("*/") {
                Some(p) => i += p + 4,
                None => {
                    out.diagnostics.push(Diagnostic { range: i..b.len(), severity: Severity::Error, message: "Unclosed /* comment".into() });
                    i = b.len();
                }
            }
            continue;
        }
        if c == b';' && depth == 0 {
            finish(&mut out, &mut start, last, &mut pending);
            i += 1;
            continue;
        }
        start.get_or_insert(i);
        i = match c {
            // Strings and regexes only skip here; `parse` reports them unclosed.
            b'"' | b'\'' | b'`' => skip_quoted(b, i, c),
            b'/' => skip_quoted(b, i, b'/'),
            b'(' | b'[' | b'{' => {
                depth += 1;
                i + 1
            }
            b')' | b']' | b'}' => {
                depth = depth.saturating_sub(1);
                i + 1
            }
            _ => i + 1,
        };
        last = i;
    }
    finish(&mut out, &mut start, last, &mut pending);
    out
}

/// The offset after a quoted token (string or regex) at `i`; at a line break when it is unclosed
/// (only template strings span lines).
fn skip_quoted(b: &[u8], i: usize, q: u8) -> usize {
    let (mut j, mut class) = (i + 1, false);
    while let Some(&c) = b.get(j) {
        match c {
            b'\\' => j += 1,
            b'\n' if q != b'`' => return j,
            b'[' if q == b'/' => class = true,
            b']' if q == b'/' => class = false,
            _ if c == q && !class => return j + 1,
            _ => {}
        }
        j += 1;
    }
    b.len()
}

/// The first byte after `i` that is not whitespace or a comment.
fn next_significant(b: &[u8], mut i: usize) -> Option<u8> {
    loop {
        while b.get(i).is_some_and(u8::is_ascii_whitespace) {
            i += 1;
        }
        if b[i..].starts_with(b"//") {
            i += b[i..].iter().position(|&c| c == b'\n').unwrap_or(b.len() - i);
        } else if b[i..].starts_with(b"/*") {
            i += b[i + 2..].windows(2).position(|w| w == b"*/").map_or(b.len() - i, |p| p + 4);
        } else {
            return b.get(i).copied();
        }
    }
}

type R<T> = Result<T, (usize, String)>;

/// Recursive descent over one statement.
struct P<'a> {
    s: &'a str,
    b: &'a [u8],
    i: usize,
}

impl<'a> P<'a> {
    fn ws(&mut self) {
        loop {
            while self.b.get(self.i).is_some_and(u8::is_ascii_whitespace) {
                self.i += 1;
            }
            let rest = &self.b[self.i..];
            if rest.starts_with(b"//") {
                self.i += rest.iter().position(|&c| c == b'\n').unwrap_or(rest.len());
            } else if rest.starts_with(b"/*") {
                self.i += rest[2..].windows(2).position(|w| w == b"*/").map_or(rest.len(), |p| p + 4);
            } else {
                return;
            }
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.ws();
        self.b.get(self.i).copied()
    }

    fn eat(&mut self, c: u8) -> bool {
        let hit = self.peek() == Some(c);
        self.i += usize::from(hit);
        hit
    }

    fn err(&self, m: impl Into<String>) -> (usize, String) {
        (self.i, m.into())
    }

    fn expect(&mut self, c: u8) -> R<()> {
        if self.eat(c) { Ok(()) } else { Err(self.err(format!("Expected {}", c as char))) }
    }

    fn ident(&mut self) -> Option<&'a str> {
        self.ws();
        let start = self.i;
        while self.b.get(self.i).is_some_and(|&c| c.is_ascii_alphabetic() || c == b'_' || c == b'$' || c >= 0x80 || (self.i > start && c.is_ascii_digit())) {
            self.i += 1;
        }
        (self.i > start).then(|| &self.s[start..self.i])
    }

    fn statement(&mut self) -> R<Call> {
        self.ws();
        let at = self.i;
        match self.ident() {
            Some("db") => {}
            Some("show") => {
                let what = self.ident().filter(|w| SHOW.contains(w)).ok_or_else(|| self.err("Expected show dbs | collections | users"))?;
                return Ok(Call { collection: None, method: "show".into(), args: vec![J::String(what.into())], chain: vec![] });
            }
            Some("use") => return Err((at, "Pick the database with `// @database <name>` or the database picker, not `use`".into())),
            _ => return Err((at, "Expected db.<collection>.<method>(…), db.<method>(…) or show …; variables, functions and loops are not run".into())),
        }
        // `db.a.b.method(`, `db["a"].method(`
        let mut path: Vec<String> = Vec::new();
        while self.peek() != Some(b'(') {
            if self.eat(b'.') {
                path.push(self.ident().ok_or_else(|| self.err("Expected a name after ."))?.to_string());
            } else if self.eat(b'[') {
                let J::String(s) = self.value()? else { return Err(self.err("Expected a collection name in quotes")) };
                self.expect(b']')?;
                path.push(s);
            } else {
                return Err(self.err("Expected .<method>(…)"));
            }
        }
        let at = self.i;
        let Some(mut method) = path.pop() else { return Err(self.err("Expected db.<collection>.<method>(…)")) };
        let mut args = self.args()?;
        let mut collection = (!path.is_empty()).then(|| path.join("."));
        if collection.is_none() && method == "getCollection" {
            let Some(J::String(name)) = args.first().cloned() else { return Err((at, "Expected getCollection(\"<name>\")".into())) };
            self.expect(b'.')?;
            method = self.ident().ok_or_else(|| self.err("Expected a method"))?.to_string();
            args = self.args()?;
            collection = Some(name);
        }
        let known = if collection.is_some() { COLLECTION_METHODS } else { DB_METHODS };
        if !known.contains(&method.as_str()) {
            let what = if collection.is_some() { "collection" } else { "database" };
            return Err((at, format!("Unknown {what} method {method}; known: {}", known.join(", "))));
        }
        let mut chain = Vec::new();
        while self.peek() == Some(b'.') {
            self.i += 1;
            let at = self.i;
            let m = self.ident().ok_or_else(|| self.err("Expected a method after ."))?.to_string();
            let allowed = match method.as_str() {
                "find" => FIND_CHAIN,
                "aggregate" => &FIND_CHAIN[FIND_CHAIN.len() - 2..],
                _ => &[],
            };
            if m != "pretty" && !allowed.contains(&m.as_str()) {
                return Err((at, format!("{method}(…) has no .{m}()")));
            }
            let a = self.args()?;
            if m != "pretty" {
                chain.push((m, a));
            }
        }
        Ok(Call { collection, method, args, chain })
    }

    fn args(&mut self) -> R<Vec<J>> {
        self.expect(b'(')?;
        let mut out = Vec::new();
        while !self.eat(b')') {
            out.push(self.value()?);
            if !self.eat(b',') {
                self.expect(b')')?;
                break;
            }
        }
        Ok(out)
    }

    fn value(&mut self) -> R<J> {
        match self.peek() {
            None => Err(self.err("Expected a value")),
            Some(b'{') => self.object(),
            Some(b'[') => {
                self.i += 1;
                let mut v = Vec::new();
                while !self.eat(b']') {
                    v.push(self.value()?);
                    if !self.eat(b',') {
                        self.expect(b']')?;
                        break;
                    }
                }
                Ok(J::Array(v))
            }
            Some(q @ (b'"' | b'\'' | b'`')) => self.string(q).map(J::String),
            Some(b'/') => self.regex(),
            Some(c) if c == b'-' || c == b'+' || c == b'.' || c.is_ascii_digit() => self.number(),
            Some(_) => self.word(),
        }
    }

    fn object(&mut self) -> R<J> {
        self.i += 1;
        let mut m = Map::new();
        while !self.eat(b'}') {
            let key = match self.peek() {
                Some(q @ (b'"' | b'\'' | b'`')) => self.string(q)?,
                Some(c) if c.is_ascii_digit() => self.number()?.to_string(),
                _ => self.ident().ok_or_else(|| self.err("Expected a key"))?.to_string(),
            };
            self.expect(b':')?;
            let v = self.value()?;
            m.insert(key, v);
            if !self.eat(b',') {
                self.expect(b'}')?;
                break;
            }
        }
        Ok(J::Object(m))
    }

    fn next_char(&mut self) -> Option<char> {
        let c = self.s[self.i..].chars().next()?;
        self.i += c.len_utf8();
        Some(c)
    }

    fn hex(&mut self, n: usize) -> Option<u32> {
        let h = self.s.get(self.i..self.i + n)?;
        let v = u32::from_str_radix(h, 16).ok()?;
        self.i += n;
        Some(v)
    }

    /// A JavaScript string with its escapes resolved.
    fn string(&mut self, q: u8) -> R<String> {
        let start = self.i;
        self.i += 1;
        let mut out = String::new();
        let unclosed = || (start, "Unclosed string".to_string());
        loop {
            let c = self.next_char().ok_or_else(unclosed)?;
            match c {
                _ if c == q as char => return Ok(out),
                '\n' if q != b'`' => return Err(unclosed()),
                '$' if q == b'`' && self.b.get(self.i) == Some(&b'{') => return Err((self.i - 1, "${…} in a template string is not supported".into())),
                '\\' => {
                    let e = self.next_char().ok_or_else(unclosed)?;
                    let bad = (self.i, "Invalid escape".to_string());
                    match e {
                        'n' => out.push('\n'),
                        't' => out.push('\t'),
                        'r' => out.push('\r'),
                        'b' => out.push('\u{8}'),
                        'f' => out.push('\u{c}'),
                        'v' => out.push('\u{b}'),
                        '0' => out.push('\0'),
                        '\n' => {}
                        'x' => out.push(self.hex(2).and_then(char::from_u32).ok_or(bad)?),
                        'u' if self.b.get(self.i) == Some(&b'{') => {
                            let close = self.s[self.i..].find('}').ok_or(bad.clone())?;
                            let v = u32::from_str_radix(&self.s[self.i + 1..self.i + close], 16).ok().and_then(char::from_u32).ok_or(bad)?;
                            self.i += close + 1;
                            out.push(v);
                        }
                        'u' => {
                            let hi = self.hex(4).ok_or(bad.clone())?;
                            // A surrogate pair: `\uD83D\uDE00`.
                            let v = if (0xD800..0xDC00).contains(&hi) && self.s[self.i..].starts_with("\\u") {
                                self.i += 2;
                                let lo = self.hex(4).ok_or(bad.clone())?;
                                0x10000 + ((hi - 0xD800) << 10) + (lo.wrapping_sub(0xDC00) & 0x3FF)
                            } else {
                                hi
                            };
                            out.push(char::from_u32(v).ok_or(bad)?);
                        }
                        other => out.push(other),
                    }
                }
                c => out.push(c),
            }
        }
    }

    /// `/pattern/flags` as extended JSON; flags BSON doesn't know (`g`, `y`) are dropped.
    fn regex(&mut self) -> R<J> {
        let start = self.i;
        let end = skip_quoted(self.b, start, b'/');
        if self.b.get(end - 1) != Some(&b'/') || end == start + 1 {
            return Err((start, "Unclosed regex".into()));
        }
        let pattern = &self.s[start + 1..end - 1];
        self.i = end;
        let f0 = self.i;
        while self.b.get(self.i).is_some_and(u8::is_ascii_alphabetic) {
            self.i += 1;
        }
        let mut flags: Vec<char> = self.s[f0..self.i].chars().filter(|c| "imsux".contains(*c)).collect();
        flags.sort();
        Ok(json!({ "$regularExpression": { "pattern": pattern, "options": flags.into_iter().collect::<String>() } }))
    }

    fn number(&mut self) -> R<J> {
        let start = self.i;
        if matches!(self.b[self.i], b'+' | b'-') {
            self.i += 1;
        }
        let digits = self.i;
        let hex = self.s[self.i..].to_ascii_lowercase().starts_with("0x");
        if hex {
            self.i += 2;
            while self.b.get(self.i).is_some_and(u8::is_ascii_hexdigit) {
                self.i += 1;
            }
        } else {
            while let Some(&c) = self.b.get(self.i) {
                let sign_after_e = matches!(c, b'+' | b'-') && matches!(self.b[self.i - 1], b'e' | b'E');
                if !(c.is_ascii_digit() || matches!(c, b'.' | b'e' | b'E') || sign_after_e) {
                    break;
                }
                self.i += 1;
            }
        }
        let t = &self.s[start..self.i];
        let neg = t.starts_with('-');
        let bad = || (start, format!("Invalid number {t}"));
        if hex {
            let n = i64::from_str_radix(&self.s[digits + 2..self.i], 16).map_err(|_| bad())?;
            return Ok(J::from(if neg { -n } else { n }));
        }
        let t = t.trim_start_matches('+');
        if !t.contains(['.', 'e', 'E'])
            && let Ok(n) = t.parse::<i64>()
        {
            return Ok(J::from(n));
        }
        t.parse::<f64>().ok().and_then(serde_json::Number::from_f64).map(J::Number).ok_or_else(bad)
    }

    /// `true`, `null`, `ObjectId("…")`, `new Date(…)` and the other mongosh constructors.
    fn word(&mut self) -> R<J> {
        let at = self.i;
        let mut w = self.ident().ok_or_else(|| self.err("Expected a value"))?;
        if w == "new" {
            w = self.ident().ok_or_else(|| self.err("Expected a constructor after new"))?;
        }
        match w {
            "true" => return Ok(J::Bool(true)),
            "false" => return Ok(J::Bool(false)),
            "null" | "undefined" => return Ok(J::Null),
            "MinKey" if self.peek() != Some(b'(') => return Ok(json!({ "$minKey": 1 })),
            "MaxKey" if self.peek() != Some(b'(') => return Ok(json!({ "$maxKey": 1 })),
            _ if self.peek() != Some(b'(') => return Err((at, format!("Unknown name {w}: variables are not supported"))),
            _ => {}
        }
        let args = self.args()?;
        let text = |i: usize| match args.get(i) {
            Some(J::String(s)) => Some(s.clone()),
            Some(J::Number(n)) => Some(n.to_string()),
            _ => None,
        };
        let need = |what: &str| (at, format!("Expected {w}({what})"));
        Ok(match w {
            "ObjectId" | "ObjectID" => json!({ "$oid": text(0).ok_or_else(|| need("\"<24 hex digits>\""))? }),
            "ISODate" | "Date" => match args.first() {
                None => {
                    let ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis());
                    json!({ "$date": { "$numberLong": ms.to_string() } })
                }
                Some(J::Number(n)) => json!({ "$date": { "$numberLong": n.as_i64().unwrap_or(n.as_f64().unwrap_or(0.0) as i64).to_string() } }),
                Some(J::String(s)) => json!({ "$date": iso(s) }),
                _ => return Err(need("\"2024-01-31T10:00:00Z\"")),
            },
            "NumberLong" | "Long" => json!({ "$numberLong": text(0).ok_or_else(|| need("<number>"))? }),
            "NumberInt" | "Int32" => json!({ "$numberInt": text(0).ok_or_else(|| need("<number>"))? }),
            "NumberDecimal" | "Decimal128" => json!({ "$numberDecimal": text(0).ok_or_else(|| need("\"<number>\""))? }),
            "Double" => json!({ "$numberDouble": text(0).ok_or_else(|| need("<number>"))? }),
            "UUID" => json!({ "$uuid": text(0).ok_or_else(|| need("\"<uuid>\""))? }),
            "Timestamp" => match args.as_slice() {
                [J::Number(t), J::Number(i)] => json!({ "$timestamp": { "t": t, "i": i } }),
                [J::Object(o)] => json!({ "$timestamp": o }),
                _ => return Err(need("<seconds>, <increment>")),
            },
            "BinData" => match args.as_slice() {
                [J::Number(sub), J::String(b64)] => json!({ "$binary": { "base64": b64, "subType": format!("{:02x}", sub.as_u64().unwrap_or(0)) } }),
                _ => return Err(need("<subtype>, \"<base64>\"")),
            },
            "MinKey" => json!({ "$minKey": 1 }),
            "MaxKey" => json!({ "$maxKey": 1 }),
            "RegExp" => {
                let mut flags: Vec<char> = text(1).unwrap_or_default().chars().filter(|c| "imsux".contains(*c)).collect();
                flags.sort();
                json!({ "$regularExpression": { "pattern": text(0).ok_or_else(|| need("\"<pattern>\""))?, "options": flags.into_iter().collect::<String>() } })
            }
            _ => return Err((at, format!("Unknown function {w}"))),
        })
    }
}

/// A date string as RFC 3339: `2024-01-31` is midnight UTC, a time without a zone is UTC.
fn iso(s: &str) -> String {
    let t = s.trim().replacen(' ', "T", 1);
    if t.len() == 10 {
        return format!("{t}T00:00:00Z");
    }
    let zoned = t.ends_with(['Z', 'z']) || t.find('T').is_some_and(|i| t[i..].contains(['+', '-']));
    if zoned { t } else { format!("{t}Z") }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Dialect::Mongo as MG, destructive_reason, is_schema_changing, prod_agent_violation, read_only_violation, set_connection, set_database};

    #[test]
    fn splits_statements_by_semicolon_and_line() {
        let src = "// @connection m\n// @database shop\n\n// plain\ndb.users.find({ age: { $gt: 30 } })\n  .sort({ name: 1 })\n  // between\n  .limit(5)\n\n// @name count\ndb.users.countDocuments({}); db.users.insertOne({\n  a: 'x;y',\n  re: /a\\/b;/i,\n})\nshow collections\n";
        let f = split(src);
        assert_eq!((f.connection.as_deref(), f.database.as_deref()), (Some("m"), Some("shop")));
        let got: Vec<(&str, usize, Kind, Option<&str>)> = f.statements.iter().map(|s| (s.text(src), s.line, s.kind, s.name.as_deref())).collect();
        assert_eq!(
            got,
            [
                ("db.users.find({ age: { $gt: 30 } })\n  .sort({ name: 1 })\n  // between\n  .limit(5)", 5, Kind::Query, None),
                ("db.users.countDocuments({})", 11, Kind::Read, Some("count")),
                ("db.users.insertOne({\n  a: 'x;y',\n  re: /a\\/b;/i,\n})", 11, Kind::Other, None),
                ("show collections", 15, Kind::Read, None),
            ]
        );
        assert!(f.diagnostics.is_empty(), "{:?}", f.diagnostics);
        let c = parse(f.statements[0].text(src)).unwrap();
        assert_eq!((c.collection.as_deref(), c.method.as_str()), (Some("users"), "find"));
        assert_eq!(c.args, [json!({"age": {"$gt": 30}})]);
        assert_eq!(c.chain, [("sort".to_string(), vec![json!({"name": 1})]), ("limit".to_string(), vec![json!(5)])]);
        let ins = parse(f.statements[2].text(src)).unwrap();
        assert_eq!(ins.args[0], json!({"a": "x;y", "re": {"$regularExpression": {"pattern": "a\\/b;", "options": "i"}}}));
    }

    #[test]
    fn literals() {
        let c = parse(r#"db['a.b'].insertOne({_id: ObjectId("65a1b2c3d4e5f6a7b8c9d0e1"), at: ISODate("2024-01-31"), t: new Date("2024-01-31 10:00:00"), n: NumberLong(5), d: NumberDecimal("1.10"), u: UUID("3b241101-e2bb-4255-8caf-4136c566a962"), "q k": 'it\'s', h: 0x1F, f: -1.5e3, nul: null, un: undefined, arr: [1, 2,], esc: "\u00e9\uD83D\uDE00", mk: MinKey, ts: Timestamp(1, 2), b: BinData(0, "AQ=="), $set: true})"#).unwrap();
        assert_eq!(c.collection.as_deref(), Some("a.b"));
        assert_eq!(
            c.args[0],
            json!({"_id": {"$oid": "65a1b2c3d4e5f6a7b8c9d0e1"}, "at": {"$date": "2024-01-31T00:00:00Z"}, "t": {"$date": "2024-01-31T10:00:00Z"},
                "n": {"$numberLong": "5"}, "d": {"$numberDecimal": "1.10"}, "u": {"$uuid": "3b241101-e2bb-4255-8caf-4136c566a962"}, "q k": "it's",
                "h": 31, "f": -1500.0, "nul": null, "un": null, "arr": [1, 2], "esc": "é😀", "mk": {"$minKey": 1}, "ts": {"$timestamp": {"t": 1, "i": 2}},
                "b": {"$binary": {"base64": "AQ==", "subType": "00"}}, "$set": true})
        );
        let g = parse("db.getCollection('x y').aggregate([{$match: {}}]).toArray()").unwrap();
        assert_eq!((g.collection.as_deref(), g.method.as_str(), kind(&g)), (Some("x y"), "aggregate", Kind::Query));
        assert_eq!(parse("db.runCommand('ping');").unwrap().command(), Some("ping"));
        assert_eq!(parse("show dbs").unwrap().label(), "show dbs");
    }

    #[test]
    fn errors_are_diagnostics() {
        for (src, msg) in [
            ("let x = 1", "Expected db.<collection>.<method>(…), db.<method>(…) or show …; variables, functions and loops are not run"),
            ("use shop", "Pick the database with `// @database <name>` or the database picker, not `use`"),
            ("db.users.frob()", "Unknown collection method frob; known: "),
            ("db.users.insertOne({a: x})", "Unknown name x: variables are not supported"),
            ("db.users.insertOne({a: 1}).limit(1)", "insertOne(…) has no .limit()"),
            ("db.users.find({a: 'open})", "Unclosed string"),
            ("db.users.find({a: 1}) db.x.find()", "Unexpected text after the call: one call per statement"),
            ("db.users.find({a: `${x}`})", "${…} in a template string is not supported"),
        ] {
            let f = split(src);
            assert_eq!(f.statements[0].kind, Kind::Other, "{src}");
            assert!(f.diagnostics.first().is_some_and(|d| d.severity == Severity::Error && d.message.starts_with(msg)), "{src}: {:?}", f.diagnostics);
        }
        let warn = split("db.a.deleteMany({})\ndb.a.updateMany({}, {$set: {x: 1}})\ndb.a.deleteMany({x: 1})");
        let msgs: Vec<&str> = warn.diagnostics.iter().map(|d| d.message.as_str()).collect();
        assert_eq!(msgs, ["deleteMany without filter: every document deletes", "updateMany without filter: every document changes"]);
    }

    #[test]
    fn kinds_and_safety() {
        for (st, kind, destructive, schema, agent_prod_ok) in [
            ("db.a.find({})", Kind::Query, None, false, true),
            ("db.a.find().sort({x: -1}).limit(3)", Kind::Query, None, false, true),
            ("db.a.aggregate([{$group: {_id: '$x'}}])", Kind::Query, None, false, true),
            ("db.a.find().explain('executionStats')", Kind::Read, None, false, true),
            ("db.a.find({x: 1}).count()", Kind::Read, None, false, true),
            ("db.a.findOne({_id: 1})", Kind::Read, None, false, true),
            ("db.a.countDocuments({})", Kind::Read, None, false, true),
            ("db.a.distinct('x')", Kind::Read, None, false, true),
            ("db.a.getIndexes()", Kind::Read, None, false, true),
            ("show dbs", Kind::Read, None, false, true),
            ("db.getCollectionNames()", Kind::Read, None, false, true),
            ("db.runCommand({ping: 1})", Kind::Read, None, false, true),
            ("db.adminCommand({listDatabases: 1})", Kind::Read, None, false, true),
            ("db.a.updateOne({_id: 1}, {$set: {x: 2}})", Kind::Other, None, false, true),
            ("db.a.updateMany({x: 1}, {$inc: {n: 1}}, {arrayFilters: []})", Kind::Other, None, false, true),
            ("db.a.updateMany({}, {$set: {x: 1}})", Kind::Other, Some("updateMany without filter"), false, false),
            ("db.a.updateOne({_id: 1}, {x: 2})", Kind::Other, None, false, false),
            ("db.a.updateOne({_id: 1}, [{$set: {x: 2}}])", Kind::Other, None, false, false),
            ("db.a.updateOne({_id: 1}, {$set: {x: 2}}, {upsert: true})", Kind::Other, None, false, false),
            ("db.a.insertOne({x: 1})", Kind::Other, None, false, false),
            ("db.a.replaceOne({_id: 1}, {x: 1})", Kind::Other, None, false, false),
            ("db.a.deleteOne({_id: 1})", Kind::Other, None, false, false),
            ("db.a.deleteMany({})", Kind::Other, Some("deleteMany without filter"), false, false),
            ("db.a.deleteMany()", Kind::Other, Some("deleteMany without filter"), false, false),
            ("db.a.findOneAndDelete({_id: 1})", Kind::Other, None, false, false),
            ("db.a.drop()", Kind::Other, Some("drop"), true, false),
            ("db.dropDatabase()", Kind::Other, Some("dropDatabase"), true, false),
            ("db.a.dropIndex('x_1')", Kind::Other, Some("dropIndex"), true, false),
            ("db.a.createIndex({x: 1})", Kind::Other, None, true, false),
            ("db.createCollection('b')", Kind::Other, None, true, false),
            ("db.a.aggregate([{$match: {}}, {$out: 'b'}])", Kind::Other, None, true, false),
            ("db.a.aggregate([{$merge: {into: 'b'}}]).explain()", Kind::Other, None, true, false),
            ("db.a.find({}, {}, {pipeline: [{\"\\u0024out\": 'b'}]})", Kind::Other, None, true, false),
            ("db.runCommand({drop: 'a'})", Kind::Other, Some("drop"), true, false),
            ("db.runCommand({aggregate: 'a', pipeline: [{$out: 'b'}], cursor: {}})", Kind::Other, None, true, false),
            ("db.runCommand({insert: 'a', documents: [{}]})", Kind::Other, None, false, false),
            ("db.runCommand({Find: 'a'})", Kind::Other, None, false, false),
            ("db.adminCommand({shutdown: 1})", Kind::Other, None, false, false),
        ] {
            assert_eq!(split(st).statements[0].kind, kind, "{st}");
            assert_eq!(destructive_reason(st, MG), destructive, "{st}");
            assert_eq!(is_schema_changing(st, MG), schema, "{st}");
            assert_eq!(read_only_violation(st, MG).is_none(), kind != Kind::Other, "{st}");
            assert_eq!(prod_agent_violation(st, MG).is_none(), agent_prod_ok, "{st}");
        }
        assert_eq!(read_only_violation("db.a.find()\ndb.a.drop()", MG).unwrap(), "Read-only connection: db.a.drop(…) is not allowed.");
        // A statement that does not parse is never a read.
        assert!(read_only_violation("db.a.find(", MG).is_some());
        assert!(prod_agent_violation("db.a.find(", MG).is_some());
    }

    #[test]
    fn directives_are_written_with_slashes() {
        let src = "db.a.find()";
        let with = set_connection(src, MG, Some("m"));
        assert_eq!(with, "// @connection m\ndb.a.find()");
        assert_eq!(set_database(&with, MG, Some("x")), "// @connection m\n// @database x\ndb.a.find()");
        assert_eq!(set_connection(&with, MG, None), "db.a.find()");
    }
}
