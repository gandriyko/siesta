//! GUI-free workspace pieces: the file index (`.sql` files) and the UI state that
//! survives a restart (`~/.siesta/state.json`: open files, cursors, connection picked per file).
//!
//! Change watching is not here: the app polls mtimes (as Respite does), which needs no extra crate.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// `~/.siesta`
pub fn base_dir() -> Option<PathBuf> {
    Some(std::env::home_dir()?.join(".siesta"))
}

pub fn is_sql(p: &Path) -> bool {
    p.extension().is_some_and(|e| e.eq_ignore_ascii_case("sql"))
}

/// `.sql` files under `root` (every file with `all`), respecting `.gitignore`.
pub fn scan(root: &Path, all: bool) -> Vec<PathBuf> {
    // ponytail: depth cap guards against a root like $HOME; raise if real projects nest deeper.
    let mut files: Vec<PathBuf> = ignore::WalkBuilder::new(root)
        .max_depth(Some(8))
        .build()
        .flatten()
        .map(|e| e.into_path())
        .filter(|p| p.is_file() && (all || is_sql(p)))
        .collect();
    files.sort();
    files
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct OpenFile {
    pub path: PathBuf,
    /// 0-based cursor line and column.
    pub line: u32,
    pub column: u32,
}

/// What is restored when a workspace is opened again.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkspaceState {
    /// Tabs, left to right.
    pub open: Vec<OpenFile>,
    /// The tab that was in front.
    pub current: Option<PathBuf>,
    /// Connection picked per file; `null` = "No connection" chosen explicitly.
    pub connections: BTreeMap<PathBuf, Option<String>>,
}

fn read(base: &Path) -> serde_json::Value {
    std::fs::read(base.join("state.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_else(|| serde_json::json!({}))
}

pub fn load_state(base: &Path, root: &Path) -> WorkspaceState {
    serde_json::from_value(read(base)["workspaces"][root.to_string_lossy().as_ref()].clone()).unwrap_or_default()
}

/// Written through a temp file, so a crash never leaves a half-written `state.json`.
pub fn save_state(base: &Path, root: &Path, state: &WorkspaceState) {
    let mut all = read(base);
    if !all["workspaces"].is_object() {
        all["workspaces"] = serde_json::json!({});
    }
    all["workspaces"][root.to_string_lossy().as_ref()] = serde_json::to_value(state).unwrap_or_default();
    all["last"] = root.to_string_lossy().into();
    let write = std::fs::create_dir_all(base).and_then(|()| {
        let tmp = base.join("state.json.tmp");
        std::fs::write(&tmp, all.to_string())?;
        std::fs::rename(tmp, base.join("state.json"))
    });
    if let Err(e) = write {
        eprintln!("siesta: state not saved: {e}");
    }
}

/// The workspace used last, if it still exists: a plain launch reopens it.
pub fn last_root(base: &Path) -> Option<PathBuf> {
    let root = PathBuf::from(read(base)["last"].as_str()?);
    root.is_dir().then_some(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_filters() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        for d in ["api/v2", "ignored", ".hidden"] {
            std::fs::create_dir_all(r.join(d)).unwrap();
        }
        for f in ["a.sql", "api/v2/b.SQL", "api/notes.txt", "ignored/c.sql", ".hidden/e.sql", "package.json"] {
            std::fs::write(r.join(f), "").unwrap();
        }
        std::fs::write(r.join(".gitignore"), "ignored/\n").unwrap();
        std::fs::create_dir(r.join(".git")).unwrap(); // .gitignore applies inside a git repo
        let rel = |all| -> Vec<String> { scan(r, all).iter().map(|p| p.strip_prefix(r).unwrap().to_string_lossy().into_owned()).collect() };
        assert_eq!(rel(false), ["a.sql", "api/v2/b.SQL"]);
        assert!(rel(true).contains(&"api/notes.txt".to_string()) && rel(true).contains(&"package.json".to_string()));
    }

    #[test]
    fn state_round_trip() {
        let (base, root) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        assert_eq!(load_state(base.path(), root.path()), WorkspaceState::default());
        assert_eq!(last_root(base.path()), None);
        let st = WorkspaceState {
            open: vec![OpenFile { path: root.path().join("a.sql"), line: 3, column: 1 }],
            current: Some(root.path().join("a.sql")),
            connections: BTreeMap::from([(root.path().join("a.sql"), Some("dev".into())), (root.path().join("b.sql"), None)]),
        };
        save_state(base.path(), root.path(), &st);
        assert_eq!(load_state(base.path(), root.path()), st);
        assert_eq!(last_root(base.path()).as_deref(), Some(root.path()));
        // Another workspace does not touch this one.
        let other = tempfile::tempdir().unwrap();
        save_state(base.path(), other.path(), &WorkspaceState::default());
        assert_eq!(load_state(base.path(), root.path()), st);
        assert_eq!(last_root(base.path()).as_deref(), Some(other.path()));
    }
}
