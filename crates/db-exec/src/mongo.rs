//! MongoDB through the official driver: one client per [`crate::Session`], statements from
//! `.mongodb.js` files ([`sql_split::mongo_parse`]) built into server commands
//! (`find`, `aggregate`, `insert`, `update`, `delete`, `findAndModify`…) and run on the session's
//! database. Documents become the same [`Outcome`] tables: columns in first-seen key order,
//! nested documents and arrays as JSON cells.
//!
//! Paging: a `find` without `.limit()` / `.skip()` gets `skip` / `limit = size + 1` (the extra
//! document says another page exists), the panel's sort as `sort` and its filters `$and`-ed to
//! the filter; an `aggregate` gets `$match` / `$sort` / `$skip` / `$limit` stages appended.
//! Cancel drops the command on the client; the server finishes its part (reads are bounded by
//! `maxTimeMS` from the statement timeout).
// ponytail: client-side cancel; `killOp` by `comment` if long commands must stop on the server too.

use std::sync::Arc;
use std::time::{Duration, Instant};

use bson::{Bson, Document, doc, oid::ObjectId};
use db_conn::{Connection, SslMode};
use mongodb::options::{ClientOptions, ServerAddress, Tls, TlsOptions};
use serde_json::Value as J;
use sql_split::{Kind, MongoCall};

use crate::{Column, DbError, Outcome, Page, Value, wrap::{Cond, Op, parse_filter}};

pub(crate) struct Mongo {
    client: mongodb::Client,
    /// The database statements run on: the connection's (or the url's), else `test`.
    pub db: String,
    /// Woken by [`crate::CancelHandle`]: the command in flight is dropped.
    pub abort: Arc<tokio::sync::Notify>,
    pub timeout: Option<Duration>,
}

/// `host:port` for messages: the fields over the url.
pub(crate) fn target(c: &Connection) -> String {
    match (&c.host, &c.url) {
        (Some(h), _) => format!("{h}:{}", c.port.unwrap_or(27017)),
        (None, Some(u)) => db_conn::without_password(u),
        (None, None) => format!("localhost:{}", c.port.unwrap_or(27017)),
    }
}

/// The driver's options: the url (SRV, replica set, `authSource`… all apply), then the fields
/// over it: host / port, user / password, SSL mode and files.
async fn options(c: &Connection) -> Result<ClientOptions, DbError> {
    let mut o = match &c.url {
        Some(u) => ClientOptions::parse(u).await.map_err(|e| error(e, c))?,
        None => ClientOptions::default(),
    };
    if c.host.is_some() || c.port.is_some() || c.url.is_none() {
        o.hosts = vec![ServerAddress::Tcp { host: c.host.clone().unwrap_or_else(|| "localhost".into()), port: Some(c.port.unwrap_or(27017)) }];
    }
    if c.user.is_some() || c.password.is_some() {
        let mut cred = o.credential.take().unwrap_or_default();
        if c.user.is_some() {
            cred.username = c.user.clone();
        }
        if c.password.is_some() {
            cred.password = c.password.clone();
        }
        o.credential = Some(cred);
    }
    if let Some(mode) = c.ssl.mode {
        o.tls = Some(match mode {
            SslMode::Disable => Tls::Disabled,
            m => {
                let mut t = TlsOptions::default();
                // `verify-ca` checks the hostname too: rustls has no CA-only mode.
                t.allow_invalid_certificates = Some(matches!(m, SslMode::Prefer | SslMode::Require));
                t.ca_file_path = c.ssl.ca.clone();
                // The driver reads the certificate and its key from one PEM file.
                t.cert_key_file_path = c.ssl.cert.clone();
                Tls::Enabled(t)
            }
        });
    }
    o.app_name = Some("siesta".into());
    Ok(o)
}

/// The driver's error as a [`DbError`]: server errors keep their code name.
fn error(e: mongodb::error::Error, c: &Connection) -> DbError {
    use mongodb::error::ErrorKind as K;
    let mut out = DbError::msg(e.to_string());
    match &*e.kind {
        K::Command(ce) => {
            out.message = ce.message.clone();
            out.code = Some(if ce.code_name.is_empty() { ce.code.to_string() } else { ce.code_name.clone() });
        }
        K::Authentication { message, .. } => {
            out.message = message.clone();
            out.code = Some("AuthenticationFailed".into());
        }
        K::Io(_) | K::ServerSelection { .. } | K::ConnectionPoolCleared { .. } | K::DnsResolve { .. } => out.connection_lost = true,
        _ => {}
    }
    let auth = matches!(&*e.kind, K::Authentication { .. }) || out.code.as_deref() == Some("AuthenticationFailed");
    if auth && !c.has_password() {
        out.message = format!("{}. No password is stored for `{1}`: set it in the connection form or with `siesta connections set-password {1}`", out.message, c.name);
    }
    out
}

/// A parsed argument as BSON (extended JSON: `{"$oid": …}` is an ObjectId).
fn bson(v: &J) -> Result<Bson, DbError> {
    Bson::try_from(v.clone()).map_err(|e| DbError::msg(format!("Invalid value {v}: {e}")))
}

/// Argument `i` as a document; a missing one is `{}`.
fn arg_doc(c: &MongoCall, i: usize) -> Result<Document, DbError> {
    match c.args.get(i) {
        None | Some(J::Null) => Ok(Document::new()),
        Some(v @ J::Object(_)) => match bson(v)? {
            Bson::Document(d) => Ok(d),
            other => Err(DbError::msg(format!("{}: argument {} must be a document, not {other}", c.label(), i + 1))),
        },
        Some(v) => Err(DbError::msg(format!("{}: argument {} must be a document, not {v}", c.label(), i + 1))),
    }
}

fn arg_str(c: &MongoCall, i: usize) -> Result<String, DbError> {
    c.args.get(i).and_then(J::as_str).map(str::to_string).ok_or_else(|| DbError::msg(format!("{}: argument {} must be a string", c.label(), i + 1)))
}

