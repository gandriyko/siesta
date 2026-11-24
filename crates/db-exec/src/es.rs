//! Elasticsearch over HTTP (`reqwest`): one client per [`crate::Session`], requests from `.es`
//! statements ([`sql_split::es_request`]), responses shaped into the same [`Outcome`] tables:
//! search hits (`_id` + `_source` fields), `_sql` columns / rows, `_cat` lists, else the whole
//! JSON response in one `response` cell.
//!
//! Paging: a `_search` whose body sets neither `size` nor `from` gets `from` / `size + 1` (the
//! extra hit says another page exists), the result panel's sort as `sort` and its filters as a
//! `bool` filter around the query. A body that sets them runs as written.
//! Cancel drops the request on the client; the server finishes its part (bounded by the timeout).
// ponytail: client-side cancel; send an `X-Opaque-Id` and `POST _tasks/<id>/_cancel` if long
// searches must stop on the server too.

use std::sync::Arc;
use std::time::{Duration, Instant};

use db_conn::{Connection, SslMode};
use serde_json::{Map, Value as J, json};
use sql_split::{EsRequest, Kind};

use crate::{Column, DbError, Outcome, Page, Value, wrap::{Cond, Op, parse_filter}};

enum Auth {
    None,
    Basic(String, Option<String>),
    ApiKey(String),
}

pub(crate) struct Es {
    client: reqwest::Client,
    /// `http(s)://host:port`, no trailing `/`.
    pub base: String,
    auth: Auth,
    /// Woken by [`crate::CancelHandle`]: the request in flight is dropped.
    pub abort: Arc<tokio::sync::Notify>,
    pub timeout: Option<Duration>,
}

/// `http(s)://host:port` of the connection: the fields over the url, port 9200 by default,
/// `https` with an SSL mode other than `disable` (or an `https://` url).
pub(crate) fn base_url(c: &Connection) -> Result<String, DbError> {
    let parsed = c.url.as_deref().map(db_conn::parse_url).transpose().map_err(|e| DbError::msg(format!("invalid url: {e}")))?.unwrap_or_default();
    let host = c.host.clone().unwrap_or(if parsed.host.is_empty() { "localhost".into() } else { parsed.host });
    let host = if host.contains(':') { format!("[{host}]") } else { host };
    let port = c.port.or(parsed.port).unwrap_or(9200);
    let tls = !matches!(c.ssl.mode.or(parsed.ssl_mode), None | Some(SslMode::Disable));
    Ok(format!("{}://{host}:{port}", if tls { "https" } else { "http" }))
}

fn auth(c: &Connection) -> Auth {
    let parsed = c.url.as_deref().and_then(|u| db_conn::parse_url(u).ok()).unwrap_or_default();
    let user = c.user.clone().or((!parsed.user.is_empty()).then_some(parsed.user));
    let password = c.password.clone().or((!parsed.password.is_empty()).then_some(parsed.password));
    match (user, password) {
        (Some(u), p) => Auth::Basic(u, p),
        (None, Some(key)) => Auth::ApiKey(key),
        (None, None) => Auth::None,
    }
}

fn client(c: &Connection) -> Result<reqwest::Client, DbError> {
    let read = |p: &std::path::Path| std::fs::read(p).map_err(|e| DbError::msg(format!("{}: {e}", p.display())));
    let mut b = reqwest::Client::builder().danger_accept_invalid_certs(c.ssl.mode == Some(SslMode::Require) || c.ssl.mode == Some(SslMode::Prefer));
    if let Some(ca) = &c.ssl.ca {
        for cert in reqwest::Certificate::from_pem_bundle(&read(ca)?).map_err(|e| DbError::msg(format!("{}: {e}", ca.display())))? {
            b = b.add_root_certificate(cert);
        }
    }
    if let Some(cert) = &c.ssl.cert {
        let mut pem = read(cert)?;
        if let Some(key) = &c.ssl.key {
            pem.push(b'\n');
            pem.extend(read(key)?);
        }
        b = b.identity(reqwest::Identity::from_pem(&pem).map_err(|e| DbError::msg(format!("client certificate or key not usable: {e}")))?);
    }
    b.build().map_err(|e| DbError::msg(chain(&e)))
}

