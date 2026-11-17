//! Execution history, per project, in `<base>/history/<project>/` (base is `~/.siesta`): one
//! `<id>.json` per executed statement — its text, connection, database, time, duration, row count
//! or error. Result data is never stored (it can be large and sensitive).

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    pub id: String,
    /// RFC 3339, UTC.
    pub time: String,
    /// `gui` / `cli`.
    pub source: String,
    /// Relative to the workspace root.
    pub file: String,
    /// 1-based line of the statement when it ran.
    pub line: usize,
    pub connection: String,
    /// The database it ran on (`None`: the connection's default).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub database: Option<String>,
    pub sql: String,
    pub duration_ms: u64,
    /// Rows returned (the first page for paged queries).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<u64>,
    /// Rows changed by DML.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub affected: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Entry {
    /// `rows` / `affected` / the error, as one short text.
    pub fn outcome(&self) -> String {
        match (&self.error, self.affected, self.rows) {
            (Some(e), ..) => e.lines().next().unwrap_or("").to_string(),
            (None, Some(n), _) => format!("{n} affected"),
            (None, None, Some(n)) => format!("{n} row{}", if n == 1 { "" } else { "s" }),
            _ => "OK".into(),
        }
    }

    /// `14:03:22` today, `Nov 11 18:40` otherwise (local time).
    pub fn time_label(&self) -> String {
        use chrono::{DateTime, Local};
        let Ok(t) = DateTime::parse_from_rfc3339(&self.time) else { return self.time.clone() };
        let at = t.with_timezone(&Local);
        if at.date_naive() == Local::now().date_naive() { at.format("%H:%M:%S").to_string() } else { at.format("%b %d %H:%M").to_string() }
    }
}

pub struct Store {
    root: PathBuf,
    dir: PathBuf,
    /// Entries kept (`history.total`); older ones are deleted on record.
    pub total: usize,
}

impl Store {
    /// `base` is `~/.siesta`; `root` is the workspace root (its folder name names the project), or
    /// the file of a single-file workspace (its file name does).
    pub fn open(base: &Path, root: &Path) -> io::Result<Self> {
        let dir = base.join("history").join(project_name(base, root)?);
        std::fs::create_dir_all(&dir)?;
        Ok(Self { root: root.to_path_buf(), dir, total: 500 })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Saves `entry` (its `id`, `time` and relative `file` are filled in from `file`); returns the id.
    pub fn record(&self, file: &Path, mut entry: Entry) -> io::Result<String> {
        let now = chrono::Utc::now();
        // Nanoseconds keep ids (= file names) in execution order when sorted.
        let mut id = now.format("%Y-%m-%dT%H-%M-%S%.9fZ").to_string();
        while self.dir.join(format!("{id}.json")).exists() {
            id.push('_');
        }
        entry.id = id.clone();
        entry.time = now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        // A single-file workspace (`root` is the file) counts from the file's folder.
        let folder = if self.root.is_file() { self.root.parent().unwrap_or(&self.root) } else { &self.root };
        entry.file = file.strip_prefix(folder).unwrap_or(file).to_string_lossy().into_owned();
        std::fs::write(self.dir.join(format!("{id}.json")), serde_json::to_vec_pretty(&entry).map_err(io::Error::other)?)?;
        // Prune by file name: no entry needs reading.
        for old in self.ids().into_iter().skip(self.total.max(1)) {
            std::fs::remove_file(self.dir.join(format!("{old}.json")))?;
        }
        Ok(id)
    }

    /// Ids, newest first.
    fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = std::fs::read_dir(&self.dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter_map(|e| e.file_name().to_str()?.strip_suffix(".json").map(str::to_string))
            .collect();
        ids.sort_unstable_by(|a, b| b.cmp(a));
        ids
    }

    /// Newest first.
    // ponytail: reads every entry (≤ `total` small files) when the window opens; add an index file if it shows up.
    pub fn list(&self) -> Vec<Entry> {
        self.ids().iter().filter_map(|id| serde_json::from_slice(&std::fs::read(self.dir.join(format!("{id}.json"))).ok()?).ok()).collect()
    }
}

fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}

/// Root folder name, or `name-<4 hex>` if another root already took it; kept in `projects.json`.
fn project_name(base: &Path, root: &Path) -> io::Result<String> {
    std::fs::create_dir_all(base)?;
    let map_path = base.join("projects.json");
    let mut map: HashMap<String, String> = std::fs::read(&map_path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let key = root.to_string_lossy().into_owned();
    if let Some(name) = map.get(&key) {
        return Ok(name.clone());
    }
    let plain = sanitize(&root.file_name().map_or("root".into(), |n| n.to_string_lossy().into_owned()));
    let name = if map.values().any(|v| *v == plain) {
        let mut h = DefaultHasher::new();
        key.hash(&mut h);
        format!("{plain}-{:04x}", h.finish() & 0xffff)
    } else {
        plain
    };
    map.insert(key, name.clone());
    std::fs::write(&map_path, serde_json::to_vec_pretty(&map).map_err(io::Error::other)?)?;
    Ok(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projects_record_list_prune() {
        let base = std::env::temp_dir().join(format!("siesta-history-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (a, b) = (Path::new("/w1/app"), Path::new("/w2/app"));
        let sa = Store::open(&base, a).unwrap();
        let sb = Store::open(&base, b).unwrap();
        assert!(sa.dir().ends_with("history/app"));
        assert!(sb.dir().file_name().unwrap().to_string_lossy().starts_with("app-"));
        assert_eq!(Store::open(&base, b).unwrap().dir(), sb.dir());

        let mut s = Store::open(&base, a).unwrap();
        s.total = 2;
        let file = a.join("q/reports.sql");
        for (sql, rows) in [("SELECT 1", 1), ("SELECT 2", 2), ("SELECT 3", 3)] {
            s.record(&file, Entry { connection: "dev".into(), sql: sql.into(), rows: Some(rows), ..Default::default() }).unwrap();
        }
        s.record(&file, Entry { connection: "dev".into(), sql: "DELETE".into(), error: Some("42P01 no table\ndetail".into()), ..Default::default() }).unwrap();
        let list = s.list();
        assert_eq!(list.iter().map(|e| e.sql.as_str()).collect::<Vec<_>>(), ["DELETE", "SELECT 3"]);
        assert_eq!((list[0].file.as_str(), list[0].outcome(), list[1].outcome()), ("q/reports.sql", "42P01 no table".into(), "3 rows".into()));
        assert!(list[0].time.ends_with('Z') && !list[0].time_label().is_empty());
        std::fs::remove_dir_all(&base).unwrap();
    }
}
