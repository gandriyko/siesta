//! One-time migration of the old `connections.json` / `connections.private.json` (one JSON map of
//! all connections, passwords in the private file or behind `${keychain}` markers) into
//! per-connection files; literal passwords become `password_enc`, keychain markers are dropped with
//! a warning. The old files are renamed to `*.migrated`.

use std::path::Path;

use serde_json::{Map, Value};

use crate::global::write_atomic;
use crate::{connections_dir, crypto, subst, valid_name};

const PUBLIC: &str = "connections.json";
const PRIVATE: &str = "connections.private.json";

fn read(path: &Path) -> Result<Map<String, Value>, String> {
    match std::fs::read_to_string(path) {
        Ok(t) if t.trim().is_empty() => Ok(Map::new()),
        Ok(t) => serde_json::from_str(&t).map_err(|e| format!("{}: invalid JSON: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// `${keychain}` or `${keychain:ACCOUNT}`.
fn keychain_marker(s: &str) -> bool {
    s.trim().strip_prefix("${").and_then(|s| s.strip_suffix('}')).is_some_and(|i| i.trim() == "keychain" || i.trim().starts_with("keychain:"))
}

/// Splits the old files in `config` into `connections/<name>.json` (an existing file is kept),
/// encrypts passwords into `password_enc` and renames the old files to `*.migrated`. Does nothing when
/// there is no `connections.json`. Returns what went wrong, for the warnings.
pub fn migrate(config: &Path) -> Vec<String> {
    if !config.join(PUBLIC).is_file() {
        return vec![];
    }
    let (public, private) = match (read(&config.join(PUBLIC)), read(&config.join(PRIVATE))) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), _) | (_, Err(e)) => return vec![format!("{e}; connections not migrated")],
    };
    let dir = connections_dir(config);
    let mut warnings = vec![];
    for (name, v) in public {
        let Value::Object(mut e) = v else {
            warnings.push(format!("'{name}' is not an object, not migrated"));
            continue;
        };
        if let Err(msg) = valid_name(&name) {
            warnings.push(format!("{msg}, not migrated"));
            continue;
        }
        // The private entry wins, field by field (`ssl` deeply).
        for (k, v) in private.get(&name).and_then(Value::as_object).into_iter().flatten() {
            match (e.get_mut(k), v) {
                (Some(Value::Object(old)), Value::Object(new)) if k == "ssl" => old.extend(new.clone()),
                _ => {
                    e.insert(k.clone(), v.clone());
                }
            }
        }
        // Relative ssl paths were relative to the config dir; the files now live one level deeper.
        if let Some(Value::Object(ssl)) = e.get_mut("ssl") {
            for k in ["ca", "cert", "key"] {
                if let Some(Value::String(p)) = ssl.get_mut(k)
                    && !p.starts_with(['~', '/']) && !p.contains("${")
                {
                    *p = config.join(&*p).to_string_lossy().into_owned();
                }
            }
        }
        let password = e.remove("password");
        let path = dir.join(format!("{name}.json"));
        if path.exists() {
            warnings.push(format!("{}: already exists, '{name}' from {PUBLIC} not migrated", path.display()));
            continue;
        }
        if let Some(Value::String(p)) = password {
            match encrypt_password(config, &name, &p) {
                Ok(Some(enc)) => {
                    e.insert("password_enc".into(), enc.into());
                }
                Ok(None) => {}
                Err(msg) => warnings.push(format!("password of '{name}' not migrated ({msg}); set it with `siesta connections set-password {name}`")),
            }
        }
        if let Err(msg) = write_atomic(&path, &e) {
            warnings.push(msg);
        }
    }
    for f in [PUBLIC, PRIVATE] {
        let from = config.join(f);
        if from.is_file()
            && let Err(e) = std::fs::rename(&from, config.join(format!("{f}.migrated")))
        {
            warnings.push(format!("{}: {e}", from.display()));
        }
    }
    warnings
}

/// The old password as `password_enc`; `Ok(None)` for an empty one. `${keychain…}` markers are
/// dropped (the keychain is not used any more).
fn encrypt_password(config: &Path, name: &str, old: &str) -> Result<Option<String>, String> {
    if keychain_marker(old) {
        return Err("it was in the system keychain, which Siesta no longer reads; re-enter the password".into());
    }
    if old.is_empty() {
        return Ok(None);
    }
    crypto::encrypt(config, name, &subst(old)?).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn splits_into_files_and_encrypts_passwords() {
        let tmp = tempfile::tempdir().unwrap();
        let c = tmp.path();
        fs::write(
            c.join(PUBLIC),
            r#"{
  "kc-mig-lit": {"driver": "postgres", "host": "h", "ssl": {"mode": "require", "ca": "certs/ca.pem"}},
  "kc-mig-own": {"driver": "mysql", "password": "${keychain}"},
  "kc-mig-team": {"driver": "mysql", "password": "${keychain:kc-mig-shared}"},
  "kc-mig-env": {"driver": "mysql", "password": "${env:PATH}"},
  "kc-mig-unset": {"driver": "mysql", "password": "${env:SIESTA_TEST_SURELY_UNSET}"},
  "bad/name": {"driver": "mysql"},
  "kc-mig-exists": {"driver": "mysql", "host": "old"}
}"#,
        )
        .unwrap();
        fs::write(c.join(PRIVATE), r#"{"kc-mig-lit": {"host": "private-host", "password": "hunter2", "ssl": {"mode": "verify-full"}}}"#).unwrap();
        fs::create_dir_all(connections_dir(c)).unwrap();
        fs::write(connections_dir(c).join("kc-mig-exists.json"), r#"{"driver": "mysql", "host": "new"}"#).unwrap();

        let w = migrate(c);
        let w = w.join("\n");
        assert!(w.contains("bad/name") && w.contains("kc-mig-exists") && w.contains("kc-mig-unset") && w.contains("password of 'kc-mig-own'") && w.contains("re-enter"), "{w}");
        assert!(!w.contains("hunter2"), "{w}");

        let file = |n: &str| -> Value { serde_json::from_str(&fs::read_to_string(connections_dir(c).join(format!("{n}.json"))).unwrap()).unwrap() };
        let mut lit = file("kc-mig-lit");
        assert!(lit.as_object_mut().unwrap().remove("password_enc").is_some());
        assert_eq!(lit, serde_json::json!({"driver": "postgres", "host": "private-host", "ssl": {"mode": "verify-full", "ca": c.join("certs/ca.pem").to_str().unwrap()}}));
        assert_eq!(file("kc-mig-team"), serde_json::json!({"driver": "mysql"}));
        assert_eq!(file("kc-mig-exists")["host"], "new");
        assert_eq!(crate::names(c), ["kc-mig-env", "kc-mig-exists", "kc-mig-lit", "kc-mig-own", "kc-mig-team", "kc-mig-unset"]);
        // Literal and `${env:}` passwords are encrypted; keychain markers and unresolved ones are dropped.
        let pw = |n: &str| crate::Connections::load(c).connections[n].resolve_password_in(c).unwrap().password;
        assert_eq!(pw("kc-mig-lit").as_deref(), Some("hunter2"));
        assert_eq!(pw("kc-mig-env"), std::env::var("PATH").ok());
        assert_eq!((pw("kc-mig-own"), pw("kc-mig-team"), pw("kc-mig-unset")), (None, None, None));
        assert!(!fs::read_to_string(connections_dir(c).join("kc-mig-lit.json")).unwrap().contains("hunter2"));
        // The old files are renamed, so a second run does nothing.
        assert!(!c.join(PUBLIC).exists() && !c.join(PRIVATE).exists());
        assert!(c.join("connections.json.migrated").is_file() && c.join("connections.private.json.migrated").is_file());
        assert!(migrate(c).is_empty());
    }

    #[test]
    fn broken_old_file_is_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(PUBLIC), "{ nope").unwrap();
        let w = migrate(tmp.path());
        assert!(w.len() == 1 && w[0].contains("invalid JSON"), "{w:?}");
        assert!(tmp.path().join(PUBLIC).is_file() && !connections_dir(tmp.path()).exists());
        assert!(migrate(&tmp.path().join("none")).is_empty());
    }

    #[test]
    fn load_migrates_first() {
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join(PUBLIC), r#"{"mig-load": {"driver": "postgres", "host": "h"}}"#).unwrap();
        let c = crate::Connections::load(tmp.path());
        assert_eq!(c.names(), ["mig-load"]);
        assert!(c.warnings.is_empty(), "{:?}", c.warnings);
    }
}
