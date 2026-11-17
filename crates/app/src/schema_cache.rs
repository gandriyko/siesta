//! The structure of each connection's database: in memory (shared with the editors'
//! completion providers) and on disk under `~/.siesta/cache/<connection>-<hash>.json`, so the
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
/// (driver, url, host, port, database, user), so a connection recreated
/// under the same name but pointing elsewhere does not reuse the old schema.
pub fn key(c: &db_conn::Connection) -> String {
    format!("{}-{:016x}", c.name, c.fingerprint())
}

/// `base/cache/<key>.json`, one file per connection whatever the workspace (the key already names
/// the server); made safe for a file name.
pub fn path(base: &Path, connection: &str) -> PathBuf {
    let s: String = connection.chars().map(|c| if c.is_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '_' }).collect();
    let s = if s.is_empty() || s.starts_with('.') { format!("_{s}") } else { s };
    base.join("cache").join(format!("{s}.json"))
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
            password_enc: None,
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
        assert_ne!(path(base, &a), path(base, &b));
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
        let p = path(dir.path(), "local dev/1");
        assert_eq!(p, dir.path().join("cache").join("local_dev_1.json"));
        assert!(load(&p).is_none());
        save(&p, &schema).unwrap();
        assert_eq!(load(&p), Some(schema.clone()));
        // Saving again replaces it; a damaged file reads as "no cache".
        save(&p, &Schema::default()).unwrap();
        assert_eq!(load(&p), Some(Schema::default()));
        std::fs::write(&p, "{not json").unwrap();
        assert!(load(&p).is_none());
        // A name cannot climb out of the cache folder.
        assert_eq!(path(dir.path(), "../../evil").parent(), Some(dir.path().join("cache").as_path()));
        assert_eq!(path(dir.path(), "..").file_name().unwrap(), "_...json");
    }
}
