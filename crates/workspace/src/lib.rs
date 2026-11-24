//! GUI-free workspace pieces, as in Respite: what the sidebar lists — the central folder
//! (`~/.config/siesta/collections`, with subfolders) and the `.sql` files added from elsewhere — and the UI
//! state that survives a restart (`~/.siesta/state.json`: open files, cursors, connection and
//! database picked per file).
//!
//! The added files are listed in `~/.siesta/files.json` (a JSON array of paths). Removing one only
//! drops it from that list: the file and its history stay.
//!
//! Change watching is not here: the app polls mtimes (as Respite does), which needs no extra crate.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A query file Siesta lists and runs: `.sql`, or an Elasticsearch `.es` file.
pub fn is_sql(p: &Path) -> bool {
    p.extension().is_some_and(|e| e.eq_ignore_ascii_case("sql")) || is_es(p)
}

/// An Elasticsearch requests file (Kibana console format).
pub fn is_es(p: &Path) -> bool {
    p.extension().is_some_and(|e| e.eq_ignore_ascii_case("es"))
}

/// The dialect a file is split with before its connection is known: `.es` is always Elastic,
/// `.sql` is Postgres until the connection says MySQL.
pub fn file_dialect(p: &Path) -> sql_split::Dialect {
    if is_es(p) { sql_split::Dialect::Elastic } else { sql_split::Dialect::default() }
}

/// Why a file can't run on a connection of dialect `conn`: `.es` needs Elasticsearch, `.sql` needs SQL.
pub fn mismatch(p: &Path, conn: sql_split::Dialect) -> Option<&'static str> {
    match (is_es(p), conn == sql_split::Dialect::Elastic) {
        (true, false) => Some(".es files run on Elasticsearch connections"),
        (false, true) => Some("Elasticsearch connections run .es files"),
        _ => None,
    }
}

/// Folders and `.sql` files under the central folder `root` (`~/.config/siesta/collections`),
/// sorted. Folders are listed so that an empty new one shows up. Respects `.gitignore` and skips
/// hidden entries.
pub fn scan(root: &Path) -> Vec<PathBuf> {
    // ponytail: depth cap guards against a runaway tree; raise if real collections nest deeper.
    let mut found: Vec<PathBuf> = ignore::WalkBuilder::new(root)
        .max_depth(Some(8))
        .build()
        .flatten()
        .map(|e| e.into_path())
        .filter(|p| p != root && (p.is_dir() || (p.is_file() && is_sql(p))))
        .collect();
    found.sort();
    found
}

/// Rewrites `-- @connection old` to the new name (`renames`: `(old, new)` pairs) on disk in every `.sql`
/// file of the central folder `collections` and of the added list (`base/files.json`). Returns how
/// many files were changed and the errors of those that could not be written.
pub fn rewrite_connection_refs(collections: &Path, base: Option<&Path>, renames: &[(String, String)]) -> (usize, Vec<String>) {
    let mut files: Vec<PathBuf> = scan(collections).into_iter().filter(|p| p.is_file()).collect();
    files.extend(base.map(load_files).unwrap_or_default());
    files.sort();
    files.dedup();
    let (mut changed, mut errors) = (0, Vec::new());
    for f in files.iter().filter(|f| is_sql(f)) {
        let Ok(text) = std::fs::read_to_string(f) else { continue };
        let d = file_dialect(f);
        let Some(old) = sql_split::split(&text, d).connection else { continue };
        let Some((_, new)) = renames.iter().find(|(o, _)| *o == old) else { continue };
        let Some(rewritten) = sql_split::rewrite_connection(&text, d, Some(new)) else { continue };
        match std::fs::write(f, rewritten) {
            Ok(()) => changed += 1,
            Err(e) => errors.push(format!("{}: {e}", f.display())),
        }
    }
    (changed, errors)
}

