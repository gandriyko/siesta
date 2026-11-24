//! Reading, modifying and writing the per-connection files: GUI-free logic behind the "New / Edit
//! connection" form and the CLI.
//!
//! Only the keys the form owns are touched; unknown keys survive. Files are written atomically
//! (temp file + rename), keys come out sorted (serde_json maps are ordered by key).

use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::{MAX_NAME, Connection, crypto, Driver, SslMode, connections_dir, mode_name, parse, parse_driver, parse_url, valid_file_name, valid_name, normalize_name};

/// What the form edits. Strings are trimmed on save; an empty one removes the key.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConnectionForm {
    pub name: String,
    pub driver: Option<Driver>,
    pub url: String,
    pub host: String,
    /// Connect through a Unix socket: `socket_path` (empty: detected at connect time) replaces `host`.
    pub socket: bool,
    pub socket_path: String,
    pub port: String,
    pub database: String,
    pub user: String,
    /// A new password to store (encrypted); empty keeps the stored one.
    pub password: String,
    pub ssl_mode: Option<SslMode>,
    pub schema: String,
    pub init_sql: String,
    pub color: String,
    pub read_only: bool,
    pub prod: bool,
    /// The file holds a `password_enc` (set by `read_form`; the form shows a placeholder).
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
        // With a socket the host is ignored, so it is not written; without one, `socket` goes away.
        m.remove("socket");
        if self.socket {
            m.remove("host");
            match self.socket_path.trim() {
                "" => m.insert("socket".into(), true.into()),
                p => m.insert("socket".into(), p.into()),
            };
        }
        match self.port.trim() {
            "" => m.remove("port"),
            p => m.insert("port".into(), p.parse::<u16>().map_err(|_| format!("port is not a number: '{p}'"))?.into()),
        };
        let driver = self.driver.ok_or("driver is required")?;
        m.insert("driver".into(), driver.name().into());
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
    pub fn to_connection(&self, config: &Path, keys: &Path, original: Option<&str>) -> Result<Connection, String> {
        let name = if self.name.trim().is_empty() { "(unsaved)" } else { self.name.trim() };
        let mut c = parse(name, &self.entry()?, &connections_dir(config), &mut vec![])?;
        c.password = match (self.password.is_empty(), original) {
            (false, _) => Some(self.password.clone()),
            (true, Some(o)) => stored_enc(config, o).and_then(|e| crypto::decrypt(keys, o, &e).ok()),
            (true, None) => None,
        };
        Ok(c)
    }
}

fn path_of(config: &Path, name: &str) -> PathBuf {
    connections_dir(config).join(format!("{name}.json"))
}

/// The `password_enc` of a connection file, if any.
fn stored_enc(config: &Path, name: &str) -> Option<String> {
    let Value::Object(e) = serde_json::from_str(&std::fs::read_to_string(path_of(config, name)).ok()?).ok()? else { return None };
    e.get("password_enc")?.as_str().map(String::from)
}

/// Whether the connection file holds a password (no decryption, no key needed).
pub fn has_password(config: &Path, name: &str) -> bool {
    stored_enc(config, name).is_some()
}

/// Encrypts `password` into the existing connection file `name` (the rest of the file stays).
pub fn set_password(config: &Path, keys: &Path, name: &str, password: &str) -> Result<(), String> {
    valid_file_name(name)?;
    let path = path_of(config, name);
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let Ok(Value::Object(mut e)) = serde_json::from_str(&text) else { return Err(format!("{}: not a JSON object", path.display())) };
    e.insert("password_enc".into(), crypto::encrypt(keys, name, password)?.into());
    write_atomic(&path, &e)
}

