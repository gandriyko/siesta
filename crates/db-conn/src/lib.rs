//! Connections: one JSON file per connection in `<config dir>/connections/<name>.json`
//! (`~/.config/siesta` unless `SIESTA_CONFIG_DIR` says otherwise); the file name is the connection name.
//!
//! A file holds driver, url | host / port / database / user, ssl, read_only, prod, schema, init_sql, color.
//! Values may use `${env:NAME}`. There is no password key: the password lives only in the system
//! keychain (`secrets`, service `siesta`, account = connection name), read when a session opens.
//! A broken connection lands in `errors`, a suspicious key in `warnings`; the rest loads.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

mod global;
mod legacy;
pub mod secrets;
pub use global::{ConnectionForm, delete, names, read_form, save};
pub use legacy::migrate;
mod urlparse;
pub use urlparse::{ParsedUrl, parse_url};

const KEYS: [&str; 12] = ["driver", "url", "host", "port", "database", "user", "ssl", "read_only", "prod", "schema", "init_sql", "color"];
const SSL_KEYS: [&str; 4] = ["mode", "ca", "cert", "key"];

/// `~/.config/siesta`, where connections (and the settings) live; `SIESTA_CONFIG_DIR` overrides it
/// (tests, sandboxes).
pub fn global_dir() -> Option<PathBuf> {
    match std::env::var_os("SIESTA_CONFIG_DIR") {
        Some(d) if !d.is_empty() => Some(d.into()),
        _ => Some(std::env::home_dir()?.join(".config/siesta")),
    }
}

/// The folder of the per-connection files inside the config dir.
pub fn connections_dir(config: &Path) -> PathBuf {
    config.join("connections")
}

