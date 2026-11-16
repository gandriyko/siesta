//! The global connection files (`connections.json` + `connections.private.json` in the config dir):
//! GUI-free read / modify / write behind the "New / Edit connection" form.
//!
//! Only the keys the form owns are touched; other connections and unknown keys survive. Files are
//! written atomically (temp file + rename), the private one with mode 0600 on unix. Keys come out
//! sorted (serde_json maps are ordered by key).

use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::{Connection, Driver, GLOBAL_PRIVATE, GLOBAL_PUBLIC, SslMode, build, mode_name, parse_driver, read_entry};

/// What the form edits. Strings are trimmed on save; an empty one removes the key.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConnectionForm {
    pub name: String,
    pub driver: Option<Driver>,
    pub url: String,
    pub host: String,
    pub port: String,
    pub database: String,
    pub user: String,
    pub password: String,
    pub ssl_mode: Option<SslMode>,
    pub schema: String,
    pub init_sql: String,
    pub color: String,
    pub read_only: bool,
    pub prod: bool,
    /// The keys the form does not own (`ssl.ca`, unknown ones), carried so Test sees them too.
    pub base: Map<String, Value>,
}

impl ConnectionForm {
    /// The public entry: `base` with the form's keys applied (no password).
    fn entry(&self) -> Result<Map<String, Value>, String> {
        let mut m = self.base.clone();
        m.remove("password");
        let mut set = |k: &str, v: &str| {
            match v.trim() {
                "" => m.remove(k),
                v => m.insert(k.into(), v.into()),
            };
        };
        for (k, v) in [("url", &self.url), ("host", &self.host), ("database", &self.database), ("user", &self.user), ("schema", &self.schema), ("init_sql", &self.init_sql), ("color", &self.color)] {
            set(k, v);
        }
        match self.port.trim() {
            "" => m.remove("port"),
            p => m.insert("port".into(), p.parse::<u16>().map_err(|_| format!("port is not a number: '{p}'"))?.into()),
        };
        let driver = self.driver.ok_or("driver is required")?;
        m.insert("driver".into(), if driver == Driver::Postgres { "postgres" } else { "mysql" }.into());
        for (k, on) in [("read_only", self.read_only), ("prod", self.prod)] {
            if on {
                m.insert(k.into(), true.into());
            } else {
                m.remove(k);
            }
        }
        let mut ssl = m.remove("ssl").and_then(|v| if let Value::Object(o) = v { Some(o) } else { None }).unwrap_or_default();
        match self.ssl_mode {
            Some(mode) => ssl.insert("mode".into(), mode_name(mode).into()),
            None => ssl.remove("mode"),
        };
        if !ssl.is_empty() {
            m.insert("ssl".into(), ssl.into());
        }
        Ok(m)
    }

    /// A connection for "Test": the form's values, `${env:}` / `${dotenv:}` resolved against `dir`.
    pub fn to_connection(&self, dir: &Path) -> Result<Connection, String> {
        let mut entry = self.entry()?;
        if !self.password.is_empty() {
            entry.insert("password".into(), self.password.clone().into());
        }
        let name = if self.name.trim().is_empty() { "(unsaved)" } else { self.name.trim() };
        let mut warnings = vec![];
        let fields = read_entry(&entry, dir, true, dir, name, &mut warnings);
        build(name, &fields)
    }
}

type Files = (Map<String, Value>, Map<String, Value>);