/// Names of the connection files, sorted.
pub fn names(config: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(connections_dir(config))
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_file())
        .filter_map(|e| e.file_name().to_str()?.strip_suffix(".json").map(String::from))
        .filter(|n| valid_file_name(n).is_ok())
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
    for k in ["host", "socket", "port", "database", "user", "schema", "init_sql", "color", "driver", "read_only", "prod", "password", "password_enc"] {
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
    // `socket`: true or a path; a `host` that is a path (legacy) or a socket in the `url` counts too.
    let (mut socket, mut socket_path) = match e.get("socket") {
        Some(Value::Bool(true)) => (true, String::new()),
        Some(Value::String(p)) => (true, p.trim().to_string()),
        _ => (false, String::new()),
    };
    let mut host = or_host(&s("host"), &p.host);
    if !socket {
        if host.starts_with('/') {
            (socket, socket_path, host) = (true, std::mem::take(&mut host), String::new());
        } else if !p.socket.is_empty() && s("host").is_empty() {
            (socket, socket_path) = (true, p.socket.clone());
        }
    }
    let or = |k: &str, v: String| Some(s(k)).filter(|x| !x.is_empty()).unwrap_or(v);
    Some(ConnectionForm {
        name: name.into(),
        driver: parse_driver(e.get("driver").and_then(Value::as_str), e.get("url").and_then(Value::as_str)).ok(),
        url: String::new(),
        host,
        socket,
        socket_path,
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
        has_password: e.get("password_enc").is_some_and(Value::is_string),
        base,
    })
}

fn or_host(own: &str, parsed: &str) -> String {
    if own.is_empty() { parsed.to_string() } else { own.to_string() }
}

/// Saves the form as `<name>.json`. `original` is the name being edited (a rename moves the file
/// and re-encrypts the password for the new name); `None` for a new connection. `Err` means nothing
/// was saved. `Ok(Some(e))` means the file is saved but the password could not be (key unreadable,
/// old password undecryptable): the form shows `e` and stays open.
pub fn save(config: &Path, keys: &Path, form: &ConnectionForm, original: Option<&str>) -> Result<Option<String>, String> {
    let name = form.name.trim();
    valid_name(name)?;
    let mut entry = form.entry()?;
    let renamed = original.filter(|o| *o != name);
    let old_enc = original.and_then(|o| stored_enc(config, o));
    let enc = match (form.password.as_str(), old_enc) {
        ("", None) => Ok(None),
        ("", Some(e)) if renamed.is_none() => Ok(Some(e)),
        ("", Some(e)) => crypto::decrypt(keys, renamed.unwrap_or_default(), &e).and_then(|p| crypto::encrypt(keys, name, &p)).map(Some),
        (pw, _) => crypto::encrypt(keys, name, pw).map(Some),
    };
    let (enc, warn) = match enc {
        Ok(e) => (e, None),
        Err(e) => (None, Some(e)),
    };
    if let Some(e) = enc {
        entry.insert("password_enc".into(), e.into());
    }
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
    Ok(warn)
}

