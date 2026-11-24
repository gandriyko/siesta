//! Connections: one JSON file per connection in `<config dir>/connections/<name>.json`
//! (`~/.config/siesta` unless `SIESTA_CONFIG_DIR` says otherwise); the file name is the connection name.
//!
//! A file holds driver, url | host / socket / port / database / user, ssl, read_only, prod, schema, init_sql, color.
//! Values may use `${env:NAME}`. The password lives only as `password_enc` (encrypted with the local
//! key `<data dir>/key`, see [`crypto`]), decrypted when a session opens.
//! A broken connection lands in `errors`, a suspicious key in `warnings`; the rest loads.
//!
//! The folders come from `siesta_settings::{config_dir, data_dir}`; functions here take them as
//! arguments (`config`: where the files are, `keys`: where `key` is), so tests use temp dirs.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

mod global;
pub mod crypto;
pub use global::{ConnectionForm, delete, has_password, migrate_names, names, read_form, save, set_password};
mod urlparse;
pub use urlparse::{ParsedUrl, parse_url};

const KEYS: [&str; 14] = ["driver", "url", "host", "port", "database", "user", "ssl", "read_only", "prod", "schema", "init_sql", "color", "password_enc", "socket"];
const SSL_KEYS: [&str; 4] = ["mode", "ca", "cert", "key"];

/// The folder of the per-connection files inside the config dir.
pub fn connections_dir(config: &Path) -> PathBuf {
    config.join("connections")
}

/// The longest connection name.
pub const MAX_NAME: usize = 64;

/// A connection name for saving: `[a-z0-9][a-z0-9_-]*`, at most [`MAX_NAME`] characters (it is a
/// file name, the value of `-- @connection` and a CLI argument). [`normalize_name`] produces one.
pub fn valid_name(name: &str) -> Result<(), String> {
    let ok = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_';
    if name.is_empty() {
        Err("name is required".into())
    } else if name.chars().count() > MAX_NAME || !name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit()) || !name.chars().all(ok) {
        Err(format!("invalid connection name '{name}': lowercase latin letters, digits, - and _ (not first), up to {MAX_NAME} characters"))
    } else {
        Ok(())
    }
}

/// The lax check for names of files that already exist (legacy names keep loading, reading,
/// deleting): no path separators, no leading dot, no control characters, no surrounding spaces.
pub fn valid_file_name(name: &str) -> Result<(), String> {
    if name.trim().is_empty() {
        Err("name is required".into())
    } else if name != name.trim() || name.starts_with('.') || name.contains(['/', '\\']) || name.chars().any(char::is_control) {
        Err(format!("invalid connection name '{name}': no / or \\, no leading dot, no control characters"))
    } else {
        Ok(())
    }
}

/// The OS user, Postgres's default user name (`USER`, else `LOGNAME`, else `postgres`).
pub fn os_user() -> String {
    ["USER", "LOGNAME"].iter().find_map(|v| std::env::var(v).ok().filter(|s| !s.is_empty())).unwrap_or_else(|| "postgres".into())
}