fn read_map(path: &Path) -> Result<Map<String, Value>, String> {
    match std::fs::read_to_string(path) {
        Ok(t) if t.trim().is_empty() => Ok(Map::new()),
        Ok(t) => serde_json::from_str(&t).map_err(|e| format!("{}: invalid JSON, not touched: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

fn read_both(dir: &Path) -> Result<Files, String> {
    Ok((read_map(&dir.join(GLOBAL_PUBLIC))?, read_map(&dir.join(GLOBAL_PRIVATE))?))
}

/// Names defined in the global files.
pub fn global_names(dir: &Path) -> Vec<String> {
    let (public, private) = read_both(dir).unwrap_or_default();
    let mut names: Vec<String> = public.keys().chain(private.keys()).cloned().collect();
    names.sort();
    names.dedup();
    names
}

/// The raw (unresolved) values of a global connection, for editing; `None` if it is not there.
pub fn read_global_form(dir: &Path, name: &str) -> Option<ConnectionForm> {
    let (public, private) = read_both(dir).ok()?;
    let (pub_e, priv_e) = (public.get(name).and_then(Value::as_object), private.get(name).and_then(Value::as_object));
    if pub_e.is_none() && priv_e.is_none() {
        return None;
    }
    let mut e = pub_e.cloned().unwrap_or_default();
    for (k, v) in priv_e.into_iter().flatten() {
        e.insert(k.clone(), v.clone());
    }
    let s = |k: &str| match e.get(k) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    };
    let b = |k: &str| e.get(k).and_then(Value::as_bool).unwrap_or(false);
    let ssl = e.get("ssl").and_then(Value::as_object);
    let mut base = pub_e.cloned().unwrap_or_default();
    for k in ["url", "host", "port", "database", "user", "password", "schema", "init_sql", "color", "driver", "read_only", "prod"] {
        base.remove(k);
    }
    if let Some(Value::Object(o)) = base.get_mut("ssl") {
        o.remove("mode");
        if o.is_empty() {
            base.remove("ssl");
        }
    }
    let driver = parse_driver(e.get("driver").and_then(Value::as_str), e.get("url").and_then(Value::as_str)).ok();
    let ssl_mode = ssl.and_then(|o| o.get("mode")).and_then(Value::as_str).and_then(|m| {
        [SslMode::Disable, SslMode::Prefer, SslMode::Require, SslMode::VerifyCa, SslMode::VerifyFull].into_iter().find(|x| mode_name(*x) == m)
    });
    Some(ConnectionForm {
        name: name.into(),
        driver,
        url: s("url"),
        host: s("host"),
        port: s("port"),
        database: s("database"),
        user: s("user"),
        password: s("password"),
        ssl_mode,
        schema: s("schema"),
        init_sql: s("init_sql"),
        color: s("color"),
        read_only: b("read_only"),
        prod: b("prod"),
        base,
    })
}

/// Saves the form into the global files. `original` is the name being edited (a rename moves the
/// entry); `None` for a new connection. Errors are shown in the form.
pub fn save_global(dir: &Path, form: &ConnectionForm, original: Option<&str>) -> Result<(), String> {
    let name = form.name.trim();
    if name.is_empty() {
        return Err("name is required".into());
    }
    let entry = form.entry()?;
    let (mut public, mut private) = read_both(dir)?;
    if Some(name) != original && (public.contains_key(name) || private.contains_key(name)) {
        return Err(format!("a global connection named '{name}' already exists"));
    }
    let from = original.unwrap_or(name);
    // Keys of the old entry the form does not own (unknown ones) survive: `entry()` starts from `base`,
    // which `read_global_form` filled; a fresh form has none.
    public.remove(from);
    public.insert(name.into(), entry.into());

    let mut private_entry = match private.remove(from) {
        Some(Value::Object(o)) => o,
        _ => Map::new(),
    };
    set_password(&mut private_entry, &form.password);
    if !private_entry.is_empty() {
        private.insert(name.into(), private_entry.into());
    }
    write_atomic(&dir.join(GLOBAL_PUBLIC), &public, false)?;
    write_atomic(&dir.join(GLOBAL_PRIVATE), &private, true)
}

/// The one place the password is stored; a keychain / secret service would replace it.
fn set_password(private_entry: &mut Map<String, Value>, password: &str) {
    if password.is_empty() {
        private_entry.remove("password");
    } else {
        private_entry.insert("password".into(), password.into());
    }
}

/// Removes the connection from both global files; whether it was there.
pub fn delete_global(dir: &Path, name: &str) -> Result<bool, String> {
    let (mut public, mut private) = read_both(dir)?;
    let found = public.remove(name).is_some() | private.remove(name).is_some();
    if found {
        write_atomic(&dir.join(GLOBAL_PUBLIC), &public, false)?;
        write_atomic(&dir.join(GLOBAL_PRIVATE), &private, true)?;
    }
    Ok(found)
}

fn write_atomic(path: &Path, map: &Map<String, Value>, private: bool) -> Result<(), String> {
    let err = |e: std::io::Error| format!("{}: {e}", path.display());
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).map_err(err)?;
    let tmp: PathBuf = dir.join(format!(".{}.{}.tmp", path.file_name().unwrap_or_default().to_string_lossy(), std::process::id()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    if private {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let _ = private;
    let text = serde_json::to_string_pretty(map).map_err(|e| e.to_string())? + "\n";
    let result = opts.open(&tmp).and_then(|mut f| f.write_all(text.as_bytes()).and_then(|()| f.sync_all())).and_then(|()| std::fs::rename(&tmp, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result.map_err(err)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn form(name: &str) -> ConnectionForm {
        ConnectionForm { name: name.into(), driver: Some(Driver::Postgres), host: "h".into(), port: "5432".into(), password: "secret".into(), ..Default::default() }
    }

    fn json(p: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(p).unwrap()).unwrap()
    }

    #[test]
    fn save_splits_password_and_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("cfg");
        let mut f = form("a");
        (f.prod, f.read_only, f.ssl_mode, f.schema) = (true, true, Some(SslMode::Require), "app".into());
        save_global(&d, &f, None).unwrap();
        let public = json(&d.join("connections.json"));
        assert_eq!(public["a"], serde_json::json!({"driver": "postgres", "host": "h", "port": 5432, "prod": true, "read_only": true, "schema": "app", "ssl": {"mode": "require"}}));
        assert_eq!(json(&d.join("connections.private.json")), serde_json::json!({"a": {"password": "secret"}}));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(d.join("connections.private.json")).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert_eq!(read_global_form(&d, "a").unwrap(), ConnectionForm { base: Map::new(), ..f.clone() });
        let c = crate::Connections::load(&d.join("q.sql"), &d, &[], Some(&d));
        assert_eq!((c.connections["a"].password.as_deref(), c.connections["a"].prod, c.connections["a"].ssl.mode), (Some("secret"), true, Some(SslMode::Require)));
        assert!(c.warnings.is_empty(), "{:?}", c.warnings);
        assert!(fs::read_dir(&d).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().ends_with(".tmp")));
    }

    #[test]
    fn preserves_others_and_unknown_keys_and_renames() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        fs::write(d.join("connections.json"), r#"{"other": {"driver": "mysql", "x": 1}, "a": {"driver": "postgres", "host": "old", "mystery": 7, "ssl": {"ca": "ca.pem", "mode": "prefer"}, "password": "leak"}}"#).unwrap();
        fs::write(d.join("connections.private.json"), r#"{"other": {"password": "op"}, "a": {"password": "old", "extra": true}}"#).unwrap();
        let mut f = read_global_form(d, "a").unwrap();
        assert_eq!((f.password.as_str(), f.ssl_mode), ("old", Some(SslMode::Prefer)));
        f.name = "b".into();
        f.host = "new".into();
        f.password = String::new();
        f.ssl_mode = None;
        save_global(d, &f, Some("a")).unwrap();
        let (public, private) = (json(&d.join("connections.json")), json(&d.join("connections.private.json")));
        assert!(public.get("a").is_none() && private.get("a").is_none());
        assert_eq!(public["other"], serde_json::json!({"driver": "mysql", "x": 1}));
        assert_eq!(public["b"], serde_json::json!({"driver": "postgres", "host": "new", "mystery": 7, "ssl": {"ca": "ca.pem"}}));
        assert_eq!(private["b"], serde_json::json!({"extra": true}));
        assert_eq!(private["other"]["password"], "op");
        assert_eq!(global_names(d), ["b", "other"]);
    }

    #[test]
    fn validation_and_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        save_global(d, &form("a"), None).unwrap();
        assert!(save_global(d, &form("a"), None).unwrap_err().contains("already exists"));
        assert!(save_global(d, &form(" "), None).unwrap_err().contains("name"));
        let mut f = form("c");
        f.port = "abc".into();
        assert!(save_global(d, &f, None).unwrap_err().contains("port"));
        f.port = String::new();
        f.driver = None;
        assert!(save_global(d, &f, None).unwrap_err().contains("driver"));
        // Editing under the same name is not a clash; an empty password drops the private entry.
        let mut e = form("a");
        e.password = String::new();
        save_global(d, &e, Some("a")).unwrap();
        assert_eq!(json(&d.join("connections.private.json")), serde_json::json!({}));
        assert!(delete_global(d, "a").unwrap() && !delete_global(d, "a").unwrap());
        assert!(global_names(d).is_empty());
        // A broken file is reported, not overwritten.
        fs::write(d.join("connections.json"), "{ nope").unwrap();
        assert!(save_global(d, &form("z"), None).unwrap_err().contains("invalid JSON"));
        assert_eq!(fs::read_to_string(d.join("connections.json")).unwrap(), "{ nope");
    }

    #[test]
    fn to_connection_resolves_and_validates() {
        let tmp = tempfile::tempdir().unwrap();
        let mut f = form("t");
        f.user = "${env:PATH}".into();
        f.prod = true;
        let c = f.to_connection(tmp.path()).unwrap();
        assert_eq!((c.user, c.password.as_deref(), c.prod, c.port), (std::env::var("PATH").ok(), Some("secret"), true, Some(5432)));
        f.port = "x".into();
        assert!(f.to_connection(tmp.path()).is_err());
    }
}
