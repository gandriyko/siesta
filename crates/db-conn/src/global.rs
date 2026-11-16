//! Reading, modifying and writing the per-connection files: GUI-free logic behind the "New / Edit
//! connection" form and the CLI.
//!
//! Only the keys the form owns are touched; unknown keys survive. Files are written atomically
//! (temp file + rename), keys come out sorted (serde_json maps are ordered by key).

use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::secrets::{self, SecretError};
use crate::{Connection, Driver, SslMode, connections_dir, mode_name, parse, parse_driver, parse_url, valid_name};

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
    /// A new password to store; empty keeps the keychain entry.
    pub password: String,
    pub ssl_mode: Option<SslMode>,
    pub schema: String,
    pub init_sql: String,
    pub color: String,
    pub read_only: bool,
    pub prod: bool,
    /// The keychain holds a password for the connection (set by `read_form`; the form shows a placeholder).
    pub has_password: bool,
    /// The keys the form does not own (`ssl.ca`, unknown ones), carried so Test sees them too.
    pub base: Map<String, Value>,
}

impl ConnectionForm {
    /// The file's content: `base` with the form's keys applied.
    fn entry(&self) -> Result<Map<String, Value>, String> {
        let mut m = self.base.clone();
        let mut set = |k: &str, v: &str| {
            match v.trim() {
                "" => m.remove(k),
                v => m.insert(k.into(), v.into()),
            };
        };
        // The `url` field only fills the other fields in the form; it is never written (no password in JSON).
        for (k, v) in [("host", &self.host), ("database", &self.database), ("user", &self.user), ("schema", &self.schema), ("init_sql", &self.init_sql), ("color", &self.color)] {
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

    /// A connection for "Test": the form's values with `${env:}` resolved, and the typed password
    /// or else the stored one of `original` (the name being edited).
    pub fn to_connection(&self, config: &Path, original: Option<&str>) -> Result<Connection, String> {
        let name = if self.name.trim().is_empty() { "(unsaved)" } else { self.name.trim() };
        let mut c = parse(name, &self.entry()?, &connections_dir(config), &mut vec![])?;
        c.password = match (self.password.is_empty(), original) {
            (false, _) => Some(self.password.clone()),
            (true, Some(o)) => secrets::get(o).ok(),
            (true, None) => None,
        };
        Ok(c)
    }
}

fn path_of(config: &Path, name: &str) -> PathBuf {
    connections_dir(config).join(format!("{name}.json"))
}

/// Names of the connection files, sorted.
pub fn names(config: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(connections_dir(config))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_file())
        .filter_map(|e| e.file_name().to_str()?.strip_suffix(".json").map(String::from))
        .filter(|n| valid_name(n).is_ok())
        .collect();
    v.sort();
    v
}

/// The raw (unresolved) values of a connection, for editing; `None` if it is not there or broken.
pub fn read_form(config: &Path, name: &str) -> Option<ConnectionForm> {
    let Value::Object(e) = serde_json::from_str(&std::fs::read_to_string(path_of(config, name)).ok()?).ok()? else { return None };
    let s = |k: &str| match e.get(k) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Number(n)) => n.to_string(),
        _ => String::new(),
    };
    let b = |k: &str| e.get(k).and_then(Value::as_bool).unwrap_or(false);
    let mut base = e.clone();
    for k in ["host", "port", "database", "user", "schema", "init_sql", "color", "driver", "read_only", "prod", "password"] {
        base.remove(k);
    }
    if let Some(Value::Object(o)) = base.get_mut("ssl") {
        o.remove("mode");
        if o.is_empty() {
            base.remove("ssl");
        }
    }
    let ssl_mode = e.get("ssl").and_then(|o| o.get("mode")).and_then(Value::as_str).and_then(|m| {
        [SslMode::Disable, SslMode::Prefer, SslMode::Require, SslMode::VerifyCa, SslMode::VerifyFull].into_iter().find(|x| mode_name(*x) == m)
    });
    // A hand-written `url` is split into the fields (explicit keys win); saving rewrites the file without it.
    // One that does not parse stays in `base`, so saving does not lose it.
    let p = parse_url(&s("url")).ok();
    if p.is_some() {
        base.remove("url");
    }
    let p = p.unwrap_or_default();
    let or = |k: &str, v: String| Some(s(k)).filter(|x| !x.is_empty()).unwrap_or(v);
    Some(ConnectionForm {
        name: name.into(),
        driver: parse_driver(e.get("driver").and_then(Value::as_str), e.get("url").and_then(Value::as_str)).ok(),
        url: String::new(),
        host: or("host", p.host),
        port: or("port", p.port.map(|p| p.to_string()).unwrap_or_default()),
        database: or("database", p.database),
        user: or("user", p.user),
        password: p.password,
        ssl_mode: ssl_mode.or(p.ssl_mode),
        schema: s("schema"),
        init_sql: s("init_sql"),
        color: s("color"),
        read_only: b("read_only"),
        prod: b("prod"),
        has_password: secrets::get(name).is_ok(),
        base,
    })
}

