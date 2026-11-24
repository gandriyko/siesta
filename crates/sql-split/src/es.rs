//! Elasticsearch `.es` files in Kibana Dev Tools console format: a `METHOD /path` line starts a
//! request, the lines up to the next request line are its body (JSON, or NDJSON for `_bulk`).
//! `#` and `//` lines are comments; `# @word value` is a directive, as `-- @word` in SQL.
//! Kibana's `"""…"""` strings may span lines.
//!
//! The same split serves the editor, the CLI and the executor ([`request`]), so they agree on
//! what a request is.

use crate::{Diagnostic, Kind, Severity, SqlFile, Statement};

const METHODS: [&str; 6] = ["GET", "POST", "PUT", "DELETE", "HEAD", "PATCH"];

/// POST endpoints that only read (anything else POSTed writes).
const READ_POST: [&str; 13] = ["_search", "_msearch", "_count", "_mget", "_sql", "_field_caps", "_validate", "_explain", "_analyze", "_async_search", "_terms_enum", "_eql", "_query"];

/// One request of an `.es` statement text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// Upper case.
    pub method: String,
    /// As written, with its query string; always starts with `/`.
    pub path: String,
    /// Comment lines dropped, `"""…"""` turned into JSON strings; empty when there is none.
    pub body: String,
}

/// The request of one statement (the text `Statement::text` gives for an Elastic file).
pub fn request(text: &str) -> Option<Request> {
    let (first, rest) = text.split_once('\n').unwrap_or((text, ""));
    let (method, path) = request_line(first)?;
    let body: Vec<&str> = rest.lines().filter(|l| !is_comment(l)).collect();
    let path = if path.starts_with('/') { path.to_string() } else { format!("/{path}") };
    Some(Request { method: method.to_ascii_uppercase(), path, body: triple_quotes(body.join("\n").trim()) })
}

/// `METHOD path` at the start of a line (indentation allowed).
fn request_line(line: &str) -> Option<(&str, &str)> {
    let t = line.trim();
    let (m, rest) = t.split_once(char::is_whitespace).unwrap_or((t, ""));
    METHODS.iter().any(|x| m.eq_ignore_ascii_case(x)).then_some((m, rest.trim()))
}

fn is_comment(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with('#') || t.starts_with("//")
}

/// The path's segments without the query string: `/a/_search?q=x` → `["a", "_search"]`.
fn segments(path: &str) -> Vec<&str> {
    path.split(['?', '#']).next().unwrap_or("").split('/').filter(|s| !s.is_empty()).collect()
}

pub(crate) fn classify(method: &str, path: &str) -> Kind {
    let segs = segments(path);
    let m = method.to_ascii_uppercase();
    if matches!(m.as_str(), "GET" | "POST") && segs.last() == Some(&"_search") {
        // The executor pages it with `from` / `size`.
        return Kind::Query;
    }
    if matches!(m.as_str(), "GET" | "HEAD") || (m == "POST" && segs.iter().any(|s| READ_POST.contains(s))) {
        return Kind::Read;
    }
    Kind::Other
}

/// Why a request needs a confirmation where destructive statements do (`confirm_destructive`):
/// deleting an index / alias / template, `_delete_by_query`, `_update_by_query` without a query,
/// closing an index. Deleting one document (`DELETE /i/_doc/1`) is not.
pub(crate) fn reason(r: &Request) -> Option<&'static str> {
    let segs = segments(&r.path);
    match r.method.as_str() {
        "DELETE" if !segs.contains(&"_doc") => Some("DELETE"),
        _ if segs.contains(&"_delete_by_query") => Some("_delete_by_query"),
        _ if segs.contains(&"_update_by_query") && !r.body.contains("\"query\"") => Some("_update_by_query without query"),
        "POST" if segs.contains(&"_close") => Some("_close"),
        _ => None,
    }
}

/// Indices, mappings or aliases may have changed: the cached schema is stale.
pub(crate) fn schema_changing(r: &Request) -> bool {
    let segs = segments(&r.path);
    let doc = segs.iter().any(|s| matches!(*s, "_doc" | "_create" | "_update" | "_bulk"));
    let admin = ["_mapping", "_aliases", "_alias", "_reindex", "_rollover", "_clone", "_split", "_shrink", "_open", "_close"];
    match r.method.as_str() {
        "PUT" | "DELETE" => !doc,
        "POST" => segs.iter().any(|s| admin.contains(s)),
        _ => false,
    }
}

pub(crate) fn read_only_violation(r: &Request) -> Option<String> {
    (classify(&r.method, &r.path) == Kind::Other).then(|| format!("Read-only connection: {} {} is not allowed.", r.method, r.path))
}

/// An agent on `prod`: reads, and updating one document by id (`POST /i/_update/<id>`, the
/// counterpart of SQL's `UPDATE … WHERE`).
pub(crate) fn prod_agent_violation(r: &Request) -> Option<String> {
    if classify(&r.method, &r.path) != Kind::Other {
        return None;
    }
    let segs = segments(&r.path);
    let one_doc = r.method == "POST" && segs.len() == 3 && segs[1] == "_update";
    (!one_doc).then(|| format!("Production connection: {} {} is not allowed; only reads and POST <index>/_update/<id>.", r.method, r.path))
}