/// Added files that still exist.
pub fn load_files(base: &Path) -> Vec<PathBuf> {
    let list: Vec<PathBuf> = std::fs::read(base.join("files.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    list.into_iter().filter(|p| p.is_file()).collect()
}

/// Persist the added files; failures are reported, not fatal.
pub fn save_files(base: &Path, files: &[PathBuf]) {
    write(base, "files.json", &serde_json::to_vec_pretty(files).unwrap_or_default());
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenFile {
    pub path: PathBuf,
    /// 0-based cursor line and column.
    pub line: u32,
    pub column: u32,
}

/// What is restored on the next launch. The connection and database of a file are not here:
/// they are its own `-- @connection` / `-- @database` lines (as Respite's `# @env`).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkspaceState {
    /// Tabs, left to right.
    pub open: Vec<OpenFile>,
    /// The tab that was in front.
    pub current: Option<PathBuf>,
}

/// The saved state; missing or unreadable (or an older layout) is the empty one.
pub fn load_state(base: &Path) -> WorkspaceState {
    std::fs::read(base.join("state.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
}

pub fn save_state(base: &Path, state: &WorkspaceState) {
    write(base, "state.json", &serde_json::to_vec(state).unwrap_or_default());
}

/// Written through a temp file, so a crash never leaves half a file.
fn write(base: &Path, name: &str, bytes: &[u8]) {
    let write = std::fs::create_dir_all(base).and_then(|()| {
        let tmp = base.join(format!("{name}.tmp"));
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(tmp, base.join(name))
    });
    if let Err(e) = write {
        eprintln!("siesta: {name} not saved: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_lists_folders_and_sql() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        for d in ["api/v2", "empty", ".hidden"] {
            std::fs::create_dir_all(r.join(d)).unwrap();
        }
        for f in ["a.sql", "api/v2/b.SQL", "api/notes.txt", ".hidden/e.sql"] {
            std::fs::write(r.join(f), "").unwrap();
        }
        let rel: Vec<String> = scan(r).iter().map(|p| p.strip_prefix(r).unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(rel, ["a.sql", "api", "api/v2", "api/v2/b.SQL", "empty"]);
    }

    #[test]
    fn added_files_and_state_round_trip() {
        let (base, other) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (a, gone) = (other.path().join("a.sql"), other.path().join("gone.sql"));
        std::fs::write(&a, "SELECT 1;").unwrap();
        assert!(load_files(base.path()).is_empty());
        save_files(base.path(), &[a.clone(), gone]);
        assert_eq!(load_files(base.path()), std::slice::from_ref(&a)); // a deleted file drops out

        assert_eq!(load_state(base.path()), WorkspaceState::default());
        let st = WorkspaceState { open: vec![OpenFile { path: a.clone(), line: 3, column: 1 }], current: Some(a.clone()) };
        save_state(base.path(), &st);
        assert_eq!(load_state(base.path()), st);
        // A state.json of the old per-workspace layout reads as empty, not as an error.
        std::fs::write(base.path().join("state.json"), r#"{"workspaces": {}, "last": "/x", "recent": []}"#).unwrap();
        assert_eq!(load_state(base.path()), WorkspaceState::default());
    }

    #[test]
    fn rewrites_connection_refs_in_central_and_added_files() {
        let dir = tempfile::tempdir().unwrap();
        let (coll, base, other) = (dir.path().join("collections"), dir.path().join("base"), dir.path().join("other"));
        for d in [&coll, &base, &other] {
            std::fs::create_dir_all(d).unwrap();
        }
        let (a, b, c) = (coll.join("a.sql"), other.join("b.sql"), coll.join("c.sql"));
        std::fs::write(&a, "-- @connection My DB\nselect 1;\n").unwrap();
        std::fs::write(&b, "-- @connection My DB\nselect 2;\n").unwrap();
        std::fs::write(&c, "-- @connection fine\nselect 3;\n").unwrap();
        save_files(&base, std::slice::from_ref(&b));
        let (n, errs) = rewrite_connection_refs(&coll, Some(&base), &[("My DB".into(), "my-db".into())]);
        assert_eq!((n, errs.len()), (2, 0));
        assert!(std::fs::read_to_string(&a).unwrap().starts_with("-- @connection my-db\n"));
        assert!(std::fs::read_to_string(&b).unwrap().starts_with("-- @connection my-db\n"));
        assert!(std::fs::read_to_string(&c).unwrap().starts_with("-- @connection fine\n"));
    }
}
