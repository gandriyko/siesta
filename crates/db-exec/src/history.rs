//! Execution history: one folder `<base>/history/` (base is `~/.siesta`), one `<id>.json` per
//! executed statement — its file, text, connection, database, time, duration, row count or error.
//! Result data is never stored (it can be large and sensitive).

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    pub id: String,
    /// RFC 3339, UTC.
    pub time: String,
    /// `gui` / `cli` / `mcp` (a run by an agent through `siesta mcp`).
    pub source: String,
    /// The file it ran in, absolute; empty for SQL from `siesta exec` / MCP `sql`.
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
    dir: PathBuf,
    /// Entries kept (`history.total`); older ones are deleted on record.
    pub total: usize,
}

impl Store {
    /// `base` is `~/.siesta`.
    pub fn open(base: &Path) -> io::Result<Self> {
        let dir = base.join("history");
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir, total: 500 })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Saves `entry` (its `id`, `time` and `file` are filled in); returns the id.
    pub fn record(&self, file: &Path, mut entry: Entry) -> io::Result<String> {
        let now = chrono::Utc::now();
        // Nanoseconds keep ids (= file names) in execution order when sorted.
        let mut id = now.format("%Y-%m-%dT%H-%M-%S%.9fZ").to_string();
        while self.dir.join(format!("{id}.json")).exists() {
            id.push('_');
        }
        entry.id = id.clone();
        entry.time = now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
        entry.file = file.to_string_lossy().into_owned();
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
        self.ids().iter().filter_map(|id| serde_json::from_slice::<Entry>(&std::fs::read(self.dir.join(format!("{id}.json"))).ok()?).ok()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_history_record_list_prune() {
        let base = tempfile::tempdir().unwrap();
        let mut s = Store::open(base.path()).unwrap();
        assert_eq!(s.dir(), base.path().join("history"));
        s.total = 3;
        let (file, other) = (Path::new("/w1/q/reports.sql"), Path::new("/w2/x.sql"));
        for (sql, rows) in [("SELECT 1", 1), ("SELECT 2", 2)] {
            s.record(file, Entry { connection: "dev".into(), sql: sql.into(), rows: Some(rows), ..Default::default() }).unwrap();
        }
        s.record(other, Entry { connection: "dev".into(), sql: "SELECT b".into(), ..Default::default() }).unwrap();
        s.record(file, Entry { connection: "dev".into(), sql: "DELETE".into(), error: Some("42P01 no table\ndetail".into()), ..Default::default() }).unwrap();
        // Newest first, the oldest one pruned; files are absolute.
        let list = s.list();
        assert_eq!(list.iter().map(|e| (e.sql.as_str(), e.file.as_str())).collect::<Vec<_>>(), [("DELETE", "/w1/q/reports.sql"), ("SELECT b", "/w2/x.sql"), ("SELECT 2", "/w1/q/reports.sql")]);
        assert_eq!((list[0].outcome(), list[2].outcome()), ("42P01 no table".into(), "2 rows".into()));
        assert!(list[0].time.ends_with('Z') && !list[0].time_label().is_empty());
    }
}
