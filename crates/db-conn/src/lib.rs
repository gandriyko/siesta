//! Connections: finds and merges `siesta.json` / `siesta.private.json`, resolves `${env:}` / `${dotenv:}`.
//!
//! Files are read first from the global config dir (`~/.config/siesta/connections.json`, then
//! `connections.private.json`), then from the workspace root down to the `.sql` file's folder (public,
//! then private in each folder), then the extra files; later ones win, so a repo's `siesta.json`
//! overrides a global connection of the same name. Merging is per connection key, field by field
//! (`ssl` deeply). A broken connection lands in `errors`, a broken file in `warnings`; the rest loads.
//! Variables are resolved, and relative `ssl` paths made absolute, per file before merging, so
//! `.env` and relative paths belong to the file that holds the value.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

mod global;
pub mod secrets;
mod workspace;
pub use global::{ConnectionForm,delete_global, global_names, read_global_form, save_global};
pub use workspace::{WorkspacePassword, add_to_gitignore, password_override, private_file_of, set_workspace_password, workspace_def_file};

/// `${keychain}` is `Some(None)` (the account is the connection name), `${keychain:ACCOUNT}` is
/// `Some(Some(ACCOUNT))`; anything else is not a marker.
fn keychain_marker(s: &str) -> Option<Option<&str>> {
    let inner = s.trim().strip_prefix("${")?.strip_suffix('}')?.trim();
    match inner.split_once(':') {
        None if inner == "keychain" => Some(None),
        Some(("keychain", a)) if !a.trim().is_empty() => Some(Some(a.trim())),
        _ => None,
    }
}

/// The marker as written to JSON: the short form when the account is the connection name.
fn marker_text(account: &str, name: &str) -> String {
    if account == name { "${keychain}".into() } else { format!("${{keychain:{account}}}") }
}

const KEYS: [&str; 13] = ["driver", "url", "host", "port", "database", "user", "password", "ssl", "read_only", "prod", "schema", "init_sql", "color"];
const GLOBAL_PUBLIC: &str = "connections.json";
const GLOBAL_PRIVATE: &str = "connections.private.json";

/// `~/.config/siesta`, where global connections (and the settings) live; `SIESTA_CONFIG_DIR`
/// overrides it (tests, sandboxes).
pub fn global_dir() -> Option<PathBuf> {
    match std::env::var_os("SIESTA_CONFIG_DIR") {
        Some(d) if !d.is_empty() => Some(d.into()),
        _ => Some(std::env::home_dir()?.join(".config/siesta")),
    }
}
const SSL_KEYS: [&str; 4] = ["mode", "ca", "cert", "key"];

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
    /// Absolute (relative paths were resolved against the file that set them).
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
    pub password: Option<String>,
    /// The account of a `"${keychain}"` / `"${keychain:ACCOUNT}"` password, read from the system
    /// keychain only when a session opens (see [`Connection::resolve_password`]); `password` is `None` then.
    pub keychain: Option<String>,
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
    /// A copy with the password read from the system keychain (when it is a `${keychain}` one).
    /// Call right before connecting: on macOS a read may prompt, so loading never does it.
    pub fn resolve_password(&self) -> Result<Connection, String> {
        let mut c = self.clone();
        if let Some(account) = &self.keychain {
            c.password = Some(secrets::get(account).map_err(|e| match e {
                secrets::SecretError::Missing => format!(
                    "No password for `{}` in the system keychain (service `{}`, account `{account}`). Edit the connection to set it.",
                    self.name,
                    secrets::SERVICE
                ),
                e => e.to_string(),
            })?);
        }
        Ok(c)
    }

    /// The strip / dot colour: `color`, else red for a production connection.
    pub fn display_color(&self) -> Option<&str> {
        self.color.as_deref().or(self.prod.then_some("red"))
    }

    /// A stable id of the server this connection points at (driver, url without its password,
    /// host, port, database, user), for caches that must not mix two connections that merely
    /// share a name (the same name in two `siesta.json` files). FNV-1a: stable across Rust versions.
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
    /// Every file that defines the name, lowest priority first (also for names in `errors`).
    pub sources: BTreeMap<String, Vec<PathBuf>>,
    /// Non-fatal: invalid JSON (file skipped), unknown keys, literal password in a public file.
    pub warnings: Vec<String>,
}