/// An error with its causes: reqwest's own text alone says only "error sending request".
fn chain(e: &dyn std::error::Error) -> String {
    let mut s = e.to_string();
    let mut cur = e.source();
    while let Some(c) = cur {
        s = format!("{s}: {c}");
        cur = c.source();
    }
    s
}

impl Es {
    /// Builds the client and checks the server answers `GET /` (address, TLS, credentials).
    pub(crate) async fn open(c: &Connection) -> Result<Es, DbError> {
        let es = Es { client: client(c)?, base: base_url(c)?, auth: auth(c), abort: Arc::default(), timeout: None };
        let (status, text) = es.send("GET", "/", "").await?;
        if status == 401 && !c.has_password() {
            let mut e = http_error(status, &text);
            e.message = format!("{}. No password or API key is stored for `{1}`: set it in the connection form or with `siesta connections set-password {1}`", e.message, c.name);
            return Err(e);
        }
        if status >= 400 {
            return Err(http_error(status, &text));
        }
        Ok(es)
    }

    /// One HTTP request; `(status, body text)`. A cancel or the statement timeout ends it.
    pub(crate) async fn send(&self, method: &str, path: &str, body: &str) -> Result<(u16, String), DbError> {
        let m = reqwest::Method::from_bytes(method.as_bytes()).map_err(|_| DbError::msg(format!("unknown method {method}")))?;
        let mut rb = self.client.request(m, format!("{}{path}", self.base));
        rb = match &self.auth {
            Auth::None => rb,
            Auth::Basic(u, p) => rb.basic_auth(u, p.as_ref()),
            Auth::ApiKey(k) => rb.header(reqwest::header::AUTHORIZATION, format!("ApiKey {k}")),
        };
        if !body.is_empty() {
            let ndjson = ["_bulk", "_msearch"].iter().any(|s| path.contains(s));
            let ct = if ndjson { "application/x-ndjson" } else { "application/json" };
            // NDJSON needs the final newline.
            rb = rb.header(reqwest::header::CONTENT_TYPE, ct).body(if ndjson { format!("{body}\n") } else { body.to_string() });
        }
        if let Some(t) = self.timeout {
            rb = rb.timeout(t);
        }
        let go = async {
            let r = rb.send().await.map_err(request_error)?;
            let status = r.status().as_u16();
            Ok((status, r.text().await.map_err(request_error)?))
        };
        tokio::select! {
            r = go => r,
            _ = self.abort.notified() => Err(DbError::msg("Cancelled (the request was dropped; Elasticsearch may still finish it)")),
        }
    }

    pub(crate) async fn run(&self, text: &str, kind: Kind, page: &Page, max_rows: usize) -> Result<Outcome, DbError> {
        let started = Instant::now();
        let r = sql_split::es_request(text).ok_or_else(|| DbError::msg("Not a request: the first line must be METHOD /path"))?;
        let path = cat_as_json(&r.path);
        // The safety checks classified the written text; refuse if the URL the client really sends
        // is another kind of request (any path trick the normalization missed).
        let url = reqwest::Url::parse(&format!("{}{path}", self.base)).map_err(|e| DbError::msg(format!("invalid path {path}: {e}")))?;
        if sql_split::es_kind(&r.method, url.path()) != kind {
            return Err(DbError { message: format!("Refused: {} {path} is sent as {}, another kind of request than it reads as", r.method, url.path()), refused: true, ..Default::default() });
        }
        let paged = if kind == Kind::Query { paged_body(&r.body, page) } else { None };
        let body = paged.clone().unwrap_or_else(|| r.body.clone());
        let (status, resp) = self.send(&r.method, &path, &body).await?;
        if status >= 400 && r.method != "HEAD" {
            return Err(http_error(status, &resp));
        }
        let mut s = shape(&r, status, &resp, paged.is_some().then_some(page.size), max_rows);
        if paged.is_none() && kind == Kind::Query {
            s.notices.push("The body sets size / from (or is not a JSON object), so it runs as written: no paging, sorting is local, filters are off.".into());
        }
        Ok(Outcome {
            columns: s.columns,
            rows: s.rows,
            wrapped: paged.is_some(),
            has_more: s.has_more,
            truncated: s.truncated,
            rows_affected: (kind == Kind::Other).then_some(s.affected.unwrap_or(0)),
            notices: s.notices,
            executed_sql: if body.is_empty() { format!("{} {path}", r.method) } else { format!("{} {path}\n{body}", r.method) },
            params: Vec::new(),
            in_transaction: false,
            raw: s.raw,
            duration: started.elapsed(),
        })
    }