/// Saves the form as `<name>.json`. `original` is the name being edited (a rename moves the file
/// and the keychain entry); `None` for a new connection. `Err` means nothing was saved. `Ok(Some(e))`
/// means the file is saved but the password could not be (keychain unavailable, denied): the form
/// shows `e` and stays open.
pub fn save(config: &Path, form: &ConnectionForm, original: Option<&str>) -> Result<Option<String>, String> {
    let name = form.name.trim();
    valid_name(name)?;
    let entry = form.entry()?;
    let renamed = original.filter(|o| *o != name);
    let (path, old) = (path_of(config, name), renamed.map(|o| path_of(config, o)));
    // The same file under a differently-cased name (case-insensitive file systems) is a rename, not a clash.
    if path.exists() && !renamed.is_some_and(|o| o.eq_ignore_ascii_case(name)) && Some(name) != original {
        return Err(format!("a connection named '{name}' already exists"));
    }
    if let Some(old) = old.filter(|o| o.exists()) {
        std::fs::create_dir_all(connections_dir(config)).map_err(|e| format!("{}: {e}", old.display()))?;
        std::fs::rename(&old, &path).map_err(|e| format!("{}: {e}", old.display()))?;
    }
    write_atomic(&path, &entry)?;
    Ok(match (form.password.as_str(), renamed) {
        ("", Some(o)) => move_secret(o, name).err(),
        ("", None) => None,
        (pw, renamed) => secrets::set(name, pw).err().map(|e| e.to_string()).or_else(|| {
            // A new password under a new name: the old entry is stale.
            renamed.and_then(|o| secrets::delete(o).err()).map(|e| e.to_string())
        }),
    })
}