/// Requests of every statement in `text` (an Elastic file or one statement).
pub(crate) fn requests(text: &str) -> Vec<Request> {
    let file = split(text);
    file.statements.iter().filter_map(|s| request(s.text(text))).collect()
}

/// Kibana's `"""raw"""` as a JSON string (`"raw"` with quotes, backslashes and newlines escaped).
fn triple_quotes(body: &str) -> String {
    let mut out = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(open) = rest.find(r#"""""#) {
        let Some(close) = rest[open + 3..].find(r#"""""#).map(|c| open + 3 + c) else { break };
        out.push_str(&rest[..open]);
        out.push('"');
        for c in rest[open + 3..close].chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c => out.push(c),
            }
        }
        out.push('"');
        rest = &rest[close + 3..];
    }
    out.push_str(rest);
    out
}

pub(crate) fn split(src: &str) -> SqlFile {
    let mut out = SqlFile::default();
    let mut pending = crate::Pending::default();
    // The open request: start, end of its last content line, line number, its directives.
    let mut open: Option<(usize, usize, usize, crate::Pending)> = None;
    let mut in_triple: Option<usize> = None;
    let mut off = 0;
    let finish = |out: &mut SqlFile, open: &mut Option<(usize, usize, usize, crate::Pending)>| {
        if let Some((start, end, line, p)) = open.take() {
            let text = &src[start..end];
            let (first, _) = text.split_once('\n').unwrap_or((text, ""));
            let (m, path) = request_line(first).unwrap_or(("", ""));
            out.statements.push(Statement { range: start..end, line, name: p.name, page_size: p.page_size, confirm: p.confirm, kind: classify(m, path) });
            if let Some(r) = request(text) {
                let segs = segments(&r.path);
                let all = segs.contains(&"_delete_by_query") && (r.body.is_empty() || r.body.contains("match_all"));
                let unfiltered = segs.contains(&"_update_by_query") && !r.body.contains("\"query\"");
                if all || unfiltered {
                    let what = if all { "_delete_by_query matching everything: every document deletes" } else { "_update_by_query without query: every document changes" };
                    out.diagnostics.push(Diagnostic { range: start..start + first.len(), severity: Severity::Warning, message: what.into() });
                }
            }
        }
    };
    for (n, raw) in src.split_inclusive('\n').enumerate() {
        let line = raw.trim_end_matches(['\n', '\r']);
        let (start, end) = (off, off + line.len());
        off += raw.len();
        let t = line.trim();
        if in_triple.is_some() {
            if line.matches(r#"""""#).count() % 2 == 1 {
                in_triple = None;
            }
            if let Some(o) = open.as_mut() {
                o.1 = end;
            }
            continue;
        }
        if t.is_empty() {
            continue;
        }
        if is_comment(t) {
            let body = t.trim_start_matches('#').trim_start_matches("//").trim();
            if let Some(rest) = body.strip_prefix('@') {
                // Directives between requests belong to the next one (like SQL's between statements).
                let at = start + (line.len() - line.trim_start().len())..end;
                let top = open.is_none() && out.statements.is_empty();
                crate::directive_word(&mut out, &mut pending, rest, at, top);
            }
            continue;
        }
        if request_line(t).is_some() {
            finish(&mut out, &mut open);
            open = Some((start + line.len() - line.trim_start().len(), end, n + 1, std::mem::take(&mut pending)));
        } else if let Some(o) = open.as_mut() {
            o.1 = end;
        } else {
            out.diagnostics.push(Diagnostic { range: start..end, severity: Severity::Error, message: "Expected a request line: METHOD /path".into() });
            continue;
        }
        if line.matches(r#"""""#).count() % 2 == 1 {
            in_triple = Some(start + line.find(r#"""""#).unwrap_or(0));
        }
    }
    if let Some(at) = in_triple {
        out.diagnostics.push(Diagnostic { range: at..src.len(), severity: Severity::Error, message: "Unclosed \"\"\"".into() });
    }
    finish(&mut out, &mut open);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Dialect::Elastic as ES, destructive_reason, is_schema_changing, prod_agent_violation, read_only_violation, set_connection, set_database};

    #[test]
    fn splits_requests_with_bodies_and_directives() {
        let src = "# @connection es-dev\n# @database x\n\n# plain\nGET /orders/_search\n{\n  \"query\": {\"match_all\": {}}\n}\n\n// @name count\nPOST orders/_count\n\nGET _cat/indices\nPUT /i/_doc/1\n{\"a\": \"\"\"x\n\"y\" z\"\"\"}\n# trailing\n";
        let f = split(src);
        assert_eq!((f.connection.as_deref(), f.database.as_deref()), (Some("es-dev"), Some("x")));
        let got: Vec<(&str, usize, Kind, Option<&str>)> = f.statements.iter().map(|s| (s.text(src), s.line, s.kind, s.name.as_deref())).collect();
        assert_eq!(
            got,
            [
                ("GET /orders/_search\n{\n  \"query\": {\"match_all\": {}}\n}", 5, Kind::Query, None),
                ("POST orders/_count", 11, Kind::Read, Some("count")),
                ("GET _cat/indices", 13, Kind::Read, None),
                ("PUT /i/_doc/1\n{\"a\": \"\"\"x\n\"y\" z\"\"\"}", 14, Kind::Other, None),
            ]
        );
        assert!(f.diagnostics.is_empty(), "{:?}", f.diagnostics);
        let r = request(f.statements[3].text(src)).unwrap();
        assert_eq!((r.method.as_str(), r.path.as_str(), r.body.as_str()), ("PUT", "/i/_doc/1", r#"{"a": "x\n\"y\" z"}"#));
        assert_eq!(request("post orders/_count").unwrap().path, "/orders/_count");
        // A cursor between requests belongs to the previous one.
        assert_eq!(f.statement_at(src.find("// @name").unwrap()).unwrap().line, 5);
    }

    #[test]
    fn diagnostics() {
        let f = split("{\"x\": 1}\n# @connection late\nGET /a\n# @connection late\nGET /b\n{\"a\": \"\"\"open\n");
        let msgs: Vec<&str> = f.diagnostics.iter().map(|d| d.message.as_str()).collect();
        assert_eq!(msgs, ["Expected a request line: METHOD /path", "`@connection` applies only at the top of the file", "Unclosed \"\"\""]);
        // Text before the first request does not make it a late directive.
        assert_eq!(f.connection.as_deref(), Some("late"));
        let warn = split("POST /i/_delete_by_query\n{\"query\": {\"match_all\": {}}}\nPOST /i/_update_by_query\nPOST /i/_delete_by_query\n{\"query\": {\"term\": {\"a\": 1}}}");
        let msgs: Vec<&str> = warn.diagnostics.iter().map(|d| d.message.as_str()).collect();
        assert_eq!(msgs, ["_delete_by_query matching everything: every document deletes", "_update_by_query without query: every document changes"]);
    }

    #[test]
    fn kinds_and_safety() {
        for (req, kind, destructive, schema, agent_prod_ok) in [
            ("GET /i/_search", Kind::Query, None, false, true),
            ("POST /i,j/_search?q=x", Kind::Query, None, false, true),
            ("GET /i/_search/template", Kind::Read, None, false, true),
            ("GET /i/_mapping", Kind::Read, None, false, true),
            ("HEAD /i", Kind::Read, None, false, true),
            ("POST /_sql?format=json", Kind::Read, None, false, true),
            ("POST /i/_count", Kind::Read, None, false, true),
            ("POST /_msearch", Kind::Read, None, false, true),
            ("PUT /i/_doc/1", Kind::Other, None, false, false),
            ("POST /i/_doc", Kind::Other, None, false, false),
            ("POST /i/_update/1", Kind::Other, None, false, true),
            ("POST /_bulk", Kind::Other, None, false, false),
            ("DELETE /i/_doc/1", Kind::Other, None, false, false),
            ("DELETE /i", Kind::Other, Some("DELETE"), true, false),
            ("PUT /i", Kind::Other, None, true, false),
            ("PUT /i/_mapping", Kind::Other, None, true, false),
            ("POST /_aliases", Kind::Other, None, true, false),
            ("POST /i/_close", Kind::Other, Some("_close"), true, false),
            ("POST /i/_delete_by_query", Kind::Other, Some("_delete_by_query"), false, false),
            ("POST /i/_update_by_query", Kind::Other, Some("_update_by_query without query"), false, false),
            ("POST /i/_update_by_query\n{\"query\": {}}", Kind::Other, None, false, false),
        ] {
            assert_eq!(split(req).statements[0].kind, kind, "{req}");
            assert_eq!(destructive_reason(req, ES), destructive, "{req}");
            assert_eq!(is_schema_changing(req, ES), schema, "{req}");
            assert_eq!(read_only_violation(req, ES).is_none(), kind != Kind::Other, "{req}");
            assert_eq!(prod_agent_violation(req, ES).is_none(), agent_prod_ok, "{req}");
        }
        assert_eq!(read_only_violation("GET /a\nDELETE /a", ES).unwrap(), "Read-only connection: DELETE /a is not allowed.");
    }

    #[test]
    fn directives_are_written_with_hash() {
        let src = "GET /a";
        let with = set_connection(src, ES, Some("es"));
        assert_eq!(with, "# @connection es\nGET /a");
        assert_eq!(set_database(&with, ES, Some("x")), "# @connection es\n# @database x\nGET /a");
        assert_eq!(set_connection(&with, ES, Some("other")), "# @connection other\nGET /a");
        assert_eq!(set_connection(&with, ES, None), "GET /a");
    }
}