/// A connection name is a file name: no path separators, no leading dot, no control characters.
pub fn valid_name(name: &str) -> Result<(), String> {
    if name.trim().is_empty() {
        Err("name is required".into())
    } else if name != name.trim() || name.starts_with('.') || name.contains(['/', '\\']) || name.chars().any(char::is_control) {
        Err(format!("invalid connection name '{name}': no / or \\, no leading dot, no control characters"))
    } else {
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Driver {
    Postgres,
    /// MySQL and MariaDB.
    MySql,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Connection {
    pub name: String,
    pub driver: Driver,
    pub url: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub database: Option<String>,
    pub user: Option<String>,
    /// `None` after loading: the keychain is read only when a session opens (see
    /// [`Connection::resolve_password`]). Set by the form's Test with a typed password.
    pub password: Option<String>,
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
        self.password.is_some() || self.url.as_deref().is_some_and(|u| !url_passwords(u).is_empty())
    }

    /// A copy with the password read from the system keychain (account = connection name) when none
    /// is known yet. No entry (or no keychain) means no password: some local databases need none.
    /// Call right before connecting: on macOS a read may prompt, so loading never does it.
    pub fn resolve_password(&self) -> Result<Connection, String> {
        let mut c = self.clone();
        if !c.has_password() {
            match secrets::get(&self.name) {
                Ok(p) => c.password = Some(p),
                Err(secrets::SecretError::Missing | secrets::SecretError::Unavailable(_)) => {}
                Err(e) => return Err(e.to_string()),
            }
        }
        Ok(c)
    }

    /// The strip / dot colour: `color`, else red for a production connection.
    pub fn display_color(&self) -> Option<&str> {
        self.color.as_deref().or(self.prod.then_some("red"))
    }

    /// A stable id of the server this connection points at (driver, url without its password,
    /// host, port, database, user), for caches that must not mix a connection with another one
    /// that was later saved under the same name. FNV-1a: stable across Rust versions.
    pub fn fingerprint(&self) -> u64 {
        let parts = [
            Some(if self.driver == Driver::Postgres { "postgres" } else { "mysql" }.to_string()),
            self.url.as_deref().map(mask_url),
            self.host.clone(),
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
    /// Non-fatal: invalid JSON (file skipped), unknown keys, migration notes.
    pub warnings: Vec<String>,
}

impl Connections {
    /// Reads `<config>/connections/*.json` (after the one-time migration of an old `connections.json`).
    pub fn load(config: &Path) -> Self {
        let mut out = Self { warnings: migrate(config), ..Default::default() };
        let dir = connections_dir(config);
        for name in names(config) {
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

/// Nearest ancestor of `file` with `.git` or `.siesta`; else the file's folder.
pub fn workspace_root(file: &Path) -> PathBuf {
    let dir = file.parent().unwrap_or(Path::new("."));
    dir.ancestors().find(|d| d.join(".git").exists() || d.join(".siesta").exists()).unwrap_or(dir).to_path_buf()
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
    put("driver", Some(if c.driver == Driver::Postgres { "postgres" } else { "mysql" }.into()));
    put("url", c.url.as_ref().map(|u| mask_url(u).into()));
    put("host", s(&c.host));
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
        Some(d) => return Err(format!("unknown driver '{d}' (postgres or mysql)")),
        None => match url.and_then(|u| u.split_once("://")).map(|(s, _)| s) {
            Some("postgres" | "postgresql") => Driver::Postgres,
            Some("mysql" | "mariadb") => Driver::MySql,
            _ => return Err("no driver: set \"driver\" or a postgres:// / mysql:// url".into()),
        },
    })
}

/// One connection's JSON object: unknown keys noted, `${env:}` substituted, ssl paths made absolute
/// (relative ones against `dir`), values checked. `password` is not a key (the keychain holds it).
fn parse(name: &str, obj: &Map<String, Value>, dir: &Path, warnings: &mut Vec<String>) -> Result<Connection, String> {
    let at = || dir.join(format!("{name}.json")).display().to_string();
    let mut f = Map::new();
    for (k, v) in obj {
        if k == "password" {
            warnings.push(format!("{}: 'password' is ignored: passwords live in the system keychain", at()));
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
    Ok(Connection {
        name: name.to_string(),
        driver,
        url,
        host: str_of("host")?,
        port,
        database: str_of("database")?,
        user: str_of("user")?,
        password: None,
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
    fn password_comes_from_the_keychain_by_name() {
        secrets::use_memory_store();
        let t = config(&[("kc-lazy", r#"{"driver": "postgres", "host": "h"}"#), ("kc-url", r#"{"url": "postgres://u:pw@h/db"}"#)]);
        let c = Connections::load(t.path());
        let lazy = &c.connections["kc-lazy"];
        // Nothing stored: no password, no error (some local databases need none).
        assert_eq!(lazy.resolve_password().unwrap().password, None);
        secrets::set("kc-lazy", "pw1").unwrap();
        assert_eq!(lazy.resolve_password().unwrap().password.as_deref(), Some("pw1"));
        // A password in the url wins; the keychain is not consulted.
        secrets::set("kc-url", "other").unwrap();
        assert_eq!(c.connections["kc-url"].resolve_password().unwrap().password, None);
        // Unavailable keychain: connect without a password.
        secrets::set_unavailable(true);
        let r = lazy.resolve_password();
        secrets::set_unavailable(false);
        assert_eq!(r.unwrap().password, None);
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
        assert!(w.contains("unknown key 'colour'") && w.contains("'password' is ignored") && w.contains("ssl.cA"), "{w}");
        assert!(w.contains("broken.json") && w.contains("invalid JSON") && w.contains("arr.json"), "{w}");
        // The rest still loaded, and the password did not.
        assert_eq!(c.connections["a"].password, None);
    }

    #[test]
    fn names_are_file_names() {
        for ok in ["dev", "my db", "prod-eu_1", "ключ"] {
            assert!(valid_name(ok).is_ok(), "{ok}");
        }
        for bad in ["", " ", "a/b", "a\\b", ".x", " x", "x ", "a\nb"] {
            assert!(valid_name(bad).is_err(), "{bad:?}");
        }
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

    #[test]
    fn root_resolution() {
        let tmp = tempfile::tempdir().unwrap();
        let t = tmp.path();
        fs::create_dir_all(t.join("repo/.git")).unwrap();
        fs::create_dir_all(t.join("repo/api/v1")).unwrap();
        fs::create_dir_all(t.join("other/.siesta")).unwrap();
        fs::create_dir_all(t.join("plain/sub")).unwrap();
        assert_eq!(workspace_root(&t.join("repo/api/v1/a.sql")), t.join("repo"));
        assert_eq!(workspace_root(&t.join("other/a.sql")), t.join("other"));
        assert_eq!(workspace_root(&t.join("plain/sub/a.sql")), t.join("plain/sub"));
    }
}