/// Moves the keychain entry of `from` to `to`; a missing one is fine.
fn move_secret(from: &str, to: &str) -> Result<(), String> {
    match secrets::get(from) {
        Ok(p) => {
            secrets::set(to, &p).map_err(|e| e.to_string())?;
            secrets::delete(from).map_err(|e| e.to_string())
        }
        Err(SecretError::Missing) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

/// Removes the connection's file and its keychain entry (best effort: a missing entry or an
/// unavailable keychain does not fail the delete); whether the file was there.
pub fn delete(config: &Path, name: &str) -> Result<bool, String> {
    valid_name(name)?;
    let path = path_of(config, name);
    let found = match std::fs::remove_file(&path) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    if found {
        let _ = secrets::delete(name);
    }
    Ok(found)
}

pub(crate) fn write_atomic(path: &Path, map: &Map<String, Value>) -> Result<(), String> {
    let err = |e: std::io::Error| format!("{}: {e}", path.display());
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).map_err(err)?;
    let tmp: PathBuf = dir.join(format!(".{}.{}.tmp", path.file_name().unwrap_or_default().to_string_lossy(), std::process::id()));
    let text = serde_json::to_string_pretty(map).map_err(|e| e.to_string())? + "\n";
    let result = std::fs::File::create(&tmp).and_then(|mut f| f.write_all(text.as_bytes()).and_then(|()| f.sync_all())).and_then(|()| std::fs::rename(&tmp, path));
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
        secrets::use_memory_store();
        ConnectionForm { name: name.into(), driver: Some(Driver::Postgres), host: "h".into(), port: "5432".into(), password: "secret".into(), ..Default::default() }
    }

    fn json(c: &Path, name: &str) -> Value {
        serde_json::from_str(&fs::read_to_string(path_of(c, name)).unwrap()).unwrap()
    }

    #[test]
    fn save_writes_the_file_without_a_password_and_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        let mut f = form("kc-save");
        (f.prod, f.read_only, f.ssl_mode, f.schema) = (true, true, Some(SslMode::Require), "app".into());
        assert_eq!(save(d, &f, None).unwrap(), None);
        assert_eq!(json(d, "kc-save"), serde_json::json!({"driver": "postgres", "host": "h", "port": 5432, "prod": true, "read_only": true, "schema": "app", "ssl": {"mode": "require"}}));
        assert_eq!(secrets::get("kc-save").as_deref(), Ok("secret"));
        // The form comes back with an empty password field and the "stored" flag.
        assert_eq!(read_form(d, "kc-save").unwrap(), ConnectionForm { password: String::new(), has_password: true, ..f.clone() });
        let c = crate::Connections::load(d);
        assert!(c.warnings.is_empty(), "{:?}", c.warnings);
        let c = &c.connections["kc-save"];
        assert_eq!((c.password.as_deref(), c.prod, c.ssl.mode), (None, true, Some(SslMode::Require)));
        assert_eq!(c.resolve_password().unwrap().password.as_deref(), Some("secret"));
        assert!(fs::read_dir(connections_dir(d)).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().ends_with(".tmp")));
    }

    #[test]
    fn edit_keeps_renames_and_deletes_the_keychain_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        save(d, &form("kc-old"), None).unwrap();
        // Edit with an empty password field: the stored one stays.
        let mut f = read_form(d, "kc-old").unwrap();
        f.host = "other".into();
        save(d, &f, Some("kc-old")).unwrap();
        assert_eq!(secrets::get("kc-old").as_deref(), Ok("secret"));
        // Rename moves the file and the entry.
        f.name = "kc-new".into();
        assert_eq!(save(d, &f, Some("kc-old")).unwrap(), None);
        assert_eq!((secrets::get("kc-old"), secrets::get("kc-new").as_deref()), (Err(SecretError::Missing), Ok("secret")));
        assert_eq!(names(d), ["kc-new"]);
        assert_eq!(json(d, "kc-new")["host"], "other");
        // A new password replaces it, also together with a rename.
        f.password = "fresh".into();
        save(d, &f, Some("kc-new")).unwrap();
        assert_eq!(secrets::get("kc-new").as_deref(), Ok("fresh"));
        f.name = "kc-newer".into();
        save(d, &f, Some("kc-new")).unwrap();
        assert_eq!((secrets::get("kc-new"), secrets::get("kc-newer").as_deref()), (Err(SecretError::Missing), Ok("fresh")));
        // Delete removes both (a second delete of the missing file is fine).
        assert!(delete(d, "kc-newer").unwrap());
        assert_eq!(secrets::get("kc-newer"), Err(SecretError::Missing));
        assert!(!delete(d, "kc-newer").unwrap());
    }

    #[test]
    fn unavailable_keychain_saves_the_rest_and_reports() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        let f = form("kc-off");
        secrets::set_unavailable(true);
        let r = save(d, &f, None);
        secrets::set_unavailable(false);
        let e = r.unwrap().expect("a password error");
        assert!(e.contains("keychain unavailable"), "{e}");
        assert_eq!(json(d, "kc-off")["host"], "h");
        assert!(json(d, "kc-off").get("password").is_none());
        assert_eq!(secrets::get("kc-off"), Err(SecretError::Missing));
    }

    #[test]
    fn preserves_unknown_keys_and_drops_a_stray_password() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        fs::create_dir_all(connections_dir(d)).unwrap();
        fs::write(path_of(d, "a"), r#"{"driver": "postgres", "host": "old", "mystery": 7, "ssl": {"ca": "ca.pem", "mode": "prefer"}, "password": "leak"}"#).unwrap();
        let mut f = read_form(d, "a").unwrap();
        assert_eq!((f.password.as_str(), f.ssl_mode), ("", Some(SslMode::Prefer)));
        f.host = "new".into();
        f.ssl_mode = None;
        save(d, &f, Some("a")).unwrap();
        assert_eq!(json(d, "a"), serde_json::json!({"driver": "postgres", "host": "new", "mystery": 7, "ssl": {"ca": "ca.pem"}}));
    }

    #[test]
    fn validation() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        save(d, &form("a"), None).unwrap();
        assert!(save(d, &form("a"), None).unwrap_err().contains("already exists"));
        assert!(save(d, &form(" "), None).unwrap_err().contains("name"));
        assert!(save(d, &form("../x"), None).unwrap_err().contains("invalid"));
        assert!(save(d, &form("b"), Some("a")).is_ok() && save(d, &form("a"), None).is_ok());
        assert!(save(d, &form("b"), Some("a")).unwrap_err().contains("already exists"));
        let mut f = form("c");
        f.port = "abc".into();
        assert!(save(d, &f, None).unwrap_err().contains("port"));
        f.port = String::new();
        f.driver = None;
        assert!(save(d, &f, None).unwrap_err().contains("driver"));
        assert_eq!(names(d), ["a", "b"]);
        assert!(delete(d, "../a").is_err());
    }

    #[test]
    fn to_connection_resolves_and_uses_the_typed_or_stored_password() {
        let tmp = tempfile::tempdir().unwrap();
        let mut f = form("kc-test");
        f.user = "${env:PATH}".into();
        f.prod = true;
        let c = f.to_connection(tmp.path(), None).unwrap();
        assert_eq!((c.user, c.password.as_deref(), c.prod, c.port), (std::env::var("PATH").ok(), Some("secret"), true, Some(5432)));
        secrets::set("kc-stored", "kept").unwrap();
        f.password = String::new();
        assert_eq!(f.to_connection(tmp.path(), Some("kc-stored")).unwrap().password.as_deref(), Some("kept"));
        assert_eq!(f.to_connection(tmp.path(), None).unwrap().password, None);
        f.port = "x".into();
        assert!(f.to_connection(tmp.path(), None).is_err());
    }
}