/// Renames every connection file whose name breaks [`valid_name`] to `normalize_name(old)` (`connection`
/// if that is empty; `-2`, `-3`, … if the name is taken) and re-encrypts its password for the new
/// name. Returns the `(old, new)` pairs done and the warnings (a password that could not move, a file
/// that could not be renamed: it stays under its old name and loads with a warning). Idempotent.
pub fn migrate_names(config: &Path, keys: &Path) -> (Vec<(String, String)>, Vec<String>) {
    let all = names(config);
    let mut taken: std::collections::BTreeSet<String> = all.iter().cloned().collect();
    let (mut done, mut warnings) = (Vec::new(), Vec::new());
    for old in all.iter().filter(|n| valid_name(n).is_err()) {
        let base = Some(normalize_name(old)).filter(|b| !b.is_empty()).unwrap_or_else(|| "connection".into());
        let free = |n: &str| !taken.contains(n) && (!path_of(config, n).exists() || old.eq_ignore_ascii_case(n));
        let mut new = base.clone();
        let mut i = 2;
        while !free(&new) {
            let suffix = format!("-{i}");
            new = format!("{}{suffix}", &base[..base.len().min(MAX_NAME - suffix.len())]);
            i += 1;
        }
        let (from, to) = (path_of(config, old), path_of(config, &new));
        let Ok(Value::Object(mut obj)) = std::fs::read_to_string(&from).map_err(|e| e.to_string()).and_then(|t| serde_json::from_str(&t).map_err(|e| e.to_string())) else {
            warnings.push(format!("connection '{old}': cannot be read, not renamed"));
            continue;
        };
        if let Err(e) = std::fs::rename(&from, &to) {
            warnings.push(format!("connection '{old}': cannot be renamed to '{new}': {e}"));
            continue;
        }
        if let Some(enc) = obj.get("password_enc").and_then(Value::as_str).map(String::from) {
            match crypto::decrypt(keys, old, &enc).and_then(|p| crypto::encrypt(keys, &new, &p)) {
                Ok(e) => {
                    obj.insert("password_enc".into(), e.into());
                }
                Err(_) => {
                    obj.remove("password_enc");
                    warnings.push(format!("connection '{old}' renamed to '{new}'; its password could not be moved, set it again"));
                }
            }
            if let Err(e) = write_atomic(&to, &obj) {
                warnings.push(e);
            }
        }
        taken.remove(old);
        taken.insert(new.clone());
        done.push((old.clone(), new));
    }
    (done, warnings)
}