/// `extra`'s keys into `d` where `d` does not set them already: options never replace what the
/// statement itself built (a `pipeline` option can't swap the checked pipeline).
fn merge(d: &mut Document, extra: Document) {
    for (k, v) in extra {
        d.entry(k).or_insert(v);
    }
}

/// How a command's answer becomes rows.
enum Shape {
    /// A cursor: the documents. `limit`: how many to read (one more than shown).
    Cursor(usize),
    /// The answer document as one row (`cursor.firstBatch` as rows when it has one).
    Doc,
    /// The array at this key of the answer, as rows.
    List(&'static str),
    /// A count: the answer's `n`, or the `n` of a `$group` document from a cursor.
    Count,
    /// `distinct`: the `values` array as one column named after the field.
    Values(String),
    /// A write: `n` / `nModified` / `upserted`; `findAndModify`'s `value` as the row.
    Write,
}

struct Plan {
    admin: bool,
    cmd: Document,
    shape: Shape,
    /// Whether `cmd` pages the result on the server (`Shape::Cursor` reads `size + 1`).
    paged: bool,
}

/// The panel's filters as query conditions.
fn conditions(page: &Page) -> Vec<Document> {
    page.filters.iter().filter_map(|(col, text)| parse_filter(text).map(|c| doc! { col: condition(c) })).collect()
}

/// A typed value matches its own type only: `=42` looks for the string, the number, and an
/// ObjectId / boolean where the text is one.
fn candidates(v: &str) -> Vec<Bson> {
    let mut out = vec![Bson::String(v.to_string())];
    if let Ok(n) = v.parse::<i64>() {
        out.push(Bson::Int64(n));
    } else if let Ok(f) = v.parse::<f64>() {
        out.push(Bson::Double(f));
    }
    if let Ok(b) = v.parse::<bool>() {
        out.push(Bson::Boolean(b));
    }
    if let Ok(o) = ObjectId::parse_str(v) {
        out.push(Bson::ObjectId(o));
    }
    out
}

/// A bound for `>` / `<`: a number, a date (`2024-01-31`, RFC 3339), else the text.
fn scalar(v: &str) -> Bson {
    if let Ok(n) = v.parse::<i64>() {
        return Bson::Int64(n);
    }
    if let Ok(f) = v.parse::<f64>() {
        return Bson::Double(f);
    }
    let iso = if v.len() == 10 { format!("{v}T00:00:00Z") } else { v.to_string() };
    match bson::DateTime::parse_rfc3339_str(&iso) {
        Ok(d) => Bson::DateTime(d),
        Err(_) => Bson::String(v.to_string()),
    }
}

fn condition(c: Cond) -> Bson {
    let all = |vs: &[String]| Bson::Array(vs.iter().flat_map(|v| candidates(v)).collect());
    match c {
        Cond::Contains(v) => {
            let escaped: String = v.chars().flat_map(|c| if "\\^$.|?*+()[]{}".contains(c) { vec!['\\', c] } else { vec![c] }).collect();
            Bson::Document(doc! { "$regex": escaped, "$options": "i" })
        }
        Cond::Cmp(Op::Eq, v) => Bson::Document(doc! { "$in": all(&[v]) }),
        Cond::Cmp(Op::Ne, v) => Bson::Document(doc! { "$nin": all(&[v]) }),
        Cond::Cmp(op, v) => {
            let k = match op {
                Op::Gt => "$gt",
                Op::Ge => "$gte",
                Op::Lt => "$lt",
                _ => "$lte",
            };
            Bson::Document(doc! { k: scalar(&v) })
        }
        Cond::IsNull => Bson::Null,
        Cond::NotNull => Bson::Document(doc! { "$ne": Bson::Null }),
        Cond::In(vs) => Bson::Document(doc! { "$in": all(&vs) }),
    }
}

fn sort_doc(page: &Page) -> Document {
    page.sort.iter().map(|(c, desc)| (c.clone(), Bson::Int32(if *desc { -1 } else { 1 }))).collect()
}

/// `filter` with the panel's conditions `$and`-ed to it.
fn filtered(filter: Document, page: &Page) -> Document {
    let conds = conditions(page);
    if conds.is_empty() {
        return filter;
    }
    let mut all: Vec<Bson> = if filter.is_empty() { vec![] } else { vec![Bson::Document(filter)] };
    all.extend(conds.into_iter().map(Bson::Document));
    doc! { "$and": all }
}

/// The command a statement runs. `page` applies to `Kind::Query` statements only.
fn plan(c: &MongoCall, kind: Kind, page: &Page, max_rows: usize) -> Result<Plan, DbError> {
    let coll = c.collection.clone().unwrap_or_default();
    let one = |cmd: Document, shape: Shape| Ok(Plan { admin: false, cmd, shape, paged: false });
    let upto = max_rows.saturating_add(1);
    let explain = |cmd: Document, c: &MongoCall| -> Result<Plan, DbError> {
        let verbosity = c.chained("explain").and_then(|a| a.first()).and_then(J::as_str).unwrap_or("queryPlanner").to_string();
        one(doc! { "explain": cmd, "verbosity": verbosity }, Shape::Doc)
    };
    match (c.collection.is_some(), c.method.as_str()) {
        (true, "find" | "findOne") => {
            let filter = arg_doc(c, 0)?;
            let mut cmd = doc! { "find": &coll, "filter": filter.clone() };
            let projection = arg_doc(c, 1)?;
            if !projection.is_empty() {
                cmd.insert("projection", projection);
            }
            for (m, a) in &c.chain {
                let key = match m.as_str() {
                    "sort" | "limit" | "skip" | "projection" | "hint" | "collation" | "maxTimeMS" | "comment" | "allowDiskUse" => m.as_str(),
                    _ => continue,
                };
                let v = a.first().map(bson).transpose()?.unwrap_or(Bson::Boolean(true));
                cmd.insert(key, v);
            }
            merge(&mut cmd, arg_doc(c, 2)?);
            if c.chained("count").is_some() {
                let mut count = doc! { "count": &coll, "query": filter };
                for k in ["limit", "skip", "hint", "collation", "maxTimeMS"] {
                    if let Some(v) = cmd.get(k).filter(|_| k != "limit" && k != "skip") {
                        count.insert(k, v.clone());
                    }
                }
                return one(count, Shape::Count);
            }
            if c.chained("explain").is_some() {
                return explain(cmd, c);
            }
            if c.method == "findOne" {
                cmd.insert("limit", 1);
                cmd.insert("singleBatch", true);
                return one(cmd, Shape::Cursor(1));
            }
            let paged = kind == Kind::Query && !cmd.contains_key("limit") && !cmd.contains_key("skip");
            if !paged {
                return one(cmd, Shape::Cursor(upto));
            }
            cmd.insert("filter", filtered(filter, page));
            if !page.sort.is_empty() {
                cmd.insert("sort", sort_doc(page));
            }
            cmd.insert("skip", page.offset as i64);
            cmd.insert("limit", page.size as i64 + 1);
            Ok(Plan { admin: false, cmd, shape: Shape::Cursor(page.size as usize + 1), paged: true })
        }
        (true, "aggregate") => {
            let mut pipeline = match c.args.first().map(bson).transpose()? {
                Some(Bson::Array(a)) => a,
                None => vec![],
                Some(_) => return Err(DbError::msg("aggregate takes an array of stages")),
            };
            let paged = kind == Kind::Query;
            if paged {
                pipeline.extend(conditions(page).into_iter().map(|m| Bson::Document(doc! { "$match": m })));
                if !page.sort.is_empty() {
                    pipeline.push(Bson::Document(doc! { "$sort": sort_doc(page) }));
                }
                if page.offset > 0 {
                    pipeline.push(Bson::Document(doc! { "$skip": page.offset as i64 }));
                }
                pipeline.push(Bson::Document(doc! { "$limit": page.size as i64 + 1 }));
            }
            let mut cmd = doc! { "aggregate": &coll, "pipeline": pipeline, "cursor": {} };
            merge(&mut cmd, arg_doc(c, 1)?);
            if c.chained("explain").is_some() {
                cmd.remove("cursor");
                return explain(cmd, c);
            }
            let limit = if paged { page.size as usize + 1 } else { upto };
            Ok(Plan { admin: false, cmd, shape: Shape::Cursor(limit), paged })
        }
        (true, "countDocuments") => {
            let mut pipeline = vec![Bson::Document(doc! { "$match": arg_doc(c, 0)? })];
            let opts = arg_doc(c, 1)?;
            for k in ["skip", "limit"] {
                if let Some(v) = opts.get(k) {
                    pipeline.push(Bson::Document(doc! { format!("${k}"): v.clone() }));
                }
            }
            pipeline.push(Bson::Document(doc! { "$group": { "_id": 1, "n": { "$sum": 1 } } }));
            one(doc! { "aggregate": &coll, "pipeline": pipeline, "cursor": {} }, Shape::Count)
        }
        (true, "estimatedDocumentCount") => one(doc! { "count": &coll }, Shape::Count),
        (true, "count") => one(doc! { "count": &coll, "query": arg_doc(c, 0)? }, Shape::Count),
        (true, "distinct") => {
            let key = arg_str(c, 0)?;
            one(doc! { "distinct": &coll, "key": &key, "query": arg_doc(c, 1)? }, Shape::Values(key))
        }
        (true, "getIndexes") => one(doc! { "listIndexes": &coll }, Shape::Cursor(upto)),
        (true, "stats") => one(doc! { "collStats": &coll }, Shape::Doc),
        (true, "insertOne" | "insertMany") => {
            let docs: Vec<J> = match (c.method.as_str(), c.args.first()) {
                ("insertOne", Some(d)) => vec![d.clone()],
                ("insertMany", Some(J::Array(a))) => a.clone(),
                _ => return Err(DbError::msg(format!("{}: expected {}", c.label(), if c.method == "insertOne" { "a document" } else { "an array of documents" }))),
            };
            let mut out = Vec::new();
            for d in &docs {
                let Bson::Document(mut d) = bson(d)? else { return Err(DbError::msg(format!("{}: only documents can be inserted", c.label()))) };
                // The id is made here, as drivers do, so the answer can show it.
                if !d.contains_key("_id") {
                    let mut with = doc! { "_id": ObjectId::new() };
                    with.extend(d);
                    d = with;
                }
                out.push(Bson::Document(d));
            }
            let mut cmd = doc! { "insert": &coll, "documents": out };
            merge(&mut cmd, arg_doc(c, 1)?);
            one(cmd, Shape::Write)
        }
        (true, "updateOne" | "updateMany" | "replaceOne") => {
            let u = c.args.get(1).map(bson).transpose()?.ok_or_else(|| DbError::msg(format!("{}: expected a filter and an update", c.label())))?;
            let mut entry = doc! { "q": arg_doc(c, 0)?, "u": u, "multi": c.method == "updateMany" };
            let mut cmd = doc! { "update": &coll };
            for (k, v) in arg_doc(c, 2)? {
                if ["upsert", "arrayFilters", "collation", "hint"].contains(&k.as_str()) {
                    entry.entry(k).or_insert(v);
                } else {
                    cmd.entry(k).or_insert(v);
                }
            }
            cmd.insert("updates", vec![entry]);
            one(cmd, Shape::Write)
        }
        (true, "deleteOne" | "deleteMany") => {
            let mut entry = doc! { "q": arg_doc(c, 0)?, "limit": i32::from(c.method == "deleteOne") };
            let mut cmd = doc! { "delete": &coll };
            for (k, v) in arg_doc(c, 1)? {
                if ["collation", "hint"].contains(&k.as_str()) {
                    entry.entry(k).or_insert(v);
                } else {
                    cmd.entry(k).or_insert(v);
                }
            }
            cmd.insert("deletes", vec![entry]);
            one(cmd, Shape::Write)
        }
        (true, "findOneAndUpdate" | "findOneAndReplace" | "findOneAndDelete") => {
            let delete = c.method == "findOneAndDelete";
            let mut cmd = doc! { "findAndModify": &coll, "query": arg_doc(c, 0)? };
            let opts = arg_doc(c, if delete { 1 } else { 2 })?;
            if delete {
                cmd.insert("remove", true);
            } else {
                let u = c.args.get(1).map(bson).transpose()?.ok_or_else(|| DbError::msg(format!("{}: expected a filter and an update", c.label())))?;
                cmd.insert("update", u);
                let after = opts.get_str("returnDocument").is_ok_and(|r| r == "after") || opts.get_bool("returnNewDocument").unwrap_or(false);
                cmd.insert("new", after);
            }
            for (k, v) in opts {
                let k = match k.as_str() {
                    "projection" => "fields".to_string(),
                    "returnDocument" | "returnNewDocument" => continue,
                    _ => k,
                };
                cmd.entry(k).or_insert(v);
            }
            one(cmd, Shape::Write)
        }
        (true, "createIndex") => {
            let keys = arg_doc(c, 0)?;
            let mut index = arg_doc(c, 1)?;
            if !index.contains_key("name") {
                let name: Vec<String> = keys.iter().map(|(k, v)| format!("{k}_{}", v.as_str().map_or_else(|| v.to_string(), str::to_string))).collect();
                index.insert("name", name.join("_"));
            }
            index.insert("key", keys);
            one(doc! { "createIndexes": &coll, "indexes": [index] }, Shape::Write)
        }
        (true, "dropIndex" | "dropIndexes") => {
            let index = c.args.first().map(bson).transpose()?.unwrap_or_else(|| Bson::String("*".into()));
            one(doc! { "dropIndexes": &coll, "index": index }, Shape::Write)
        }
        (true, "drop") => one(doc! { "drop": &coll }, Shape::Write),
        (true, "renameCollection") => {
            // Answered on `admin` with full names; set in `run`, which knows the database.
            let to = arg_str(c, 0)?;
            let drop_target = c.args.get(1).and_then(J::as_bool).unwrap_or(false);
            Ok(Plan { admin: true, cmd: doc! { "renameCollection": &coll, "to": to, "dropTarget": drop_target }, shape: Shape::Write, paged: false })
        }
        (false, "runCommand" | "adminCommand") => {
            let cmd = match c.args.first() {
                Some(J::String(s)) => doc! { s: 1 },
                _ => arg_doc(c, 0)?,
            };
            let shape = if kind == Kind::Other { Shape::Write } else { Shape::Doc };
            Ok(Plan { admin: c.method == "adminCommand", cmd, shape, paged: false })
        }
        (false, "getCollectionNames") => one(doc! { "listCollections": 1, "nameOnly": true, "authorizedCollections": true }, Shape::Cursor(upto)),
        (false, "getCollectionInfos") => one(doc! { "listCollections": 1, "filter": arg_doc(c, 0)? }, Shape::Cursor(upto)),
        (false, "createCollection") => {
            let mut cmd = doc! { "create": arg_str(c, 0)? };
            merge(&mut cmd, arg_doc(c, 1)?);
            one(cmd, Shape::Write)
        }
        (false, "dropDatabase") => one(doc! { "dropDatabase": 1 }, Shape::Write),
        (false, "stats") => one(doc! { "dbStats": 1 }, Shape::Doc),
        (false, "version") => Ok(Plan { admin: true, cmd: doc! { "buildInfo": 1 }, shape: Shape::Doc, paged: false }),
        (false, "serverStatus") => one(doc! { "serverStatus": 1 }, Shape::Doc),
        (false, "show") => match c.args.first().and_then(J::as_str).unwrap_or("") {
            "dbs" | "databases" => Ok(Plan { admin: true, cmd: doc! { "listDatabases": 1 }, shape: Shape::List("databases"), paged: false }),
            "users" => one(doc! { "usersInfo": 1 }, Shape::List("users")),
            _ => one(doc! { "listCollections": 1, "nameOnly": true, "authorizedCollections": true }, Shape::Cursor(upto)),
        },
        _ => Err(DbError::msg(format!("{} is not supported", c.label()))),
    }
}

impl Mongo {
    /// Builds the client and checks the server answers `ping` (address, TLS, credentials).
    pub(crate) async fn open(c: &Connection) -> Result<Mongo, DbError> {
        let o = options(c).await?;
        let db = c.database.clone().filter(|d| !d.is_empty()).or_else(|| o.default_database.clone()).unwrap_or_else(|| "test".into());
        let client = mongodb::Client::with_options(o).map_err(|e| error(e, c))?;
        let m = Mongo { client, db, abort: Arc::default(), timeout: None };
        m.client.database(&m.db).run_command(doc! { "ping": 1 }).await.map_err(|e| error(e, c))?;
        Ok(m)
    }