    /// `_count` with the same query and filters as the `_search` in `text`.
    pub(crate) async fn count(&self, text: &str, filters: &[(String, String)]) -> Result<u64, DbError> {
        let r = sql_split::es_request(text).ok_or_else(|| DbError::msg("Not a request"))?;
        let (path, query) = r.path.split_once('?').map_or((r.path.as_str(), ""), |(p, q)| (p, q));
        let path = format!("{}_count{}", path.strip_suffix("_search").unwrap_or(path), if query.is_empty() { String::new() } else { format!("?{query}") });
        let body: Map<String, J> = serde_json::from_str(if r.body.is_empty() { "{}" } else { &r.body }).map_err(|e| DbError::msg(format!("Cannot count: the body is not a JSON object ({e})")))?;
        let page = Page { filters: filters.to_vec(), ..Default::default() };
        let query = filtered_query(body.get("query").cloned(), &page);
        let (status, resp) = self.send("POST", &path, &json!({ "query": query }).to_string()).await?;
        if status >= 400 {
            return Err(http_error(status, &resp));
        }
        serde_json::from_str::<J>(&resp).ok().and_then(|v| v["count"].as_u64()).ok_or_else(|| DbError::msg(format!("unexpected count result: {resp}")))
    }

    /// Index names (no hidden `.` ones) with their flattened mappings: `(index, [(field, type)])`.
    pub(crate) async fn mappings(&self) -> Result<Vec<(String, Vec<(String, String)>)>, DbError> {
        let (status, resp) = self.send("GET", "/_mapping", "").await?;
        if status >= 400 {
            return Err(http_error(status, &resp));
        }
        Ok(flatten_mappings(&serde_json::from_str(&resp).unwrap_or_default()))
    }
}

/// `_cat/*` answers in JSON unless the path asks for a format.
fn cat_as_json(path: &str) -> String {
    let cat = path.trim_start_matches('/').starts_with("_cat");
    if !cat || path.contains("format=") {
        return path.to_string();
    }
    format!("{path}{}format=json", if path.contains('?') { "&" } else { "?" })
}

fn request_error(e: reqwest::Error) -> DbError {
    if e.is_timeout() {
        return DbError::msg("No answer within the statement timeout (run.statement_timeout_ms)");
    }
    DbError { message: chain(&e), connection_lost: e.is_connect(), ..Default::default() }
}

/// Elasticsearch's `{"error": {"type", "reason", "root_cause", "caused_by"}, "status"}`.
pub(crate) fn http_error(status: u16, text: &str) -> DbError {
    let v: J = serde_json::from_str(text).unwrap_or(J::Null);
    let err = &v["error"];
    let mut out = DbError::msg(match err {
        J::Object(_) => err["reason"].as_str().unwrap_or("").to_string(),
        J::String(s) => s.clone(),
        _ if text.trim().is_empty() => format!("HTTP {status}"),
        _ => text.trim().to_string(),
    });
    out.code = Some(err["type"].as_str().map_or_else(|| format!("HTTP {status}"), str::to_string));
    let caused = err["caused_by"]["reason"].as_str().or(err["root_cause"][0]["reason"].as_str()).filter(|r| *r != out.message);
    out.detail = caused.map(str::to_string);
    if out.message.is_empty() {
        out.message = format!("HTTP {status}");
    }
    out
}

/// The `_search` body with paging, sort and filters, or `None` when it must run as written.
pub(crate) fn paged_body(body: &str, page: &Page) -> Option<String> {
    let mut b: Map<String, J> = if body.trim().is_empty() { Map::new() } else { serde_json::from_str(body).ok()? };
    if b.contains_key("size") || b.contains_key("from") {
        return None;
    }
    b.insert("from".into(), json!(page.offset));
    b.insert("size".into(), json!(page.size + 1));
    if !page.sort.is_empty() {
        b.insert("sort".into(), J::Array(page.sort.iter().map(|(c, desc)| json!({ c: if *desc { "desc" } else { "asc" } })).collect()));
    }
    if page.filters.iter().any(|f| parse_filter(&f.1).is_some()) {
        let q = filtered_query(b.remove("query"), page);
        b.insert("query".into(), q);
    }
    Some(J::Object(b).to_string())
}

