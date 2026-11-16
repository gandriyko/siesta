//! The global connection files (`connections.json` + `connections.private.json` in the config dir):
//! GUI-free read / modify / write behind the "New / Edit connection" form.
//!
//! Only the keys the form owns are touched; other connections and unknown keys survive. Files are
//! written atomically (temp file + rename), the private one with mode 0600 on unix. Keys come out
//! sorted (serde_json maps are ordered by key).

use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::secrets::{self, SecretError};
use crate::{Connection, Driver, GLOBAL_PRIVATE, GLOBAL_PUBLIC, SslMode, build, keychain_marker, marker_text, mode_name, parse_driver, read_entry};

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
    /// The keychain account the saved connection's password lives under (the form's password field
    /// stays empty then; empty on save = keep it). Set by `read_global_form`.
    pub keychain: Option<String>,
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
        let mut c = build(name, &fields)?;
        if self.password.is_empty() {
            c.keychain = self.keychain.clone();
        }
        Ok(c)
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
    let marker = e.get("password").and_then(Value::as_str).and_then(keychain_marker);
    let keychain = marker.map(|a| a.unwrap_or(name).to_string());
    Some(ConnectionForm {
        keychain,
        name: name.into(),
        driver,
        url: s("url"),
        host: s("host"),
        port: s("port"),
        database: s("database"),
        user: s("user"),
        password: if marker.is_some() { String::new() } else { s("password") },
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
///
/// The password goes to the system keychain (the file gets the `"${keychain}"` marker); an empty
/// field keeps the stored one, a rename moves it, a plain one in `connections.private.json` is
/// migrated. When the keychain is unavailable the password falls back to `connections.private.json`
/// and the returned `Some(warning)` tells the form to say so.
pub fn save_global(dir: &Path, form: &ConnectionForm, original: Option<&str>) -> Result<Option<String>, String> {
    let name = form.name.trim();
    if name.is_empty() {
        return Err("name is required".into());
    }
    let mut entry = form.entry()?;
    let (mut public, mut private) = read_both(dir)?;
    if Some(name) != original && (public.contains_key(name) || private.contains_key(name)) {
        return Err(format!("a global connection named '{name}' already exists"));
    }
    let from = original.unwrap_or(name);
    let old_marker = marker_of(&public, &private, from);
    // An explicit `${keychain:ACCOUNT}` keeps its account; the default one follows the name.
    let (old_account, account) = match &old_marker {
        Some(Some(a)) => (a.clone(), a.clone()),
        _ => (from.to_string(), name.to_string()),
    };
    // Keys of the old entry the form does not own (unknown ones) survive: `entry()` starts from `base`,
    // which `read_global_form` filled; a fresh form has none.
    public.remove(from);
    let mut private_entry = match private.remove(from) {
        Some(Value::Object(o)) => o,
        _ => Map::new(),
    };
    private_entry.remove("password");
    let mut warning = None;
    let keep = |entry: &mut Map<String, Value>| entry.insert("password".into(), marker_text(&account, name).into());
    if !form.password.is_empty() {
        match secrets::set(&account, &form.password) {
            Ok(()) => drop(keep(&mut entry)),
            Err(SecretError::Unavailable(_)) => {
                private_entry.insert("password".into(), form.password.clone().into());
                warning = Some(format!("System keychain unavailable: password saved to {}", dir.join(GLOBAL_PRIVATE).display()));
            }
            Err(e) => return Err(e.to_string()),
        }
    } else if old_marker.is_some() {
        if old_account != account {
            match secrets::get(&old_account) {
                Ok(p) => {
                    secrets::set(&account, &p).map_err(|e| e.to_string())?;
                    secrets::delete(&old_account).map_err(|e| e.to_string())?;
                }
                Err(SecretError::Missing) => {}
                Err(e) => return Err(e.to_string()),
            }
        }
        keep(&mut entry);
    }
    public.insert(name.into(), entry.into());
    if !private_entry.is_empty() {
        private.insert(name.into(), private_entry.into());
    }
    write_atomic(&dir.join(GLOBAL_PUBLIC), &public, false)?;
    write_atomic(&dir.join(GLOBAL_PRIVATE), &private, true)?;
    Ok(warning)
}

/// The `${keychain}` marker of `name`'s password in either file: `Some(None)` default account,
/// `Some(Some(account))` explicit.
fn marker_of(public: &Map<String, Value>, private: &Map<String, Value>, name: &str) -> Option<Option<String>> {
    [public, private].into_iter().find_map(|m| m.get(name)?.get("password")?.as_str().and_then(keychain_marker)).map(|a| a.map(String::from))
}

/// Removes the connection from both global files and its keychain entry (best effort: a missing
/// entry or an unavailable keychain does not fail the delete); whether it was there.
pub fn delete_global(dir: &Path, name: &str) -> Result<bool, String> {
    let (mut public, mut private) = read_both(dir)?;
    let marker = marker_of(&public, &private, name);
    let found = public.remove(name).is_some() | private.remove(name).is_some();
    if found {
        write_atomic(&dir.join(GLOBAL_PUBLIC), &public, false)?;
        write_atomic(&dir.join(GLOBAL_PRIVATE), &private, true)?;
        if let Some(a) = marker {
            let _ = secrets::delete(a.as_deref().unwrap_or(name));
        }
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
        secrets::use_memory_store();
        ConnectionForm { name: name.into(), driver: Some(Driver::Postgres), host: "h".into(), port: "5432".into(), password: "secret".into(), ..Default::default() }
    }

    fn json(p: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(p).unwrap()).unwrap()
    }

    #[test]
    fn save_stores_password_in_keychain_and_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path().join("cfg");
        let mut f = form("kc-save");
        (f.prod, f.read_only, f.ssl_mode, f.schema) = (true, true, Some(SslMode::Require), "app".into());
        assert_eq!(save_global(&d, &f, None).unwrap(), None);
        let public = json(&d.join("connections.json"));
        assert_eq!(public["kc-save"], serde_json::json!({"driver": "postgres", "host": "h", "port": 5432, "prod": true, "read_only": true, "schema": "app", "ssl": {"mode": "require"}, "password": "${keychain}"}));
        assert_eq!(json(&d.join("connections.private.json")), serde_json::json!({}));
        assert_eq!(secrets::get("kc-save").as_deref(), Ok("secret"));
        // The form comes back with an empty password field and the account.
        assert_eq!(read_global_form(&d, "kc-save").unwrap(), ConnectionForm { base: Map::new(), password: String::new(), keychain: Some("kc-save".into()), ..f.clone() });
        // Loading never reads the keychain; resolving does.
        let c = crate::Connections::load(&d.join("q.sql"), &d, &[], Some(&d));
        assert!(c.warnings.is_empty(), "{:?}", c.warnings);
        let c = &c.connections["kc-save"];
        assert_eq!((c.password.as_deref(), c.keychain.as_deref(), c.prod, c.ssl.mode), (None, Some("kc-save"), true, Some(SslMode::Require)));
        assert_eq!(c.resolve_password().unwrap().password.as_deref(), Some("secret"));
        assert!(fs::read_dir(&d).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().ends_with(".tmp")));
    }

    #[test]
    fn edit_keeps_renames_and_deletes_the_keychain_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        save_global(d, &form("kc-old"), None).unwrap();
        // Edit with an empty password field: the stored one stays.
        let mut f = read_global_form(d, "kc-old").unwrap();
        f.host = "other".into();
        save_global(d, &f, Some("kc-old")).unwrap();
        assert_eq!(secrets::get("kc-old").as_deref(), Ok("secret"));
        assert_eq!(json(&d.join("connections.json"))["kc-old"]["password"], "${keychain}");
        // Rename moves the entry.
        f.name = "kc-new".into();
        save_global(d, &f, Some("kc-old")).unwrap();
        assert_eq!((secrets::get("kc-old"), secrets::get("kc-new").as_deref()), (Err(secrets::SecretError::Missing), Ok("secret")));
        assert_eq!(json(&d.join("connections.json"))["kc-new"]["password"], "${keychain}");
        // A new password replaces it.
        f.password = "fresh".into();
        save_global(d, &f, Some("kc-new")).unwrap();
        assert_eq!(secrets::get("kc-new").as_deref(), Ok("fresh"));
        // Delete removes it (a second delete of the missing entry is fine).
        assert!(delete_global(d, "kc-new").unwrap());
        assert_eq!(secrets::get("kc-new"), Err(secrets::SecretError::Missing));
        assert!(!delete_global(d, "kc-new").unwrap());
    }

    #[test]
    fn explicit_account_is_kept() {
        secrets::use_memory_store();
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        fs::write(d.join("connections.json"), r#"{"kc-team": {"driver": "postgres", "host": "h", "password": "${keychain:kc-team-acct}"}}"#).unwrap();
        let mut f = read_global_form(d, "kc-team").unwrap();
        assert_eq!(f.keychain.as_deref(), Some("kc-team-acct"));
        f.password = "pw".into();
        f.name = "kc-team2".into();
        save_global(d, &f, Some("kc-team")).unwrap();
        assert_eq!(secrets::get("kc-team-acct").as_deref(), Ok("pw"));
        assert_eq!(json(&d.join("connections.json"))["kc-team2"]["password"], "${keychain:kc-team-acct}");
        delete_global(d, "kc-team2").unwrap();
        assert_eq!(secrets::get("kc-team-acct"), Err(secrets::SecretError::Missing));
    }

    #[test]
    fn plain_private_password_is_migrated_on_edit() {
        secrets::use_memory_store();
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        fs::write(d.join("connections.json"), r#"{"kc-mig": {"driver": "postgres", "host": "h"}}"#).unwrap();
        fs::write(d.join("connections.private.json"), r#"{"kc-mig": {"password": "legacy"}}"#).unwrap();
        let f = read_global_form(d, "kc-mig").unwrap();
        assert_eq!((f.password.as_str(), f.keychain.as_deref()), ("legacy", None));
        save_global(d, &f, Some("kc-mig")).unwrap();
        assert_eq!(secrets::get("kc-mig").as_deref(), Ok("legacy"));
        assert_eq!(json(&d.join("connections.json"))["kc-mig"]["password"], "${keychain}");
        assert_eq!(json(&d.join("connections.private.json")), serde_json::json!({}));
    }

    #[test]
    fn unavailable_keychain_falls_back_to_the_private_file() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        let f = form("kc-off");
        secrets::set_unavailable(true);
        let w = save_global(d, &f, None);
        secrets::set_unavailable(false);
        let w = w.unwrap().expect("a warning");
        assert!(w.starts_with("System keychain unavailable: password saved to ") && w.contains("connections.private.json"), "{w}");
        assert_eq!(json(&d.join("connections.private.json")), serde_json::json!({"kc-off": {"password": "secret"}}));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(fs::metadata(d.join("connections.private.json")).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert!(json(&d.join("connections.json"))["kc-off"].get("password").is_none());
        assert_eq!(secrets::get("kc-off"), Err(secrets::SecretError::Missing));
        let c = crate::Connections::load(&d.join("q.sql"), d, &[], Some(d));
        assert_eq!(c.connections["kc-off"].password.as_deref(), Some("secret"));
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