/// Removes the connection's file (the password goes with it); whether the file was there.
pub fn delete(config: &Path, name: &str) -> Result<bool, String> {
    valid_file_name(name)?;
    let path = path_of(config, name);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
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
        ConnectionForm { name: name.into(), driver: Some(Driver::Postgres), host: "h".into(), port: "5432".into(), password: "secret".into(), ..Default::default() }
    }

    fn json(c: &Path, name: &str) -> Value {
        serde_json::from_str(&fs::read_to_string(path_of(c, name)).unwrap()).unwrap()
    }

    #[test]
    fn migrate_renames_legacy_files_and_keeps_passwords() {
        let tmp = tempfile::tempdir().unwrap();
        let (d, keys) = (&tmp.path().join("config"), &tmp.path().join("data"));
        fs::create_dir_all(connections_dir(d)).unwrap();
        let enc = |n: &str| crypto::encrypt(keys, n, "pw").unwrap();
        let put = |n: &str, pw: Option<String>| {
            let mut m = Map::new();
            m.insert("driver".into(), "postgres".into());
            if let Some(p) = pw {
                m.insert("password_enc".into(), p.into());
            }
            write_atomic(&path_of(d, n), &m).unwrap();
        };
        put("My DB", Some(enc("My DB")));
        put("my-db", None);
        put("ok", None);
        put("!!!", None);
        put("Bad pw", Some("v1:garbage".into()));
        let (done, warns) = migrate_names(d, keys);
        assert!(done.contains(&("My DB".into(), "my-db-2".into())) && done.contains(&("!!!".into(), "connection".into())) && done.contains(&("Bad pw".into(), "bad-pw".into())), "{done:?}");
        assert_eq!(done.len(), 3);
        assert_eq!(warns.len(), 1, "{warns:?}");
        assert!(warns[0].contains("'bad-pw'") && json(d, "bad-pw").get("password_enc").is_none());
        assert!(!path_of(d, "My DB").exists() || path_of(d, "My DB").canonicalize().ok() == path_of(d, "my-db-2").canonicalize().ok());
        let moved = crate::Connections::load(d).connections.remove("my-db-2").unwrap();
        assert_eq!(moved.resolve_password_in(keys).unwrap().password.as_deref(), Some("pw"));
        assert!(json(d, "ok").is_object());
        // Second run: nothing left to do.
        assert_eq!(migrate_names(d, keys), (vec![], vec![]));
    }

    #[test]
    fn socket_roundtrips_through_the_form() {
        let tmp = tempfile::tempdir().unwrap();
        let (d, keys) = (&tmp.path().join("config"), &tmp.path().join("data"));
        let mut f = form("kc-sock");
        f.socket = true;
        save(d, keys, &f, None).unwrap();
        assert_eq!(json(d, "kc-sock")["socket"], true);
        assert!(json(d, "kc-sock").get("host").is_none());
        let r = read_form(d, "kc-sock").unwrap();
        assert!(r.socket && r.socket_path.is_empty());
        f.socket_path = "/var/x".into();
        save(d, keys, &f, Some("kc-sock")).unwrap();
        assert_eq!(json(d, "kc-sock")["socket"], "/var/x");
        f.socket = false;
        save(d, keys, &f, Some("kc-sock")).unwrap();
        assert!(json(d, "kc-sock").get("socket").is_none());
        assert_eq!(json(d, "kc-sock")["host"], "h");
        fs::write(path_of(d, "old"), r#"{"driver": "postgres", "host": "/tmp"}"#).unwrap();
        let r = read_form(d, "old").unwrap();
        assert_eq!((r.socket, r.socket_path.as_str(), r.host.as_str()), (true, "/tmp", ""));
    }

    #[test]
    fn save_writes_the_file_with_an_encrypted_password_and_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let (d, keys) = (&tmp.path().join("config"), &tmp.path().join("data"));
        let mut f = form("kc-save");
        (f.prod, f.read_only, f.ssl_mode, f.schema) = (true, true, Some(SslMode::Require), "app".into());
        assert_eq!(save(d, keys, &f, None).unwrap(), None);
        // The key goes to the data dir, never next to the connection files.
        assert!(crypto::key_path(keys).is_file() && !crypto::key_path(d).exists());
        let mut j = json(d, "kc-save");
        let enc = j.as_object_mut().unwrap().remove("password_enc").unwrap();
        assert_eq!(j, serde_json::json!({"driver": "postgres", "host": "h", "port": 5432, "prod": true, "read_only": true, "schema": "app", "ssl": {"mode": "require"}}));
        assert!(enc.as_str().unwrap().starts_with("v1:") && !fs::read_to_string(path_of(d, "kc-save")).unwrap().contains("secret"));
        assert!(has_password(d, "kc-save") && !has_password(d, "nope"));
        // The form comes back with an empty password field and the "stored" flag.
        assert_eq!(read_form(d, "kc-save").unwrap(), ConnectionForm { password: String::new(), has_password: true, ..f.clone() });
        let c = crate::Connections::load(d);
        assert!(c.warnings.is_empty(), "{:?}", c.warnings);
        let c = &c.connections["kc-save"];
        assert_eq!((c.password.as_deref(), c.prod, c.ssl.mode), (None, true, Some(SslMode::Require)));
        assert_eq!(c.resolve_password_in(keys).unwrap().password.as_deref(), Some("secret"));
        assert!(c.resolve_password_in(d).is_err());
        assert!(fs::read_dir(connections_dir(d)).unwrap().all(|e| !e.unwrap().file_name().to_string_lossy().ends_with(".tmp")));
    }

    #[test]
    fn edit_keeps_reencrypts_on_rename_and_delete_removes_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        let pw = |c: &Path, n: &str| crate::Connections::load(c).connections[n].resolve_password_in(c).map(|c| c.password);
        save(d, d, &form("kc-old"), None).unwrap();
        // Edit with an empty password field: the stored one stays.
        let mut f = read_form(d, "kc-old").unwrap();
        f.host = "other".into();
        let before = json(d, "kc-old")["password_enc"].clone();
        save(d, d, &f, Some("kc-old")).unwrap();
        assert_eq!(json(d, "kc-old")["password_enc"], before);
        assert_eq!(pw(d, "kc-old"), Ok(Some("secret".into())));
        // Rename moves the file and re-encrypts for the new name (the AAD).
        f.name = "kc-new".into();
        assert_eq!(save(d, d, &f, Some("kc-old")).unwrap(), None);
        assert_eq!(names(d), ["kc-new"]);
        assert_ne!(json(d, "kc-new")["password_enc"], before);
        assert_eq!(pw(d, "kc-new"), Ok(Some("secret".into())));
        assert_eq!(json(d, "kc-new")["host"], "other");
        // A new password replaces it, also together with a rename.
        f.password = "fresh".into();
        save(d, d, &f, Some("kc-new")).unwrap();
        assert_eq!(pw(d, "kc-new"), Ok(Some("fresh".into())));
        f.name = "kc-newer".into();
        save(d, d, &f, Some("kc-new")).unwrap();
        assert_eq!(pw(d, "kc-newer"), Ok(Some("fresh".into())));
        // A copied file under another name does not decrypt.
        fs::copy(path_of(d, "kc-newer"), path_of(d, "copy")).unwrap();
        assert!(pw(d, "copy").unwrap_err().contains("Can't decrypt"));
        // set_password encrypts into an existing file only.
        set_password(d, d, "copy", "again").unwrap();
        assert_eq!(pw(d, "copy"), Ok(Some("again".into())));
        assert!(set_password(d, d, "nope", "x").is_err());
        // Delete removes the file (a second delete of the missing file is fine).
        assert!(delete(d, "kc-newer").unwrap());
        assert!(!has_password(d, "kc-newer") && !delete(d, "kc-newer").unwrap());
    }

    #[test]
    fn unreadable_key_saves_the_rest_and_reports() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        fs::write(crypto::key_path(d), [1u8; 3]).unwrap();
        let e = save(d, d, &form("kc-off"), None).unwrap().expect("a password error");
        assert!(e.contains("32 bytes"), "{e}");
        assert_eq!(json(d, "kc-off")["host"], "h");
        assert!(json(d, "kc-off").get("password_enc").is_none());
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
        save(d, d, &f, Some("a")).unwrap();
        assert_eq!(json(d, "a"), serde_json::json!({"driver": "postgres", "host": "new", "mystery": 7, "ssl": {"ca": "ca.pem"}}));
    }

    #[test]
    fn validation() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        save(d, d, &form("a"), None).unwrap();
        assert!(save(d, d, &form("a"), None).unwrap_err().contains("already exists"));
        assert!(save(d, d, &form(" "), None).unwrap_err().contains("name"));
        assert!(save(d, d, &form("../x"), None).unwrap_err().contains("invalid"));
        assert!(save(d, d, &form("b"), Some("a")).is_ok() && save(d, d, &form("a"), None).is_ok());
        assert!(save(d, d, &form("b"), Some("a")).unwrap_err().contains("already exists"));
        let mut f = form("c");
        f.port = "abc".into();
        assert!(save(d, d, &f, None).unwrap_err().contains("port"));
        f.port = String::new();
        f.driver = None;
        assert!(save(d, d, &f, None).unwrap_err().contains("driver"));
        assert_eq!(names(d), ["a", "b"]);
        assert!(delete(d, "../a").is_err());
    }

    #[test]
    fn to_connection_resolves_and_uses_the_typed_or_stored_password() {
        let tmp = tempfile::tempdir().unwrap();
        let mut f = form("kc-test");
        f.user = "${env:PATH}".into();
        f.prod = true;
        let c = f.to_connection(tmp.path(), tmp.path(),None).unwrap();
        assert_eq!((c.user, c.password.as_deref(), c.prod, c.port), (std::env::var("PATH").ok(), Some("secret"), true, Some(5432)));
        let mut stored = form("kc-stored");
        stored.password = "kept".into();
        save(tmp.path(), tmp.path(), &stored, None).unwrap();
        f.password = String::new();
        assert_eq!(f.to_connection(tmp.path(), tmp.path(),Some("kc-stored")).unwrap().password.as_deref(), Some("kept"));
        assert_eq!(f.to_connection(tmp.path(), tmp.path(),None).unwrap().password, None);
        f.port = "x".into();
        assert!(f.to_connection(tmp.path(), tmp.path(),None).is_err());
    }
}