/// `query` with the panel's filters as a `bool` filter around it.
fn filtered_query(query: Option<J>, page: &Page) -> J {
    let conds: Vec<J> = page.filters.iter().filter_map(|(col, text)| parse_filter(text).map(|c| condition(col, c))).collect();
    if conds.is_empty() {
        return query.unwrap_or_else(|| json!({ "match_all": {} }));
    }
    let mut b = Map::new();
    if let Some(q) = query {
        b.insert("must".into(), q);
    }
    b.insert("filter".into(), J::Array(conds));
    json!({ "bool": b })
}

fn condition(col: &str, c: Cond) -> J {
    let range = |op: &str, v: String| json!({ "range": { col: { op: v } } });
    match c {
        Cond::Contains(v) => json!({ "wildcard": { col: { "value": format!("*{v}*"), "case_insensitive": true } } }),
        Cond::Cmp(Op::Eq, v) => json!({ "match": { col: v } }),
        Cond::Cmp(Op::Ne, v) => json!({ "bool": { "must_not": { "match": { col: v } } } }),
        Cond::Cmp(Op::Gt, v) => range("gt", v),
        Cond::Cmp(Op::Ge, v) => range("gte", v),
        Cond::Cmp(Op::Lt, v) => range("lt", v),
        Cond::Cmp(Op::Le, v) => range("lte", v),
        Cond::IsNull => json!({ "bool": { "must_not": { "exists": { "field": col } } } }),
        Cond::NotNull => json!({ "exists": { "field": col } }),
        Cond::In(vs) => json!({ "terms": { col: vs } }),
    }
}

#[derive(Debug, Default)]
pub(crate) struct Shaped {
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<Value>>,
    pub has_more: bool,
    pub truncated: bool,
    pub affected: Option<u64>,
    pub notices: Vec<String>,
    /// The response when it is JSON.
    pub raw: Option<J>,
}

pub(crate) fn value(v: &J) -> Value {
    match v {
        J::Null => Value::Null,
        J::Bool(b) => Value::Bool(*b),
        J::Number(n) => n.as_i64().map(Value::Int).or_else(|| n.as_u64().map(|u| Value::Decimal(u.to_string()))).unwrap_or_else(|| Value::Float(n.as_f64().unwrap_or(f64::NAN))),
        J::String(s) => Value::Text(s.clone()),
        other => Value::Json(other.clone()),
    }
}

fn type_of(v: &J) -> &'static str {
    match v {
        J::Null => "null",
        J::Bool(_) => "boolean",
        J::Number(n) if n.is_i64() || n.is_u64() => "long",
        J::Number(_) => "double",
        J::String(_) => "keyword",
        _ => "object",
    }
}

/// A table of objects: columns in first-seen key order, typed by the first non-null value.
fn table<'a>(objects: impl IntoIterator<Item = Vec<(&'a str, &'a J)>>, max_rows: usize) -> (Vec<Column>, Vec<Vec<Value>>, bool) {
    let mut names: Vec<&str> = Vec::new();
    let mut types: Vec<&'static str> = Vec::new();
    let mut cells: Vec<Vec<(&str, &J)>> = Vec::new();
    let mut truncated = false;
    for o in objects {
        if cells.len() >= max_rows {
            truncated = true;
            break;
        }
        for (k, v) in &o {
            match names.iter().position(|n| n == k) {
                Some(i) if types[i] == "null" => types[i] = type_of(v),
                Some(_) => {}
                None => {
                    names.push(k);
                    types.push(type_of(v));
                }
            }
        }
        cells.push(o);
    }
    let columns = names.iter().zip(&types).map(|(n, t)| Column { name: n.to_string(), type_name: t.to_string() }).collect();
    let rows = cells.iter().map(|o| names.iter().map(|n| o.iter().find(|(k, _)| k == n).map_or(Value::Null, |(_, v)| value(v))).collect()).collect();
    (columns, rows, truncated)
}