/// Turns whatever was typed into the nearest valid name, as the form's Name field does while you
/// type: non-ASCII is transliterated to Latin, lowercased, whitespace becomes `-`, every other
/// disallowed character and leading `-` / `_` are dropped, cut to [`MAX_NAME`]. A trailing `-`
/// stays (so `my ` then `db` gives `my-db`), repeated `-` are kept. Idempotent.
pub fn normalize_name(s: &str) -> String {
    let mut out = String::new();
    for c in deunicode::deunicode(s).chars() {
        let c = if c.is_whitespace() { '-' } else { c.to_ascii_lowercase() };
        let ok = c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_';
        if ok && !(out.is_empty() && (c == '-' || c == '_')) && out.len() < MAX_NAME {
            out.push(c);
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Driver {
    Postgres,
    /// MySQL and MariaDB.
    MySql,
    /// Elasticsearch over HTTP: `http://` without SSL mode, `https://` with one. `user` + password
    /// is basic auth; a password without `user` is an API key.
    Elastic,
}

impl Driver {
    /// The value of `"driver"` in a connection file.
    pub fn name(self) -> &'static str {
        match self {
            Driver::Postgres => "postgres",
            Driver::MySql => "mysql",
            Driver::Elastic => "elasticsearch",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SslMode {
    Disable,
    Prefer,
    Require,
    VerifyCa,
    VerifyFull,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ssl {
    /// `None`: the driver's default.
    pub mode: Option<SslMode>,
    /// Absolute (relative paths are resolved against the connections folder).
    pub ca: Option<PathBuf>,
    pub cert: Option<PathBuf>,
    pub key: Option<PathBuf>,
}

/// Which Unix socket to connect through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Socket {
    /// Found at connect time, see [`detect_socket`].
    Auto,
    /// Postgres: the socket folder, MySQL / MariaDB: the socket file.
    Path(String),
}

const PG_SOCKET_DIRS: [&str; 6] = ["/tmp", "/private/tmp", "/var/run/postgresql", "/run/postgresql", "/opt/homebrew/var/run/postgresql", "/usr/local/var/run/postgresql"];
const MYSQL_SOCKETS: [&str; 7] = ["/tmp/mysql.sock", "/tmp/mysqld.sock", "/var/run/mysqld/mysqld.sock", "/run/mysqld/mysqld.sock", "/var/lib/mysql/mysql.sock", "/opt/homebrew/var/mysql/mysql.sock", "/usr/local/var/mysql/mysql.sock"];

/// The places a server's Unix socket is usually at (Postgres: folders, MySQL: files).
pub fn socket_candidates(driver: Driver) -> &'static [&'static str] {
    match driver {
        Driver::Postgres => &PG_SOCKET_DIRS,
        Driver::MySql => &MYSQL_SOCKETS,
        Driver::Elastic => &[],
    }
}

/// The local server's Unix socket: Postgres' folder (holding `.s.PGSQL.<port>`) or MySQL's file.
pub fn detect_socket(driver: Driver, port: Option<u16>) -> Option<PathBuf> {
    detect_socket_in(driver, port, socket_candidates(driver), |p| p.exists())
}

/// [`detect_socket`] over an explicit candidate list and existence check.
pub fn detect_socket_in(driver: Driver, port: Option<u16>, candidates: &[&str], exists: impl Fn(&Path) -> bool) -> Option<PathBuf> {
    candidates.iter().map(PathBuf::from).find(|c| match driver {
        Driver::Postgres => exists(&c.join(format!(".s.PGSQL.{}", port.unwrap_or(5432)))),
        Driver::MySql => exists(c),
        Driver::Elastic => false,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connection {
    pub name: String,
    pub driver: Driver,
    pub url: Option<String>,
    pub host: Option<String>,
    /// A Unix socket instead of TCP; `host` is then ignored.
    pub socket: Option<Socket>,
    pub port: Option<u16>,
    pub database: Option<String>,
    pub user: Option<String>,
    /// `None` after loading: `password_enc` is decrypted only when a session opens (see
    /// [`Connection::resolve_password`]). Set by the form's Test with a typed password.
    pub password: Option<String>,
    /// The `password_enc` value of the file (`v1:<base64>`, see [`crypto`]).
    pub password_enc: Option<String>,
    pub ssl: Ssl,
    pub read_only: bool,
    /// A production connection: red, and every run asks for confirmation.
    pub prod: bool,
    /// Postgres `search_path`.
    pub schema: Option<String>,
    pub init_sql: Option<String>,
    pub color: Option<String>,
}

impl Connection {
    /// Whether a password is already known: set explicitly or written in the url.
    pub fn has_password(&self) -> bool {
        self.password.is_some() || self.password_enc.is_some() || self.url.as_deref().is_some_and(|u| !url_passwords(u).is_empty())
    }

    /// A copy with `password_enc` decrypted (the key in the data dir) when no password is
    /// given explicitly or in the url. No `password_enc` means no password: some local databases need none.
    /// Call right before connecting; loading never decrypts.
    pub fn resolve_password(&self) -> Result<Connection, String> {
        match siesta_settings::data_dir() {
            Some(d) => self.resolve_password_in(&d),
            None if self.password_enc.is_some() => Err("no home directory: can't find the password key".into()),
            None => Ok(self.clone()),
        }
    }

    /// [`resolve_password`](Self::resolve_password) with an explicit folder of the `key`.
    pub fn resolve_password_in(&self, keys: &Path) -> Result<Connection, String> {
        let mut c = self.clone();
        if self.password.is_none() && !self.url.as_deref().is_some_and(|u| !url_passwords(u).is_empty())
            && let Some(enc) = &self.password_enc
        {
            c.password = Some(crypto::decrypt(keys, &self.name, enc)?);
        }
        Ok(c)
    }

    /// The strip / dot colour: `color`, else red for a production connection.
    pub fn display_color(&self) -> Option<&str> {
        self.color.as_deref().or(self.prod.then_some("red"))
    }

    /// This connection pointed at `database` (`None`: keep its own default). The fingerprint of
    /// the copy differs per database, so schema caches stay apart.
    pub fn with_database(&self, database: Option<&str>) -> Connection {
        let mut c = self.clone();
        if let Some(d) = database {
            c.database = Some(d.to_string());
        }
        c
    }

    /// A stable id of the server this connection points at (driver, url without its password,
    /// host, port, database, user), for caches that must not mix a connection with another one
    /// that was later saved under the same name. FNV-1a: stable across Rust versions.
    pub fn fingerprint(&self) -> u64 {
        let parts = [
            Some(self.driver.name().to_string()),
            self.url.as_deref().map(mask_url),
            self.host.clone(),
            self.socket.as_ref().map(|s| match s {
                Socket::Auto => "socket:auto".to_string(),
                Socket::Path(p) => format!("socket:{p}"),
            }),
            self.port.map(|p| p.to_string()),
            self.database.clone(),
            self.user.clone(),
        ];
        let mut h: u64 = 0xcbf29ce484222325;
        for p in parts {
            // `\0` between fields, `\1` for an absent one: ("a", None) differs from (None, "a").
            for b in p.as_deref().map_or(&b"\x01"[..], str::as_bytes).iter().chain(&[0u8]) {
                h = (h ^ u64::from(*b)).wrapping_mul(0x100000001b3);
            }
        }
        h
    }
}

#[derive(Debug, Default)]
pub struct Connections {
    /// Sorted by name.
    pub connections: BTreeMap<String, Connection>,
    /// Connections that could not be built (unresolved variable, no driver, bad value).
    pub errors: BTreeMap<String, String>,
    /// Non-fatal: invalid JSON (file skipped), unknown keys.
    pub warnings: Vec<String>,
}

impl Connections {
    /// Reads `<config>/connections/*.json`.
    pub fn load(config: &Path) -> Self {
        let mut out = Self::default();
        let dir = connections_dir(config);
        for name in names(config) {
            if valid_name(&name).is_err() {
                out.warnings.push(format!("connection '{name}': names are lowercase latin letters, digits, - and _; open it and save to rename it to '{}'", normalize_name(&name)));
            }
            let path = dir.join(format!("{name}.json"));
            let obj = match std::fs::read_to_string(&path).map_err(|e| e.to_string()).and_then(|t| serde_json::from_str::<Value>(&t).map_err(|e| e.to_string())) {
                Ok(Value::Object(o)) => o,
                Ok(_) => {
                    out.warnings.push(format!("{}: not a JSON object, skipped", path.display()));
                    continue;
                }
                Err(e) => {
                    out.warnings.push(format!("{}: invalid JSON, skipped: {e}", path.display()));
                    continue;
                }
            };
            match parse(&name, &obj, &dir, &mut out.warnings) {
                Ok(c) => {
                    out.connections.insert(name, c);
                }
                Err(e) => {
                    out.errors.insert(name, e);
                }
            }
        }
        out
    }

    /// Names for the picker, sorted.
    pub fn names(&self) -> Vec<&str> {
        self.connections.keys().map(String::as_str).collect()
    }
}

/// The connection as JSON for CLI output: the password inside `url` becomes `***`.
pub fn masked(c: &Connection) -> Value {
    let mut m = Map::new();
    let mut put = |k: &str, v: Option<Value>| {
        if let Some(v) = v {
            m.insert(k.into(), v);
        }
    };
    let s = |o: &Option<String>| o.clone().map(Value::from);
    let p = |o: &Option<PathBuf>| o.as_ref().map(|p| Value::from(p.to_string_lossy().into_owned()));
    put("name", Some(c.name.clone().into()));
    put("driver", Some(c.driver.name().into()));
    put("url", c.url.as_ref().map(|u| mask_url(u).into()));
    put("host", s(&c.host));
    put(
        "socket",
        c.socket.as_ref().map(|k| match k {
            Socket::Auto => Value::Bool(true),
            Socket::Path(p) => Value::from(p.clone()),
        }),
    );
    put("port", c.port.map(Value::from));
    put("database", s(&c.database));
    put("user", s(&c.user));
    let mut ssl = Map::new();
    if let Some(mode) = c.ssl.mode {
        ssl.insert("mode".into(), mode_name(mode).into());
    }
    for (k, v) in [("ca", p(&c.ssl.ca)), ("cert", p(&c.ssl.cert)), ("key", p(&c.ssl.key))] {
        if let Some(v) = v {
            ssl.insert(k.into(), v);
        }
    }
    put("ssl", (!ssl.is_empty()).then(|| ssl.into()));
    put("read_only", Some(c.read_only.into()));
    put("prod", Some(c.prod.into()));
    put("schema", s(&c.schema));
    put("init_sql", s(&c.init_sql));
    put("color", s(&c.color));
    put("password_enc", c.password_enc.as_ref().map(|_| "***".into()));
    Value::Object(m)
}

pub fn mode_name(m: SslMode) -> &'static str {
    match m {
        SslMode::Disable => "disable",
        SslMode::Prefer => "prefer",
        SslMode::Require => "require",
        SslMode::VerifyCa => "verify-ca",
        SslMode::VerifyFull => "verify-full",
    }
}

/// Byte ranges of the passwords in `scheme://user:password@host/…?password=…`, in order.
pub fn url_passwords(url: &str) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let Some(start) = url.find("://").map(|i| i + 3) else { return out };
    let end = url[start..].find(['/', '?', '#']).map_or(url.len(), |i| start + i);
    if let Some(at) = url[start..end].rfind('@').map(|i| i + start)
        && let Some(colon) = url[start..at].find(':').map(|i| i + start)
    {
        out.push(colon + 1..at);
    }
    if let Some(q) = url.find('?').map(|i| i + 1) {
        let qend = url[q..].find('#').map_or(url.len(), |i| q + i);
        let mut pos = q;
        for part in url[q..qend].split('&') {
            if let Some(v) = part.strip_prefix("password=") {
                out.push(pos + "password=".len()..pos + "password=".len() + v.len());
            }
            pos += part.len() + 1;
        }
    }
    out
}

fn mask_url(url: &str) -> String {
    let mut out = url.to_string();
    for r in url_passwords(url).into_iter().rev() {
        out.replace_range(r, "***");
    }
    out
}

fn expand_home(p: &Path) -> PathBuf {
    match (p.strip_prefix("~"), std::env::home_dir()) {
        (Ok(rest), Some(home)) => home.join(rest),
        _ => p.to_path_buf(),
    }
}

/// `${env:NAME}` from the process environment.
fn subst(text: &str) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find("${") {
        let Some(close) = rest[open..].find('}').map(|c| c + open) else { break };
        out.push_str(&rest[..open]);
        let expr = rest[open + 2..close].trim();
        let value = match expr.split_once(':') {
            Some(("env", n)) => std::env::var(n.trim()).ok(),
            _ => return Err(format!("unsupported variable '${{{expr}}}'")),
        };
        out.push_str(&value.ok_or_else(|| format!("unresolved variable '${{{expr}}}'"))?);
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

fn parse_driver(driver: Option<&str>, url: Option<&str>) -> Result<Driver, String> {
    Ok(match driver {
        Some("postgres") => Driver::Postgres,
        Some("mysql") => Driver::MySql,
        Some("elasticsearch") => Driver::Elastic,
        Some(d) => return Err(format!("unknown driver '{d}' (postgres, mysql or elasticsearch)")),
        None => match url.and_then(|u| u.split_once("://")).map(|(s, _)| s) {
            Some("postgres" | "postgresql") => Driver::Postgres,
            Some("mysql" | "mariadb") => Driver::MySql,
            Some("http" | "https") => Driver::Elastic,
            _ => return Err("no driver: set \"driver\" or a postgres:// / mysql:// / http(s):// url".into()),
        },
    })
}

/// One connection's JSON object: unknown keys noted, `${env:}` substituted, ssl paths made absolute
/// (relative ones against `dir`), values checked. `password` is not a key (only `password_enc` is).
fn parse(name: &str, obj: &Map<String, Value>, dir: &Path, warnings: &mut Vec<String>) -> Result<Connection, String> {
    let at = || dir.join(format!("{name}.json")).display().to_string();
    let mut f = Map::new();
    for (k, v) in obj {
        if k == "password" {
            warnings.push(format!("{}: 'password' is ignored: set it in the connection form or with `siesta connections set-password {name}`", at()));
        } else if !KEYS.contains(&k.as_str()) {
            warnings.push(format!("{}: unknown key '{k}'", at()));
        } else if let (Value::Object(ssl), "ssl") = (v, k.as_str()) {
            let mut o = Map::new();
            for (sk, sv) in ssl {
                if !SSL_KEYS.contains(&sk.as_str()) {
                    warnings.push(format!("{}: unknown key 'ssl.{sk}'", at()));
                    continue;
                }
                let mut sv = subst_value(sv)?;
                if let (true, Value::String(p)) = (sk != "mode", &sv) {
                    sv = dir.join(expand_home(Path::new(p))).to_string_lossy().into_owned().into();
                }
                o.insert(sk.clone(), sv);
            }
            f.insert(k.clone(), o.into());
        } else {
            f.insert(k.clone(), subst_value(v)?);
        }
    }
    build(name, &f)
}

fn subst_value(v: &Value) -> Result<Value, String> {
    match v {
        Value::String(s) => subst(s).map(Value::String),
        other => Ok(other.clone()),
    }
}

fn build(name: &str, f: &Map<String, Value>) -> Result<Connection, String> {
    let str_of = |k: &str| match f.get(k) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(format!("'{k}' must be a string")),
    };
    let url = str_of("url")?;
    let driver = parse_driver(str_of("driver")?.as_deref(), url.as_deref())?;
    let port = match f.get("port") {
        None | Some(Value::Null) => None,
        Some(Value::Number(n)) => Some(n.as_u64().and_then(|n| u16::try_from(n).ok()).ok_or("'port' out of range")?),
        Some(Value::String(s)) => Some(s.trim().parse().map_err(|_| format!("'port' is not a number: '{s}'"))?),
        Some(_) => return Err("'port' must be a number".into()),
    };
    let flag = |k: &str| match f.get(k) {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(b)) => Ok(*b),
        Some(_) => Err(format!("'{k}' must be true or false")),
    };
    let (read_only, prod) = (flag("read_only")?, flag("prod")?);
    let mut ssl = Ssl::default();
    if let Some(Value::Object(o)) = f.get("ssl") {
        let path = |k: &str| o.get(k).and_then(Value::as_str).map(PathBuf::from);
        ssl.mode = match o.get("mode").and_then(Value::as_str) {
            None => None,
            Some("disable") => Some(SslMode::Disable),
            Some("prefer") => Some(SslMode::Prefer),
            Some("require") => Some(SslMode::Require),
            Some("verify-ca") => Some(SslMode::VerifyCa),
            Some("verify-full") => Some(SslMode::VerifyFull),
            Some(m) => return Err(format!("unknown ssl.mode '{m}'")),
        };
        (ssl.ca, ssl.cert, ssl.key) = (path("ca"), path("cert"), path("key"));
    }
    let mut host = str_of("host")?;
    let mut socket = match f.get("socket") {
        None | Some(Value::Null) | Some(Value::Bool(false)) => None,
        Some(Value::Bool(true)) => Some(Socket::Auto),
        Some(Value::String(p)) if p.trim().is_empty() => Some(Socket::Auto),
        Some(Value::String(p)) => Some(Socket::Path(p.trim().to_string())),
        Some(_) => return Err("'socket' must be true or a path".into()),
    };
    // Legacy: a host that is a path is a socket.
    if socket.is_none() && host.as_deref().is_some_and(|h| h.starts_with('/')) {
        socket = host.take().map(Socket::Path);
    }
    Ok(Connection {
        name: name.to_string(),
        driver,
        url,
        host,
        socket,
        port,
        database: str_of("database")?,
        user: str_of("user")?,
        password: None,
        password_enc: str_of("password_enc")?,
        ssl,
        read_only,
        prod,
        schema: str_of("schema")?,
        init_sql: str_of("init_sql")?,
        color: str_of("color")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn socket_detection_and_keys() {
        let pg = |dirs: &[&str], port, present: &[&str]| detect_socket_in(Driver::Postgres, port, dirs, |p| present.iter().any(|x| Path::new(x) == p));
        assert_eq!(pg(&["/a", "/b"], None, &["/b/.s.PGSQL.5432"]), Some(PathBuf::from("/b")));
        assert_eq!(pg(&["/a", "/b"], Some(6000), &["/a/.s.PGSQL.5432"]), None);
        assert_eq!(pg(&["/a", "/b"], Some(6000), &["/a/.s.PGSQL.6000", "/b/.s.PGSQL.6000"]), Some(PathBuf::from("/a")));
        let my = detect_socket_in(Driver::MySql, Some(1), &["/x.sock", "/y.sock"], |p| p == Path::new("/y.sock"));
        assert_eq!(my, Some(PathBuf::from("/y.sock")));

        let t = config(&[("a", r#"{"driver": "postgres", "socket": true}"#), ("b", r#"{"driver": "mysql", "socket": "/s.sock"}"#), ("c", r#"{"driver": "postgres", "host": "/tmp"}"#), ("d", r#"{"driver": "postgres", "socket": 3}"#)]);
        let c = Connections::load(t.path());
        assert!(c.warnings.is_empty(), "{:?}", c.warnings);
        assert_eq!(c.connections["a"].socket, Some(Socket::Auto));
        assert_eq!(c.connections["b"].socket, Some(Socket::Path("/s.sock".into())));
        assert_eq!((c.connections["c"].socket.clone(), c.connections["c"].host.clone()), (Some(Socket::Path("/tmp".into())), None));
        assert!(c.errors.contains_key("d"));
    }

    /// A config dir with one connection file per `(name, json)`.
    fn config(files: &[(&str, &str)]) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(connections_dir(tmp.path())).unwrap();
        for (n, j) in files {
            fs::write(connections_dir(tmp.path()).join(format!("{n}.json")), j).unwrap();
        }
        tmp
    }

    #[test]
    fn loads_one_file_per_connection_and_parses_prod() {
        let t = config(&[("a", r#"{"driver": "postgres", "host": "h", "port": 5, "prod": true, "read_only": true, "schema": "app"}"#), ("b", r#"{"driver": "mysql"}"#)]);
        fs::write(connections_dir(t.path()).join("notes.txt"), "x").unwrap();
        fs::write(connections_dir(t.path()).join(".hidden.json"), "{}").unwrap();
        let c = Connections::load(t.path());
        assert_eq!(c.names(), ["a", "b"]);
        let a = &c.connections["a"];
        assert_eq!((a.host.as_deref(), a.port, a.prod, a.read_only, a.password.as_deref()), (Some("h"), Some(5), true, true, None));
        assert!(c.warnings.is_empty() && c.errors.is_empty(), "{:?} {:?}", c.warnings, c.errors);
        assert!(Connections::load(&t.path().join("missing")).connections.is_empty());
        fs::write(connections_dir(t.path()).join("a.json"), r#"{"driver": "postgres", "prod": "yes"}"#).unwrap();
        assert!(Connections::load(t.path()).errors["a"].contains("'prod'"));
    }

    #[test]
    fn password_is_decrypted_lazily_from_password_enc() {
        let t = config(&[("lazy", r#"{"driver": "postgres", "host": "h"}"#)]);
        let enc = crypto::encrypt(t.path(), "lazy", "pw1").unwrap();
        let url_enc = crypto::encrypt(t.path(), "viaurl", "other").unwrap();
        fs::write(connections_dir(t.path()).join("enc.json"), format!(r#"{{"driver": "postgres", "password_enc": "{}"}}"#, crypto::encrypt(t.path(), "enc", "pw2").unwrap())).unwrap();
        fs::write(connections_dir(t.path()).join("viaurl.json"), format!(r#"{{"url": "postgres://u:pw@h/db", "password_enc": "{url_enc}"}}"#)).unwrap();
        fs::write(connections_dir(t.path()).join("moved.json"), format!(r#"{{"driver": "postgres", "password_enc": "{enc}"}}"#)).unwrap();
        let c = Connections::load(t.path());
        assert!(c.warnings.is_empty(), "{:?}", c.warnings);
        // Nothing stored: no password, no error (some local databases need none).
        assert!(!c.connections["lazy"].has_password());
        assert_eq!(c.connections["lazy"].resolve_password_in(t.path()).unwrap().password, None);
        assert!(c.connections["enc"].has_password() && c.connections["enc"].password.is_none());
        assert_eq!(c.connections["enc"].resolve_password_in(t.path()).unwrap().password.as_deref(), Some("pw2"));
        // A password in the url wins; nothing is decrypted.
        assert_eq!(c.connections["viaurl"].resolve_password_in(t.path()).unwrap().password, None);
        // The ciphertext of another connection does not decrypt.
        assert!(c.connections["moved"].resolve_password_in(t.path()).unwrap_err().contains("Can't decrypt the password of `moved`"));
        // password_enc is masked in CLI output.
        assert_eq!(masked(&c.connections["enc"])["password_enc"], "***");
    }

    #[test]
    fn ssl_paths_resolve_against_the_connections_folder() {
        let t = config(&[("a", r#"{"driver": "mysql", "ssl": {"mode": "verify-full", "ca": "./certs/ca.pem", "cert": "c.pem", "key": "/abs/k.pem"}}"#)]);
        let ssl = &Connections::load(t.path()).connections["a"].ssl;
        let d = connections_dir(t.path());
        assert_eq!(ssl.mode, Some(SslMode::VerifyFull));
        assert_eq!((ssl.ca.clone(), ssl.cert.clone(), ssl.key.clone()), (Some(d.join("./certs/ca.pem")), Some(d.join("c.pem")), Some(PathBuf::from("/abs/k.pem"))));
    }

    #[test]
    fn env_variables() {
        let t = config(&[("a", r#"{"url": "postgres://u:${env:PATH}@h/db", "user": "${env:PATH}"}"#), ("bad", r#"{"driver": "mysql", "user": "${env:SIESTA_TEST_SURELY_UNSET}"}"#), ("dot", r#"{"driver": "mysql", "user": "${dotenv:X}"}"#), ("ok", r#"{"driver": "mysql"}"#)]);
        let c = Connections::load(t.path());
        assert_eq!(c.connections["a"].user, std::env::var("PATH").ok());
        assert_eq!(c.names(), ["a", "ok"]);
        assert!(c.errors["bad"].contains("SIESTA_TEST_SURELY_UNSET"), "{}", c.errors["bad"]);
        assert!(c.errors["dot"].contains("unsupported variable"), "{}", c.errors["dot"]);
    }

    #[test]
    fn driver_from_url_and_missing() {
        let t = config(&[("p", r#"{"url": "postgresql://h/db"}"#), ("m", r#"{"url": "mariadb://h/db"}"#), ("n", r#"{"host": "h"}"#), ("x", r#"{"driver": "oracle"}"#)]);
        let c = Connections::load(t.path());
        assert_eq!((c.connections["p"].driver, c.connections["m"].driver), (Driver::Postgres, Driver::MySql));
        assert!(c.errors["n"].contains("no driver"));
        assert!(c.errors["x"].contains("oracle"));
    }

    #[test]
    fn warnings() {
        let t = config(&[("a", r#"{"driver": "postgres", "colour": "red", "password": "x", "ssl": {"cA": "x"}}"#), ("broken", "{ not json"), ("arr", "[]")]);
        let c = Connections::load(t.path());
        let w = c.warnings.join("\n");
        assert!(w.contains("unknown key 'colour'") && w.contains("'password' is ignored") && w.contains("ssl.cA") && !w.contains("password_enc"), "{w}");
        assert!(w.contains("broken.json") && w.contains("invalid JSON") && w.contains("arr.json"), "{w}");
        // The rest still loaded, and the password did not.
        assert_eq!(c.connections["a"].password, None);
    }

    #[test]
    fn names_are_slugs() {
        for ok in ["dev", "prod-eu_1", "a", "0x", "a-", &"a".repeat(64)] {
            assert!(valid_name(ok).is_ok(), "{ok}");
        }
        for bad in ["", " ", "a/b", ".x", "x ", "my db", "ключ", "Dev", "-a", "_a", "a.b", "a@b", &"a".repeat(65)] {
            assert!(valid_name(bad).is_err(), "{bad:?}");
        }
        for ok in ["dev", "my db", "prod-eu_1", "ключ", "My DB"] {
            assert!(valid_file_name(ok).is_ok(), "{ok}");
        }
        for bad in ["", " ", "a/b", "a\\b", ".x", " x", "x ", "a\nb"] {
            assert!(valid_file_name(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn normalizing() {
        for (i, o) in [("My DB", "my-db"), ("Größe", "grosse"), ("it's \"a\" @#.x/y", "its-a-xy"), ("-_-a", "a"), ("my ", "my-"), ("a  b", "a--b"), ("PROD_EU-1", "prod_eu-1"), ("---", ""), ("a-", "a-")] {
            assert_eq!(normalize_name(i), o, "{i:?}");
        }
        let k = normalize_name("ключ");
        assert!(!k.is_empty() && valid_name(&k).is_ok(), "{k}");
        assert_eq!(normalize_name(&"é".repeat(100)).len(), 64);
        for i in ["My DB", "ключ тест", "  x ", "-a-", "Größe!", "\u{1F600}a"] {
            let n = normalize_name(i);
            assert_eq!(normalize_name(&n), n, "{i:?}");
        }
    }

    #[test]
    fn legacy_names_load_with_a_warning() {
        let t = config(&[("My DB", r#"{"url": "postgres://h/d"}"#), ("ok", r#"{"url": "postgres://h/d"}"#)]);
        let c = Connections::load(t.path());
        assert!(c.connections.contains_key("My DB") && c.connections.contains_key("ok"));
        let w = c.warnings.join("\n");
        assert!(w.contains("connection 'My DB'") && w.contains("rename it to 'my-db'") && !w.contains("'ok'"), "{w}");
    }

    #[test]
    fn masking() {
        let t = config(&[("a", r#"{"url": "mysql://ro:s3cret@db:3306/shop?x=a@b", "read_only": true}"#)]);
        let m = masked(&Connections::load(t.path()).connections["a"]);
        assert_eq!(m["url"], "mysql://ro:***@db:3306/shop?x=a@b");
        assert_eq!((m["read_only"].as_bool(), m["driver"].as_str()), (Some(true), Some("mysql")));
        assert!(!m.to_string().contains("s3cret"));
        assert_eq!(mask_url("postgres://user@h/db"), "postgres://user@h/db");
        assert_eq!(mask_url("postgres://u@h/db?sslmode=require&password=x%40y&a=b#f"), "postgres://u@h/db?sslmode=require&password=***&a=b#f");
        assert_eq!(mask_url("postgres://u:p@h/db?password=q"), "postgres://u:***@h/db?password=***");
    }

    #[test]
    fn fingerprint_tells_servers_apart() {
        let t = config(&[("dev", r#"{"driver": "postgres", "host": "h1", "database": "x"}"#)]);
        let a = Connections::load(t.path()).connections["dev"].clone();
        let mut b = a.clone();
        b.host = Some("h2".into());
        assert_ne!(a.fingerprint(), b.fingerprint());
        assert_eq!(a.fingerprint(), a.clone().fingerprint());
        // The password (also in the url) is not part of it; the user and the field a value is in are.
        let (mut c, mut d) = (a.clone(), a.clone());
        c.password = Some("other".into());
        assert_eq!(a.fingerprint(), c.fingerprint());
        d.user = Some("someone".into());
        assert_ne!(a.fingerprint(), d.fingerprint());
        let (mut u1, mut u2) = (a.clone(), a.clone());
        (u1.url, u2.url) = (Some("postgres://u:one@h/db".into()), Some("postgres://u:two@h/db".into()));
        assert_eq!(u1.fingerprint(), u2.fingerprint());
        let (mut x, mut y) = (a.clone(), a.clone());
        (x.host, x.database, y.host, y.database) = (Some("ab".into()), None, None, Some("ab".into()));
        assert_ne!(x.fingerprint(), y.fingerprint());
    }
}
