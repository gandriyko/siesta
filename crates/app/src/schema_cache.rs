//! The structure of each connection's database: in memory (shared with the editors'
//! completion providers) and on disk under `~/.siesta/cache/<project>/<connection>.json`, so the
//! DB tab and autocomplete work right after a start, before any session is open.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
};

use db_exec::Schema;

/// Per connection [`key`].
#[derive(Default)]
pub struct Cache {
    pub schemas: HashMap<String, Arc<Schema>>,
    pub loading: HashSet<String>,
    pub errors: HashMap<String, String>,
    /// Read from the server in this run (picking a connection does it once; DDL and "Refresh" again).
    pub fresh: HashSet<String>,
    /// Keys whose disk cache was already looked at.
    pub disk_tried: HashSet<String>,
}

pub type Shared = Rc<RefCell<Cache>>;

/// The cache key of a connection, in memory and on disk: its name plus a hash of what it points at
/// (driver, url, host, port, database, user), so the same name in two `siesta.json` files of a
/// monorepo keeps two schemas.
pub fn key(c: &db_conn::Connection) -> String {
    format!("{}-{:016x}", c.name, c.fingerprint())
}

/// `base/cache/<project>/<key>.json`; names are made safe for a file name.
pub fn path(base: &Path, root: &Path, connection: &str) -> PathBuf {
    let safe = |s: &str| {
        let s: String = s.chars().map(|c| if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' }).collect();
        if s.is_empty() || s.starts_with('.') { format!("_{s}") } else { s }
    };
    let project = root.file_name().map_or_else(|| "workspace".to_string(), |n| n.to_string_lossy().into_owned());
    base.join("cache").join(safe(&project)).join(format!("{}.json", safe(connection)))
}

pub fn load(path: &Path) -> Option<Schema> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// Written next to the final name and renamed, so a crash never leaves half a file.
pub fn save(path: &Path, schema: &Schema) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(schema).map_err(std::io::Error::other)?)?;
    std::fs::rename(tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use db_exec::{ColumnInfo, Index, SchemaNode, Table, TableKind};

    #[test]
    fn same_name_different_server_is_a_different_key() {
        let conn = |host: &str| db_conn::Connection {
            name: "dev".into(),
            driver: db_conn::Driver::Postgres,
            url: None,
            host: Some(host.into()),
            port: None,
            database: Some("app".into()),
            user: None,
            password: None,
            ssl: Default::default(),
            read_only: false,
            prod: false,
            schema: None,
            init_sql: None,
            color: None,
        };
        let (a, b) = (key(&conn("h1")), key(&conn("h2")));
        assert_ne!(a, b);
        assert_eq!(a, key(&conn("h1")));
        assert!(a.starts_with("dev-"));
        // Two files on disk, not one.
        let base = Path::new("/b");
        assert_ne!(path(base, Path::new("/w/p"), &a), path(base, Path::new("/w/p"), &b));
    }

    #[test]
    fn round_trip_on_disk() {
        let schema = Schema {
            schemas: vec![SchemaNode {
                name: "public".into(),
                tables: vec![Table {
                    name: "users".into(),
                    kind: TableKind::View,
                    columns: vec![ColumnInfo { name: "id".into(), data_type: "integer".into(), nullable: false, primary_key: true, ordinal: 1 }],
                    indexes: vec![Index { name: "users_pkey".into(), columns: vec!["id".into()], unique: true, primary: true }],
                }],
            }],
        };
        let dir = tempfile::tempdir().unwrap();
        let p = path(dir.path(), Path::new("/work/my project"), "local dev/1");
        assert_eq!(p, dir.path().join("cache").join("my_project").join("local_dev_1.json"));
        assert!(load(&p).is_none());
        save(&p, &schema).unwrap();
        assert_eq!(load(&p), Some(schema.clone()));
        // Saving again replaces it; a damaged file reads as "no cache".
        save(&p, &Schema::default()).unwrap();
        assert_eq!(load(&p), Some(Schema::default()));
        std::fs::write(&p, "{not json").unwrap();
        assert!(load(&p).is_none());
        // A name cannot climb out of the cache folder.
        assert!(path(dir.path(), Path::new("/w/x"), "../../evil").starts_with(dir.path().join("cache")));
        assert_eq!(path(dir.path(), Path::new("/w/x"), "..").file_name().unwrap(), "_...json");
    }
}