/// The response as a table (see the module docs). `page_size`: the request was paged (one extra hit asked for).
pub(crate) fn shape(r: &EsRequest, status: u16, text: &str, page_size: Option<u64>, max_rows: usize) -> Shaped {
    let mut s = Shaped::default();
    if r.method == "HEAD" {
        s.columns = vec![Column { name: "status".into(), type_name: "long".into() }];
        s.rows = vec![vec![Value::Int(status as i64)]];
        return s;
    }
    let Ok(v) = serde_json::from_str::<J>(text) else {
        s.columns = vec![Column { name: "response".into(), type_name: "text".into() }];
        s.rows = vec![vec![Value::Text(text.to_string())]];
        return s;
    };
    s.affected = affected(&v);
    let hits = v["hits"]["hits"].as_array();
    let aggs = v.get("aggregations").is_some();
    if let Some(hits) = hits.filter(|h| !h.is_empty() || !aggs) {
        let limit = page_size.map_or(max_rows, |n| n as usize);
        let indices: std::collections::HashSet<&str> = hits.iter().filter_map(|h| h["_index"].as_str()).collect();
        let objects = hits.iter().map(|h| {
            let mut o: Vec<(&str, &J)> = vec![("_id", &h["_id"])];
            if indices.len() > 1 {
                o.push(("_index", &h["_index"]));
            }
            let src = h.get("_source").or(h.get("fields"));
            o.extend(src.and_then(J::as_object).into_iter().flatten().map(|(k, v)| (k.as_str(), v)));
            o
        });
        let (columns, rows, cut) = table(objects, limit);
        (s.columns, s.rows) = (columns, rows);
        if page_size.is_some() { s.has_more = cut } else { s.truncated = cut }
        let total = &v["hits"]["total"];
        if let Some(n) = total["value"].as_u64().or(total.as_u64()) {
            let plus = if total["relation"] == "gte" { "+" } else { "" };
            s.notices.push(format!("{n}{plus} hits"));
        }
        if aggs {
            s.notices.push("The response also has aggregations: set \"size\": 0 to see them.".into());
        }
    } else if let (Some(cols), Some(rows)) = (v["columns"].as_array(), v["rows"].as_array()) {
        // `_sql`
        s.columns = cols.iter().map(|c| Column { name: c["name"].as_str().unwrap_or("").into(), type_name: c["type"].as_str().unwrap_or("").into() }).collect();
        s.truncated = rows.len() > max_rows;
        s.rows = rows.iter().take(max_rows).map(|r| r.as_array().into_iter().flatten().map(value).collect()).collect();
        if v.get("cursor").is_some() {
            s.notices.push("More rows: the response has a cursor (POST _sql {\"cursor\": \"…\"} gives the next page).".into());
        }
    } else if let Some(items) = v.as_array().filter(|a| !a.is_empty() && a.iter().all(J::is_object)) {
        // `_cat/…?format=json`
        let (columns, rows, cut) = table(items.iter().map(|o| o.as_object().into_iter().flatten().map(|(k, v)| (k.as_str(), v)).collect()), max_rows);
        (s.columns, s.rows, s.truncated) = (columns, rows, cut);
    } else {
        s.columns = vec![Column { name: "response".into(), type_name: "object".into() }];
        s.rows = vec![vec![value(&v)]];
    }
    if v["timed_out"] == true {
        s.notices.push("The search timed out: the hits are partial.".into());
    }
    if let Some(n) = v["_shards"]["failed"].as_u64().filter(|n| *n > 0) {
        s.notices.push(format!("{n} shard(s) failed: the hits are partial."));
    }
    let mut v = v;
    // The extra hit only told whether another page exists: the Raw view shows the page.
    // `pointer_mut`, not `v["hits"]`: indexing a non-object mutably panics (`_cat` arrays).
    if let (Some(n), Some(hits)) = (page_size, v.pointer_mut("/hits/hits").and_then(J::as_array_mut)) {
        hits.truncate(n as usize);
    }
    s.raw = Some(v);
    s
}