impl Connections {
    /// The global files of `global` (if any), then files from `root` down to the `.sql` file's folder,
    /// then `extra` (`~` expanded), later ones overriding earlier ones.
    pub fn load(sql_file: &Path, root: &Path, extra: &[PathBuf], global: Option<&Path>) -> Self {
        let dir = sql_file.parent().unwrap_or(root);
        let mut dirs: Vec<&Path> = dir.ancestors().take_while(|d| d.starts_with(root)).collect();
        if dirs.is_empty() {
            dirs.push(dir);
        }
        dirs.reverse();
        let mut files: Vec<(PathBuf, bool)> = global.into_iter().flat_map(|d| [(d.join(GLOBAL_PUBLIC), false), (d.join(GLOBAL_PRIVATE), true)]).collect();
        files.extend(dirs.iter().flat_map(|d| [(d.join("siesta.json"), false), (d.join("siesta.private.json"), true)]));
        // An extra file counts as public only when it is named like the public one.
        files.extend(extra.iter().map(|p| {
            let p = expand_home(p);
            let private = p.file_name().is_none_or(|n| n != "siesta.json");
            (p, private)
        }));

        let mut out = Self::default();
        let mut merged: BTreeMap<String, Map<String, Value>> = BTreeMap::new();
        for (path, private) in files {
            let Ok(text) = std::fs::read_to_string(&path) else { continue };
            let json = match serde_json::from_str::<Map<String, Value>>(&text) {
                Ok(j) => j,
                Err(e) => {
                    out.warnings.push(format!("{}: invalid JSON, skipped: {e}", path.display()));
                    continue;
                }
            };
            let fdir = path.parent().unwrap_or(Path::new("."));
            for (name, v) in json {
                let Value::Object(obj) = v else {
                    out.warnings.push(format!("{}: '{name}' is not an object, skipped", path.display()));
                    continue;
                };
                let fields = read_entry(&obj, fdir, private, &path, &name, &mut out.warnings);
                out.sources.entry(name.clone()).or_default().push(path.clone());
                merge(merged.entry(name).or_default(), fields);
            }
        }
        for (name, fields) in merged {
            match build(&name, &fields) {
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

/// The connection as JSON for CLI output: `password` and the password inside `url` become `***`.
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
    // The marker is not a secret; a literal password is.
    put("password", c.keychain.as_ref().map(|a| marker_text(a, &c.name).into()).or(c.password.as_ref().map(|_| "***".into())));
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

/// Merges one file's fields over the earlier ones; `ssl` field by field.
fn merge(into: &mut Map<String, Value>, from: Map<String, Value>) {
    for (k, v) in from {
        match (into.get_mut(&k), v) {
            (Some(Value::Object(old)), Value::Object(new)) if k == "ssl" => old.extend(new),
            (_, v) => {
                into.insert(k, v);
            }
        }
    }
}

/// One connection of one file: variables substituted, ssl paths made absolute, warnings noted.
fn read_entry(obj: &Map<String, Value>, dir: &Path, private: bool, file: &Path, name: &str, warnings: &mut Vec<String>) -> Map<String, Value> {
    let mut out = Map::new();
    for (k, v) in obj {
        if !KEYS.contains(&k.as_str()) {
            warnings.push(format!("{}: '{name}': unknown key '{k}'", file.display()));
            continue;
        }
        let literal = |s: &str| !s.contains("${");
        match (k.as_str(), v) {
            ("password", Value::String(s)) if keychain_marker(s).is_some() => {
                let account = keychain_marker(s).flatten().unwrap_or(name);
                out.insert(k.clone(), Map::from_iter([("$keychain".to_string(), account.into())]).into());
            }
            ("ssl", Value::Object(ssl)) => {
                let mut o = Map::new();
                for (sk, sv) in ssl {
                    if !SSL_KEYS.contains(&sk.as_str()) {
                        warnings.push(format!("{}: '{name}': unknown key 'ssl.{sk}'", file.display()));
                        continue;
                    }
                    let mut sv = subst_value(sv, dir);
                    if let (true, Value::String(p)) = (sk != "mode", &sv) {
                        let p = expand_home(Path::new(p));
                        sv = dir.join(p).to_string_lossy().into_owned().into();
                    }
                    o.insert(sk.clone(), sv);
                }
                out.insert(k.clone(), o.into());
            }
            ("password", Value::String(s)) if !private && !s.is_empty() && literal(s) => {
                warnings.push(format!("{}: '{name}': literal password in a public file, move it to siesta.private.json", file.display()));
                out.insert(k.clone(), subst_value(v, dir));
            }
            ("url", Value::String(u)) if !private && url_passwords(u).into_iter().any(|r| literal(&u[r])) => {
                warnings.push(format!("{}: '{name}': literal password in url of a public file, move it to siesta.private.json", file.display()));
                out.insert(k.clone(), subst_value(v, dir));
            }
            _ => {
                out.insert(k.clone(), subst_value(v, dir));
            }
        }
    }
    out
}

/// A failed substitution becomes `{"$unresolved": message}`: a later file setting the same field
/// replaces it, and `build` fails the connection only if it survives the merge.
fn subst_value(v: &Value, dir: &Path) -> Value {
    match v {
        Value::String(s) => subst(s, dir).map_or_else(|e| Value::Object(Map::from_iter([("$unresolved".to_string(), e.into())])), Value::String),
        other => other.clone(),
    }
}

fn unresolved(v: &Value) -> Option<&str> {
    v.as_object()?.get("$unresolved")?.as_str()
}

/// `${env:NAME}` from the process, `${dotenv:NAME}` from `.env` in `dir`.
fn subst(text: &str, dir: &Path) -> Result<String, String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find("${") {
        let Some(close) = rest[open..].find('}').map(|c| c + open) else { break };
        out.push_str(&rest[..open]);
        let expr = rest[open + 2..close].trim();
        let value = match expr.split_once(':') {
            Some(("env", n)) => std::env::var(n.trim()).ok(),
            Some(("dotenv", n)) => dotenvy::from_path_iter(dir.join(".env")).ok().and_then(|it| it.flatten().find(|(k, _)| k == n.trim()).map(|(_, v)| v)),
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

fn build(name: &str, f: &Map<String, Value>) -> Result<Connection, String> {
    let ssl_fields = f.get("ssl").and_then(Value::as_object).into_iter().flatten();
    if let Some(e) = f.values().chain(ssl_fields.map(|(_, v)| v)).find_map(unresolved) {
        return Err(e.to_string());
    }
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
    let keychain = f.get("password").and_then(|v| v.get("$keychain")).and_then(Value::as_str).map(String::from);
    let password = if keychain.is_some() { None } else { str_of("password")? };
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
        password,
        keychain,
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

    fn load(root: &Path, file: &str) -> Connections {
        Connections::load(&root.join(file), root, &[], None)
    }

    #[test]
    fn keychain_markers_resolve_lazily_and_mask() {
        secrets::use_memory_store();
        assert_eq!((keychain_marker("${keychain}"), keychain_marker(" ${keychain: a b }"), keychain_marker("${keychain:}"), keychain_marker("${env:X}"), keychain_marker("pw")), (Some(None), Some(Some("a b")), None, None, None));
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        // The marker is fine in a public file (no "literal password" warning).
        fs::write(r.join("siesta.json"), r#"{"kc-lazy": {"driver": "postgres", "host": "h", "password": "${keychain}"}, "kc-team": {"driver": "postgres", "host": "h", "password": "${keychain:kc-shared}"}}"#).unwrap();
        let c = load(r, "q.sql");
        assert!(c.warnings.is_empty() && c.errors.is_empty(), "{:?} {:?}", c.warnings, c.errors);
        let (lazy, team) = (&c.connections["kc-lazy"], &c.connections["kc-team"]);
        assert_eq!((lazy.keychain.as_deref(), lazy.password.as_deref(), team.keychain.as_deref()), (Some("kc-lazy"), None, Some("kc-shared")));
        // Nothing stored yet: a clear error naming the account.
        let e = lazy.resolve_password().unwrap_err();
        assert_eq!(e, "No password for `kc-lazy` in the system keychain (service `siesta`, account `kc-lazy`). Edit the connection to set it.");
        secrets::set("kc-lazy", "pw1").unwrap();
        secrets::set("kc-shared", "pw2").unwrap();
        assert_eq!((lazy.resolve_password().unwrap().password.as_deref(), team.resolve_password().unwrap().password.as_deref()), (Some("pw1"), Some("pw2")));
        // `masked` shows the marker as written, not a secret.
        assert_eq!((masked(lazy)["password"].as_str(), masked(team)["password"].as_str()), (Some("${keychain}"), Some("${keychain:kc-shared}")));
        // A later file's literal password replaces the marker.
        fs::write(r.join("siesta.private.json"), r#"{"kc-lazy": {"password": "lit"}}"#).unwrap();
        let c = load(r, "q.sql");
        assert_eq!((c.connections["kc-lazy"].keychain.clone(), c.connections["kc-lazy"].password.as_deref()), (None, Some("lit")));
        assert_eq!(masked(&c.connections["kc-lazy"])["password"], "***");
    }

    #[test]
    fn chain_merge_order() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        fs::create_dir_all(r.join("svc")).unwrap();
        fs::write(r.join("siesta.json"), r#"{"a": {"driver": "postgres", "host": "root", "port": 1, "user": "u"}}"#).unwrap();
        fs::write(r.join("siesta.private.json"), r#"{"a": {"host": "root-private", "password": "p"}}"#).unwrap();
        fs::write(r.join("svc/siesta.json"), r#"{"a": {"port": 2}, "b": {"driver": "mysql"}}"#).unwrap();
        let c = load(r, "svc/q.sql");
        let a = &c.connections["a"];
        assert_eq!((a.host.as_deref(), a.port, a.user.as_deref(), a.password.as_deref()), (Some("root-private"), Some(2), Some("u"), Some("p")));
        assert_eq!(c.names(), ["a", "b"]);
        // From the root folder the leaf's connections are not visible.
        assert_eq!(load(r, "q.sql").names(), ["a"]);
        assert!(c.warnings.is_empty(), "{:?}", c.warnings);

        // Extra files come last.
        let extra = r.join("extra.private.json");
        fs::write(&extra, r#"{"a": {"host": "extra"}}"#).unwrap();
        let c = Connections::load(&r.join("svc/q.sql"), r, &[extra], None);
        assert_eq!(c.connections["a"].host.as_deref(), Some("extra"));
    }

    #[test]
    fn global_files_come_first_and_prod_parses() {
        let tmp = tempfile::tempdir().unwrap();
        let (g, r) = (tmp.path().join("cfg"), tmp.path().join("repo"));
        fs::create_dir_all(&g).unwrap();
        fs::create_dir_all(&r).unwrap();
        fs::write(g.join("connections.json"), r#"{"a": {"driver": "postgres", "host": "global", "prod": true}, "g": {"driver": "mysql", "password": "lit"}}"#).unwrap();
        fs::write(g.join("connections.private.json"), r#"{"a": {"password": "gp"}, "g": {"password": "from-private"}}"#).unwrap();
        fs::write(r.join("siesta.json"), r#"{"a": {"host": "repo"}}"#).unwrap();
        let c = Connections::load(&r.join("q.sql"), &r, &[], Some(&g));
        let a = &c.connections["a"];
        assert_eq!((a.host.as_deref(), a.password.as_deref(), a.prod), (Some("repo"), Some("gp"), true));
        assert_eq!(c.sources["a"].len(), 3);
        assert_eq!(c.connections["g"].password.as_deref(), Some("from-private"));
        // The literal-password warning does not apply to the global private file (the public one still warns).
        assert_eq!(c.warnings.len(), 1, "{:?}", c.warnings);
        assert!(c.warnings[0].contains("'g': literal password"));
        assert!(!Connections::load(&r.join("q.sql"), &r, &[], None).connections.contains_key("g"));
        fs::write(r.join("siesta.json"), r#"{"a": {"prod": "yes"}}"#).unwrap();
        assert!(Connections::load(&r.join("q.sql"), &r, &[], Some(&g)).errors["a"].contains("'prod'"));
    }

    #[test]
    fn ssl_merge_and_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        fs::create_dir_all(r.join("svc")).unwrap();
        fs::write(r.join("siesta.json"), r#"{"a": {"driver": "mysql", "ssl": {"mode": "verify-full", "ca": "./certs/ca.pem"}}}"#).unwrap();
        fs::write(r.join("svc/siesta.private.json"), r#"{"a": {"ssl": {"cert": "c.pem", "key": "/abs/k.pem", "mode": "require"}}}"#).unwrap();
        let ssl = &load(r, "svc/q.sql").connections["a"].ssl;
        assert_eq!(ssl.mode, Some(SslMode::Require));
        assert_eq!(ssl.ca, Some(r.join("./certs/ca.pem")));
        assert_eq!(ssl.cert, Some(r.join("svc/c.pem")));
        assert_eq!(ssl.key, Some(PathBuf::from("/abs/k.pem")));
    }

    #[test]
    fn env_and_dotenv() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        fs::create_dir_all(r.join("svc")).unwrap();
        fs::write(r.join("svc/.env"), "DB_PASS=from-dotenv\n").unwrap();
        fs::write(r.join(".env"), "DB_PASS=wrong\n").unwrap();
        fs::write(r.join("svc/siesta.private.json"), r#"{"a": {"driver": "postgres", "password": "${dotenv:DB_PASS}", "user": "${env:PATH}"}}"#).unwrap();
        let c = load(r, "svc/q.sql");
        let a = &c.connections["a"];
        assert_eq!(a.password.as_deref(), Some("from-dotenv"));
        assert_eq!(a.user, std::env::var("PATH").ok());
    }

    #[test]
    fn unresolved_variable_fails_only_that_connection() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        fs::write(r.join("siesta.private.json"), r#"{"bad": {"driver": "mysql", "password": "${env:SIESTA_TEST_SURELY_UNSET}"}, "ok": {"driver": "mysql"}, "nodot": {"driver": "mysql", "user": "${dotenv:X}"}}"#).unwrap();
        let c = load(r, "q.sql");
        assert_eq!(c.names(), ["ok"]);
        assert!(c.errors["bad"].contains("SIESTA_TEST_SURELY_UNSET"), "{}", c.errors["bad"]);
        assert!(c.errors["nodot"].contains("dotenv:X"));
    }

    #[test]
    fn unresolved_variable_overridden_by_later_file() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        fs::write(r.join("siesta.json"), r#"{"a": {"driver": "mysql", "password": "${env:SIESTA_TEST_SURELY_UNSET}", "ssl": {"ca": "${env:SIESTA_TEST_SURELY_UNSET}"}}}"#).unwrap();
        assert!(load(r, "q.sql").errors["a"].contains("SIESTA_TEST_SURELY_UNSET"));
        fs::write(r.join("siesta.private.json"), r#"{"a": {"password": "lit", "ssl": {"ca": "ca.pem"}}}"#).unwrap();
        let c = load(r, "q.sql");
        assert!(c.errors.is_empty(), "{:?}", c.errors);
        assert_eq!(c.connections["a"].password.as_deref(), Some("lit"));
    }

    #[test]
    fn driver_from_url_and_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        fs::write(
            r.join("siesta.json"),
            r#"{"p": {"url": "postgresql://h/db"}, "m": {"url": "mariadb://h/db"}, "n": {"host": "h"}, "x": {"driver": "oracle"}}"#,
        )
        .unwrap();
        let c = load(r, "q.sql");
        assert_eq!((c.connections["p"].driver, c.connections["m"].driver), (Driver::Postgres, Driver::MySql));
        assert!(c.errors["n"].contains("no driver"));
        assert!(c.errors["x"].contains("oracle"));
    }

    #[test]
    fn warnings() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        fs::create_dir_all(r.join("svc")).unwrap();
        fs::write(r.join("siesta.json"), r#"{"a": {"driver": "postgres", "password": "hunter2", "colour": "red"}, "b": {"url": "mysql://u:pw@h/d"}, "c": {"driver": "mysql", "password": "${env:PATH}"}}"#).unwrap();
        fs::write(r.join("siesta.private.json"), r#"{"a": {"password": "fine"}}"#).unwrap();
        fs::write(r.join("svc/siesta.json"), "{ not json").unwrap();
        let c = load(r, "svc/q.sql");
        let w = c.warnings.join("\n");
        assert!(w.contains("'a': literal password"), "{w}");
        assert!(w.contains("'b': literal password in url"), "{w}");
        assert!(!w.contains("'c'"), "{w}");
        assert!(w.contains("unknown key 'colour'"), "{w}");
        assert!(w.contains("svc/siesta.json") && w.contains("invalid JSON"), "{w}");
        // The rest still loaded.
        assert_eq!(c.connections["a"].password.as_deref(), Some("fine"));
    }

    #[test]
    fn masking() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        fs::write(r.join("siesta.private.json"), r#"{"a": {"url": "mysql://ro:s3cret@db:3306/shop?x=a@b", "password": "s3cret", "read_only": true}}"#).unwrap();
        let c = load(r, "q.sql");
        let m = masked(&c.connections["a"]);
        assert_eq!(m["url"], "mysql://ro:***@db:3306/shop?x=a@b");
        assert_eq!((m["password"].as_str(), m["read_only"].as_bool(), m["driver"].as_str()), (Some("***"), Some(true), Some("mysql")));
        assert!(!m.to_string().contains("s3cret"));
        assert_eq!(mask_url("postgres://user@h/db"), "postgres://user@h/db");
        assert_eq!(mask_url("postgres://u@h/db?sslmode=require&password=x%40y&a=b#f"), "postgres://u@h/db?sslmode=require&password=***&a=b#f");
        assert_eq!(mask_url("postgres://u:p@h/db?password=q"), "postgres://u:***@h/db?password=***");
    }

    #[test]
    fn password_in_url_query_is_masked_and_warned() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        fs::write(r.join("siesta.json"), r#"{"a": {"url": "postgres://u@h/db?password=hunter2"}, "b": {"url": "postgres://u@h/db?password=${env:PATH}"}}"#).unwrap();
        let c = load(r, "q.sql");
        assert!(!masked(&c.connections["a"]).to_string().contains("hunter2"));
        let w = c.warnings.join("\n");
        assert!(w.contains("'a': literal password in url"), "{w}");
        assert!(!w.contains("'b'"), "{w}");
    }

    #[test]
    fn fingerprint_tells_same_named_connections_apart() {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        fs::create_dir_all(r.join("a")).unwrap();
        fs::create_dir_all(r.join("b")).unwrap();
        fs::write(r.join("a/siesta.json"), r#"{"dev": {"driver": "postgres", "host": "h1", "database": "x", "password": "p1"}}"#).unwrap();
        fs::write(r.join("b/siesta.json"), r#"{"dev": {"driver": "postgres", "host": "h2", "database": "x", "password": "p2"}}"#).unwrap();
        let (a, b) = (load(r, "a/q.sql").connections["dev"].clone(), load(r, "b/q.sql").connections["dev"].clone());
        assert_ne!(a.fingerprint(), b.fingerprint());
        assert_eq!(a.fingerprint(), load(r, "a/q.sql").connections["dev"].fingerprint());
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