    fn database(&self, admin: bool) -> mongodb::Database {
        self.client.database(if admin { "admin" } else { &self.db })
    }

    /// `f` raced against a cancel and the statement timeout.
    async fn guarded<T>(&self, f: impl IntoFuture<Output = Result<T, mongodb::error::Error>>, c: &Connection) -> Result<T, DbError> {
        let timeout = async {
            match self.timeout {
                Some(t) => tokio::time::sleep(t).await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            r = f.into_future() => r.map_err(|e| error(e, c)),
            _ = self.abort.notified() => Err(DbError::msg("Cancelled (the command was dropped; MongoDB may still finish it)")),
            _ = timeout => Err(DbError::msg("No answer within the statement timeout (run.statement_timeout_ms)")),
        }
    }

    /// Runs a command; a cursor's documents up to `limit`, else the answer document.
    async fn command(&self, p: &Plan, limit: Option<usize>, c: &Connection) -> Result<(Option<Vec<Document>>, Document), DbError> {
        let db = self.database(p.admin);
        let mut cmd = p.cmd.clone();
        if let (Some(t), Some(name)) = (self.timeout, cmd.keys().next().cloned())
            && ["find", "aggregate", "count", "distinct"].contains(&name.as_str())
        {
            cmd.entry("maxTimeMS".to_string()).or_insert(Bson::Int64(t.as_millis() as i64));
        }
        match limit {
            Some(n) => {
                let docs = self
                    .guarded(
                        async {
                            let mut cur = db.run_cursor_command(cmd).await?;
                            let mut out = Vec::new();
                            while out.len() < n && cur.advance().await? {
                                out.push(cur.deserialize_current()?);
                            }
                            Ok(out)
                        },
                        c,
                    )
                    .await?;
                Ok((Some(docs), Document::new()))
            }
            None => Ok((None, self.guarded(db.run_command(cmd), c).await?)),
        }
    }

    pub(crate) async fn run(&self, text: &str, kind: Kind, page: &Page, max_rows: usize, c: &Connection) -> Result<Outcome, DbError> {
        let started = Instant::now();
        let call = sql_split::mongo_parse(text).map_err(|(_, m)| DbError::msg(m))?;
        // The safety checks classified this same parse; a mismatch means the caller's kind is stale.
        if sql_split::mongo_kind(&call) != kind {
            return Err(DbError { message: format!("Refused: {} runs as another kind of statement than it was checked as", call.label()), refused: true, ..Default::default() });
        }
        let mut p = plan(&call, kind, page, max_rows)?;
        if call.method == "renameCollection" {
            let (from, to) = (format!("{}.{}", self.db, p.cmd.get_str("renameCollection").unwrap_or("")), format!("{}.{}", self.db, p.cmd.get_str("to").unwrap_or("")));
            p.cmd.insert("renameCollection", from);
            p.cmd.insert("to", to);
        }
        let limit = match p.shape {
            Shape::Cursor(n) => Some(n),
            Shape::Count if p.cmd.contains_key("aggregate") => Some(1),
            _ => None,
        };
        let (docs, answer) = self.command(&p, limit, c).await?;
        let mut notices = Vec::new();
        let mut s = Shaped::default();
        match (&p.shape, docs) {
            (Shape::Count, Some(docs)) => s.count(docs.first().map_or(0, |d| number(d.get("n")))),
            (Shape::Count, None) => s.count(number(answer.get("n"))),
            (Shape::Cursor(_), Some(docs)) => {
                let shown = if p.paged { page.size as usize } else { max_rows };
                let cut = docs.len() > shown;
                s.table(&docs[..docs.len().min(shown)]);
                if p.paged { s.has_more = cut } else { s.truncated = cut }
                s.raw = Some(J::Array(docs.into_iter().take(shown).map(|d| Bson::Document(d).into_relaxed_extjson()).collect()));
            }
            (Shape::Values(key), _) => {
                let values = answer.get_array("values").cloned().unwrap_or_default();
                s.truncated = values.len() > max_rows;
                s.columns = vec![Column { name: key.clone(), type_name: values.first().map_or("null", type_of).into() }];
                s.rows = values.iter().take(max_rows).map(|v| vec![value(v)]).collect();
                s.raw = Some(Bson::Document(answer).into_relaxed_extjson());
            }
            (Shape::List(key), _) => {
                let items: Vec<Document> = answer.get_array(key).map(|a| a.iter().filter_map(|v| v.as_document().cloned()).collect()).unwrap_or_default();
                s.truncated = items.len() > max_rows;
                s.table(&items[..items.len().min(max_rows)]);
                s.raw = Some(Bson::Document(answer).into_relaxed_extjson());
            }
            (Shape::Write, _) => {
                write_errors(&answer)?;
                let n = number(answer.get("n"));
                s.affected = Some(match p.cmd.keys().next().map(String::as_str) {
                    Some("update") => number(answer.get("nModified")) + answer.get_array("upserted").map_or(0, |u| u.len() as u64),
                    Some("insert" | "delete") => n,
                    Some("findAndModify") => number(answer.get_document("lastErrorObject").ok().and_then(|l| l.get("n"))),
                    _ => 0,
                });
                if let Ok(v) = answer.get_document("value") {
                    s.table(std::slice::from_ref(v));
                }
                let mut raw = answer;
                if let Ok(docs) = p.cmd.get_array("documents") {
                    raw.insert("insertedIds", docs.iter().filter_map(|d| d.as_document().and_then(|d| d.get("_id")).cloned()).collect::<Vec<_>>());
                }
                s.raw = Some(Bson::Document(raw).into_relaxed_extjson());
            }
            (_, _) => {
                // One answer document: a cursor's first batch as rows, else the answer itself.
                write_errors(&answer)?;
                match answer.get_document("cursor").ok().and_then(|cur| cur.get_array("firstBatch").ok()) {
                    Some(batch) => {
                        let docs: Vec<Document> = batch.iter().filter_map(|v| v.as_document().cloned()).collect();
                        s.table(&docs[..docs.len().min(max_rows)]);
                        s.truncated = docs.len() > max_rows;
                        if answer.get_document("cursor").ok().and_then(|cur| cur.get_i64("id").ok()).is_some_and(|id| id != 0) {
                            notices.push("More documents: the command left a cursor open (only the first batch is shown).".into());
                        }
                    }
                    None => s.table(std::slice::from_ref(&answer)),
                }
                s.raw = Some(Bson::Document(answer).into_relaxed_extjson());
            }
        }
        if kind == Kind::Query && !p.paged {
            notices.push("The statement sets .limit() / .skip(), so it runs as written: no paging, sorting is local, filters are off.".into());
        }
        Ok(Outcome {
            columns: s.columns,
            rows: s.rows,
            wrapped: p.paged,
            has_more: s.has_more,
            truncated: s.truncated,
            rows_affected: (kind == Kind::Other).then_some(s.affected.unwrap_or(0)),
            notices,
            executed_sql: format!("db.getSiblingDB({:?}).runCommand({})", if p.admin { "admin" } else { &self.db }, Bson::Document(p.cmd).into_relaxed_extjson()),
            params: Vec::new(),
            in_transaction: false,
            raw: s.raw,
            duration: started.elapsed(),
        })
    }

    /// The number of documents a `find` / `aggregate` statement gives with the panel's filters.
    pub(crate) async fn count(&self, text: &str, filters: &[(String, String)], c: &Connection) -> Result<u64, DbError> {
        let call = sql_split::mongo_parse(text).map_err(|(_, m)| DbError::msg(m))?;
        let page = Page { filters: filters.to_vec(), ..Default::default() };
        let plan = match call.method.as_str() {
            "find" => Plan { admin: false, cmd: doc! { "count": call.collection.clone().unwrap_or_default(), "query": filtered(arg_doc(&call, 0)?, &page) }, shape: Shape::Count, paged: false },
            "aggregate" if sql_split::mongo_kind(&call) == Kind::Query => {
                let Some(Bson::Array(mut pipeline)) = call.args.first().map(bson).transpose()? else { return Err(DbError::msg("aggregate takes an array of stages")) };
                pipeline.extend(conditions(&page).into_iter().map(|m| Bson::Document(doc! { "$match": m })));
                pipeline.push(Bson::Document(doc! { "$count": "n" }));
                Plan { admin: false, cmd: doc! { "aggregate": call.collection.clone().unwrap_or_default(), "pipeline": pipeline, "cursor": {} }, shape: Shape::Count, paged: false }
            }
            _ => return Err(DbError::msg(format!("Cannot count {}", call.label()))),
        };
        let limit = plan.cmd.contains_key("aggregate").then_some(1);
        let (docs, answer) = self.command(&plan, limit, c).await?;
        Ok(match docs {
            Some(d) => d.first().map_or(0, |d| number(d.get("n"))),
            None => number(answer.get("n")),
        })
    }

    /// Database names (`listDatabases`), sorted.
    pub(crate) async fn databases(&self, c: &Connection) -> Result<Vec<String>, DbError> {
        let answer = self.guarded(self.database(true).run_command(doc! { "listDatabases": 1, "nameOnly": true }), c).await?;
        let mut names: Vec<String> = answer.get_array("databases").into_iter().flatten().filter_map(|d| d.as_document()?.get_str("name").ok().map(str::to_string)).collect();
        names.sort();
        Ok(names)
    }

    /// Collections of the database (views as views) with fields seen in a few documents
    /// (dotted paths into embedded documents) and their indexes.
    pub(crate) async fn collections(&self, c: &Connection) -> Result<Vec<crate::Table>, DbError> {
        let list =self.command(&Plan { admin: false, cmd: doc! { "listCollections": 1, "authorizedCollections": true }, shape: Shape::Cursor(usize::MAX), paged: false }, Some(usize::MAX), c).await?.0.unwrap_or_default();
        let mut out = Vec::new();
        for info in list {
            let Ok(name) = info.get_str("name").map(str::to_string) else { continue };
            if name.starts_with("system.") {
                continue;
            }
            let view = info.get_str("type").ok() == Some("view");
            // ponytail: fields from 20 documents; a field only rarer documents have is missed.
            let sample = self.command(&Plan { admin: false, cmd: doc! { "find": &name, "limit": 20 }, shape: Shape::Cursor(20), paged: false }, Some(20), c).await.map(|r| r.0.unwrap_or_default()).unwrap_or_default();
            let mut fields: Vec<(String, String)> = Vec::new();
            for d in &sample {
                walk("", d, &mut fields);
            }
            let indexes = if view {
                Vec::new()
            } else {
                self.command(&Plan { admin: false, cmd: doc! { "listIndexes": &name }, shape: Shape::Cursor(usize::MAX), paged: false }, Some(usize::MAX), c).await.map(|r| r.0.unwrap_or_default()).unwrap_or_default()
            };
            let indexes: Vec<crate::Index> = indexes
                .iter()
                .map(|i| crate::Index {
                    name: i.get_str("name").unwrap_or("").to_string(),
                    columns: i.get_document("key").map(|k| k.keys().cloned().collect()).unwrap_or_default(),
                    unique: i.get_bool("unique").unwrap_or(false) || i.get_str("name").ok() == Some("_id_"),
                    primary: i.get_str("name").ok() == Some("_id_"),
                })
                .collect();
            let columns = fields
                .into_iter()
                .enumerate()
                .map(|(i, (name, data_type))| crate::ColumnInfo { primary_key: name == "_id", name, data_type, nullable: true, ordinal: i as u32 + 1 })
                .collect();
            out.push(crate::Table { name, kind: if view { crate::TableKind::View } else { crate::TableKind::Table }, columns, indexes });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// A collection as statements that recreate it: `createCollection` with its options, then
    /// `createIndex` for every index but `_id_`.
    pub(crate) async fn ddl(&self, name: &str, c: &Connection) -> Result<String, DbError> {
        let infos = self.command(&Plan { admin: false, cmd: doc! { "listCollections": 1, "filter": { "name": name } }, shape: Shape::Cursor(1), paged: false }, Some(1), c).await?.0.unwrap_or_default();
        let info = infos.first().ok_or_else(|| DbError::msg(format!("no such collection: {name}")))?;
        let reference = if name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') && !name.starts_with(|c: char| c.is_ascii_digit()) { format!("db.{name}") } else { format!("db.getCollection({name:?})") };
        let options = info.get_document("options").cloned().unwrap_or_default();
        let mut out = if options.is_empty() { format!("db.createCollection({name:?})") } else { format!("db.createCollection({name:?}, {})", Bson::Document(options).into_relaxed_extjson()) };
        if info.get_str("type").ok() != Some("view") {
            let indexes = self.command(&Plan { admin: false, cmd: doc! { "listIndexes": name }, shape: Shape::Cursor(usize::MAX), paged: false }, Some(usize::MAX), c).await?.0.unwrap_or_default();
            for mut i in indexes.into_iter().filter(|i| i.get_str("name").ok() != Some("_id_")) {
                let key = i.remove("key").unwrap_or(Bson::Document(Document::new()));
                i.remove("v");
                out += &format!("\n{reference}.createIndex({}, {})", key.into_relaxed_extjson(), Bson::Document(i).into_relaxed_extjson());
            }
        }
        Ok(out)
    }
}

/// A write answered `ok: 1` but with `writeErrors` / `writeConcernError`: an error all the same.
fn write_errors(answer: &Document) -> Result<(), DbError> {
    let first = answer.get_array("writeErrors").ok().and_then(|e| e.first()).and_then(Bson::as_document).or_else(|| answer.get_document("writeConcernError").ok());
    match first {
        None => Ok(()),
        Some(e) => {
            let mut out = DbError::msg(e.get_str("errmsg").unwrap_or("write error"));
            out.code = e.get_str("codeName").ok().map(str::to_string).or_else(|| e.get("code").map(|c| c.to_string()));
            let n = number(answer.get("n"));
            if n > 0 {
                out.detail = Some(format!("{n} document(s) were written before the error"));
            }
            Err(out)
        }
    }
}

fn number(v: Option<&Bson>) -> u64 {
    match v {
        Some(Bson::Int32(n)) => *n as u64,
        Some(Bson::Int64(n)) => *n as u64,
        Some(Bson::Double(f)) => *f as u64,
        _ => 0,
    }
}

/// Fields of a document with dotted paths into embedded documents (not into arrays).
fn walk(prefix: &str, d: &Document, out: &mut Vec<(String, String)>) {
    for (k, v) in d {
        let name = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
        match out.iter_mut().find(|(n, _)| *n == name) {
            Some(f) if f.1 == "null" => f.1 = type_of(v).into(),
            Some(_) => {}
            None => out.push((name.clone(), type_of(v).into())),
        }
        if let Bson::Document(inner) = v {
            walk(&name, inner, out);
        }
    }
}

pub(crate) fn value(v: &Bson) -> Value {
    match v {
        Bson::Null | Bson::Undefined => Value::Null,
        Bson::Boolean(b) => Value::Bool(*b),
        Bson::Int32(n) => Value::Int(*n as i64),
        Bson::Int64(n) => Value::Int(*n),
        Bson::Double(f) => Value::Float(*f),
        Bson::String(s) => Value::Text(s.clone()),
        Bson::Decimal128(d) => Value::Decimal(d.to_string()),
        Bson::ObjectId(o) => Value::Other { type_name: "objectId".into(), text: o.to_hex() },
        Bson::DateTime(d) => Value::DateTime(d.try_to_rfc3339_string().unwrap_or_else(|_| d.to_string())),
        other => Value::Json(other.clone().into_relaxed_extjson()),
    }
}

/// mongosh's type names (`$type`).
fn type_of(v: &Bson) -> &'static str {
    match v {
        Bson::Double(_) => "double",
        Bson::String(_) => "string",
        Bson::Document(_) => "object",
        Bson::Array(_) => "array",
        Bson::Binary(_) => "binData",
        Bson::ObjectId(_) => "objectId",
        Bson::Boolean(_) => "bool",
        Bson::DateTime(_) => "date",
        Bson::Null | Bson::Undefined => "null",
        Bson::RegularExpression(_) => "regex",
        Bson::Int32(_) => "int",
        Bson::Timestamp(_) => "timestamp",
        Bson::Int64(_) => "long",
        Bson::Decimal128(_) => "decimal",
        _ => "other",
    }
}

#[derive(Default)]
struct Shaped {
    columns: Vec<Column>,
    rows: Vec<Vec<Value>>,
    has_more: bool,
    truncated: bool,
    affected: Option<u64>,
    raw: Option<J>,
}

impl Shaped {
    /// Documents as a table: columns in first-seen key order, typed by the first non-null value.
    fn table(&mut self, docs: &[Document]) {
        let mut cols: Vec<(String, &'static str)> = Vec::new();
        for d in docs {
            for (k, v) in d {
                match cols.iter_mut().find(|(n, _)| n == k) {
                    Some(c) if c.1 == "null" => c.1 = type_of(v),
                    Some(_) => {}
                    None => cols.push((k.clone(), type_of(v))),
                }
            }
        }
        self.rows = docs.iter().map(|d| cols.iter().map(|(n, _)| d.get(n).map_or(Value::Null, value)).collect()).collect();
        self.columns = cols.into_iter().map(|(name, t)| Column { name, type_name: t.into() }).collect();
    }

    fn count(&mut self, n: u64) {
        self.columns = vec![Column { name: "count".into(), type_name: "long".into() }];
        self.rows = vec![vec![Value::Int(n as i64)]];
        self.raw = Some(serde_json::json!({ "count": n }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(text: &str) -> MongoCall {
        sql_split::mongo_parse(text).unwrap()
    }

    fn planned(text: &str, page: &Page) -> Plan {
        let c = call(text);
        plan(&c, sql_split::mongo_kind(&c), page, 1000).unwrap()
    }

    #[test]
    fn find_is_paged_with_sort_and_filters() {
        let page = Page { offset: 100, size: 50, sort: vec![("age".into(), true)], filters: vec![("name".into(), "ann".into()), ("age".into(), ">=18".into()), ("x".into(), "".into())] };
        let p = planned("db.users.find({active: true}, {name: 1})", &page);
        assert!(p.paged);
        assert_eq!(
            p.cmd,
            doc! { "find": "users", "filter": { "$and": [{ "active": true }, { "name": { "$regex": "ann", "$options": "i" } }, { "age": { "$gte": 18_i64 } }] },
                "projection": { "name": 1 }, "sort": { "age": -1 }, "skip": 100_i64, "limit": 51_i64 }
        );
        // `.limit()` runs as written.
        let p = planned("db.users.find().sort({a: 1}).limit(5)", &page);
        assert!(!p.paged);
        assert_eq!(p.cmd, doc! { "find": "users", "filter": {}, "sort": { "a": 1 }, "limit": 5 });
        // Options never replace what the statement built.
        let p = planned("db.users.find({a: 1}, {}, {filter: {b: 2}, maxTimeMS: 5})", &Page::default());
        assert_eq!(p.cmd.get_document("filter").unwrap(), &doc! { "a": 1 });
        assert_eq!(p.cmd.get_i32("maxTimeMS").unwrap(), 5);
    }

    #[test]
    fn aggregate_gets_stages_appended() {
        let page = Page { offset: 10, size: 5, sort: vec![("n".into(), false)], filters: vec![("_id".into(), "=65a1b2c3d4e5f6a7b8c9d0e1".into())] };
        let p = planned("db.o.aggregate([{$group: {_id: '$c', n: {$sum: 1}}}], {pipeline: [{$out: 'x'}]})", &Page::default());
        // An `$out` anywhere makes it a write; the parse is the same for the checks and here.
        assert!(!p.paged);
        let p = planned("db.o.aggregate([{$group: {_id: '$c', n: {$sum: 1}}}])", &page);
        let stages: Vec<String> = p.cmd.get_array("pipeline").unwrap().iter().map(|s| s.as_document().unwrap().keys().next().unwrap().clone()).collect();
        assert_eq!(stages, ["$group", "$match", "$sort", "$skip", "$limit"]);
        let m = p.cmd.get_array("pipeline").unwrap()[1].as_document().unwrap().get_document("$match").unwrap().get_document("_id").unwrap().get_array("$in").unwrap().clone();
        assert!(m.iter().any(|v| matches!(v, Bson::ObjectId(_))) && m.iter().any(|v| matches!(v, Bson::String(_))), "{m:?}");
    }

    #[test]
    fn writes_become_commands() {
        let p = planned("db.u.updateMany({a: 1}, {$set: {b: 2}}, {upsert: true, writeConcern: {w: 1}, updates: [{q: {}, u: {}, multi: true}]})", &Page::default());
        // An `updates` option can't replace the checked statement.
        assert_eq!(p.cmd, doc! { "update": "u", "writeConcern": { "w": 1 }, "updates": [{ "q": { "a": 1 }, "u": { "$set": { "b": 2 } }, "multi": true, "upsert": true }] });
        let p = planned("db.u.deleteOne({_id: ObjectId('65a1b2c3d4e5f6a7b8c9d0e1')})", &Page::default());
        assert_eq!(p.cmd.get_array("deletes").unwrap()[0].as_document().unwrap().get_i32("limit").unwrap(), 1);
        let p = planned("db.u.insertOne({a: 1})", &Page::default());
        let d = p.cmd.get_array("documents").unwrap()[0].as_document().unwrap().clone();
        assert_eq!(d.keys().collect::<Vec<_>>(), ["_id", "a"]);
        let p = planned("db.u.createIndex({a: 1, b: -1}, {unique: true})", &Page::default());
        assert_eq!(p.cmd, doc! { "createIndexes": "u", "indexes": [{ "unique": true, "name": "a_1_b_-1", "key": { "a": 1, "b": -1 } }] });
        let p = planned("db.u.findOneAndUpdate({a: 1}, {$inc: {n: 1}}, {returnDocument: 'after', projection: {n: 1}})", &Page::default());
        assert_eq!(p.cmd, doc! { "findAndModify": "u", "query": { "a": 1 }, "update": { "$inc": { "n": 1 } }, "new": true, "fields": { "n": 1 } });
        assert_eq!(planned("db.runCommand('ping')", &Page::default()).cmd, doc! { "ping": 1 });
    }

    #[test]
    fn documents_become_rows() {
        let mut s = Shaped::default();
        let oid = ObjectId::parse_str("65a1b2c3d4e5f6a7b8c9d0e1").unwrap();
        s.table(&[doc! { "_id": oid, "n": Bson::Null, "tags": ["a"] }, doc! { "n": 2_i64, "sub": { "x": 1 } }]);
        let cols: Vec<(&str, &str)> = s.columns.iter().map(|c| (c.name.as_str(), c.type_name.as_str())).collect();
        assert_eq!(cols, [("_id", "objectId"), ("n", "long"), ("tags", "array"), ("sub", "object")]);
        assert_eq!(s.rows[0][0], Value::Other { type_name: "objectId".into(), text: "65a1b2c3d4e5f6a7b8c9d0e1".into() });
        assert_eq!(s.rows[1][3], Value::Json(serde_json::json!({"x": 1})));
        let mut fields = Vec::new();
        walk("", &doc! { "a": { "b": 1, "c": { "d": "x" } }, "e": [1] }, &mut fields);
        let names: Vec<&str> = fields.iter().map(|f| f.0.as_str()).collect();
        assert_eq!(names, ["a", "a.b", "a.c", "a.c.d", "e"]);
        let e = write_errors(&doc! { "ok": 1, "n": 1, "writeErrors": [{ "index": 1, "code": 11000, "codeName": "DuplicateKey", "errmsg": "E11000 duplicate key" }] }).unwrap_err();
        assert_eq!((e.code.as_deref(), e.message.as_str(), e.detail.as_deref()), (Some("DuplicateKey"), "E11000 duplicate key", Some("1 document(s) were written before the error")));
    }
}