/// Documents a write touched: `deleted` / `updated` / `created` counts, `_bulk` items, or a
/// single document's `result`.
fn affected(v: &J) -> Option<u64> {
    let n: u64 = ["deleted", "updated", "created"].iter().filter_map(|k| v[k].as_u64()).sum();
    if ["deleted", "updated", "created"].iter().any(|k| v[k].is_u64()) {
        return Some(n);
    }
    if let Some(items) = v["items"].as_array() {
        return Some(items.len() as u64);
    }
    match v["result"].as_str()? {
        "created" | "updated" | "deleted" => Some(1),
        _ => Some(0),
    }
}

/// `GET /_mapping` → `(index, [(dotted field, type)])`, sorted, hidden (`.`) indices left out.
pub(crate) fn flatten_mappings(v: &J) -> Vec<(String, Vec<(String, String)>)> {
    fn walk(prefix: &str, props: &Map<String, J>, out: &mut Vec<(String, String)>) {
        for (k, f) in props {
            let name = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
            match f["properties"].as_object() {
                Some(p) => {
                    out.push((name.clone(), f["type"].as_str().unwrap_or("object").into()));
                    walk(&name, p, out);
                }
                None => out.push((name, f["type"].as_str().unwrap_or("object").into())),
            }
        }
    }
    let mut out: Vec<(String, Vec<(String, String)>)> = v
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(i, _)| !i.starts_with('.'))
        .map(|(i, m)| {
            let mut fields = Vec::new();
            if let Some(p) = m["mappings"]["properties"].as_object() {
                walk("", p, &mut fields);
            }
            (i.clone(), fields)
        })
        .collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(text: &str) -> EsRequest {
        sql_split::es_request(text).unwrap()
    }

    #[test]
    fn base_url_and_auth() {
        let mut c = Connection { name: "e".into(), driver: db_conn::Driver::Elastic, url: None, host: None, socket: None, port: None, database: None, user: None, password: None, password_enc: None, ssl: Default::default(), read_only: false, prod: false, schema: None, init_sql: None, color: None };
        assert_eq!(base_url(&c).unwrap(), "http://localhost:9200");
        c.url = Some("https://u:p@es.example.com".into());
        assert_eq!(base_url(&c).unwrap(), "https://es.example.com:9200");
        assert!(matches!(auth(&c), Auth::Basic(u, Some(p)) if u == "u" && p == "p"));
        (c.url, c.host, c.port, c.ssl.mode) = (None, Some("::1".into()), Some(9201), Some(SslMode::Require));
        assert_eq!(base_url(&c).unwrap(), "https://[::1]:9201");
        c.password = Some("key".into());
        assert!(matches!(auth(&c), Auth::ApiKey(k) if k == "key"));
        assert_eq!(cat_as_json("/_cat/indices?v"), "/_cat/indices?v&format=json");
        assert_eq!(cat_as_json("/_cat/indices?format=txt"), "/_cat/indices?format=txt");
        assert_eq!(cat_as_json("/i/_search"), "/i/_search");
    }

    #[test]
    fn paging_sort_and_filters() {
        let page = Page { offset: 100, size: 50, sort: vec![("age".into(), true)], filters: vec![("name".into(), "ann".into()), ("age".into(), ">=18".into()), ("x".into(), "".into())] };
        let b: J = serde_json::from_str(&paged_body(r#"{"query": {"term": {"a": 1}}}"#, &page).unwrap()).unwrap();
        assert_eq!(
            b,
            json!({"from": 100, "size": 51, "sort": [{"age": "desc"}], "query": {"bool": {"must": {"term": {"a": 1}}, "filter": [
                {"wildcard": {"name": {"value": "*ann*", "case_insensitive": true}}}, {"range": {"age": {"gte": "18"}}}]}}})
        );
        let plain: J = serde_json::from_str(&paged_body("", &Page::default()).unwrap()).unwrap();
        assert_eq!(plain, json!({"from": 0, "size": 101}));
        assert_eq!(paged_body(r#"{"size": 0, "aggs": {}}"#, &Page::default()), None);
        assert_eq!(paged_body("not json", &Page::default()), None);
    }

    #[test]
    fn hits_become_rows() {
        let resp = r#"{"took": 1, "timed_out": false, "hits": {"total": {"value": 10000, "relation": "gte"}, "hits": [
            {"_index": "a", "_id": "1", "_source": {"name": "ann", "age": 30}},
            {"_index": "b", "_id": "2", "_source": {"age": null, "tags": ["x"], "name": "bob"}},
            {"_index": "a", "_id": "3", "_source": {"name": "cy", "age": 4.5}}]}}"#;
        let s = shape(&req("GET /a,b/_search"), 200, resp, Some(2), 100);
        let cols: Vec<(&str, &str)> = s.columns.iter().map(|c| (c.name.as_str(), c.type_name.as_str())).collect();
        assert_eq!(cols, [("_id", "keyword"), ("_index", "keyword"), ("name", "keyword"), ("age", "long"), ("tags", "object")]);
        assert_eq!(s.rows.len(), 2);
        assert_eq!(s.rows[1], vec![Value::Text("2".into()), Value::Text("b".into()), Value::Text("bob".into()), Value::Null, Value::Json(json!(["x"]))]);
        assert!(s.has_more && !s.truncated);
        assert_eq!(s.notices, ["10000+ hits"]);
        // Raw keeps the whole response, cut to the page.
        let raw = s.raw.unwrap();
        assert_eq!((raw["took"].clone(), raw["hits"]["hits"].as_array().unwrap().len()), (json!(1), 2));
        // Unpaged: the row cap, not a page.
        let s = shape(&req("GET /a/_search"), 200, resp, None, 2);
        assert!(s.truncated && !s.has_more);
    }

    #[test]
    fn other_responses() {
        let sql = shape(&req("POST /_sql"), 200, r#"{"columns": [{"name": "n", "type": "long"}], "rows": [[1], [2]], "cursor": "c"}"#, None, 100);
        assert_eq!((sql.columns[0].name.as_str(), sql.rows.len()), ("n", 2));
        assert!(sql.notices[0].contains("cursor"));
        let cat = shape(&req("GET /_cat/indices"), 200, r#"[{"index": "a", "docs.count": "3"}, {"index": "b"}]"#, None, 100);
        assert_eq!((cat.columns.len(), cat.rows[1][1].clone()), (2, Value::Null));
        let aggs = shape(&req("GET /a/_search"), 200, r#"{"hits": {"hits": []}, "aggregations": {"x": {"value": 1}}}"#, None, 100);
        assert_eq!((aggs.columns[0].name.as_str(), aggs.rows.len()), ("response", 1));
        let doc = shape(&req("PUT /a/_doc/1"), 201, r#"{"result": "created", "_id": "1"}"#, None, 100);
        assert_eq!(doc.affected, Some(1));
        assert_eq!(shape(&req("POST /a/_delete_by_query"), 200, r#"{"deleted": 7, "failures": []}"#, None, 100).affected, Some(7));
        assert_eq!(shape(&req("POST /_bulk"), 200, r#"{"items": [{}, {}]}"#, None, 100).affected, Some(2));
        let head = shape(&req("HEAD /a"), 404, "", None, 100);
        assert_eq!(head.rows, vec![vec![Value::Int(404)]]);
        let text = shape(&req("GET /_cat/health?format=txt"), 200, "green\n", None, 100);
        assert_eq!(text.rows, vec![vec![Value::Text("green\n".into())]]);
    }

    #[test]
    fn errors() {
        let e = http_error(404, r#"{"error": {"root_cause": [{"type": "index_not_found_exception", "reason": "no such index [x]"}], "type": "index_not_found_exception", "reason": "no such index [x]"}, "status": 404}"#);
        assert_eq!((e.code.as_deref(), e.message.as_str(), e.detail), (Some("index_not_found_exception"), "no such index [x]", None));
        let e = http_error(400, r#"{"error": {"type": "search_phase_execution_exception", "reason": "all shards failed", "caused_by": {"reason": "bad field"}}}"#);
        assert_eq!(e.detail.as_deref(), Some("bad field"));
        assert_eq!(http_error(502, "").message, "HTTP 502");
        assert_eq!(http_error(500, "boom").message, "boom");
    }

    #[test]
    fn mappings_flatten() {
        let v = json!({"b": {"mappings": {"properties": {"user": {"properties": {"name": {"type": "text"}}}, "age": {"type": "long"}}}}, ".hidden": {}, "a": {"mappings": {}}});
        assert_eq!(flatten_mappings(&v), vec![("a".into(), vec![]), ("b".into(), vec![("user".into(), "object".into()), ("user.name".into(), "text".into()), ("age".into(), "long".into())])]);
    }
}
