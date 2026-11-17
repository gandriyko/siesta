//! GUI-free workspace pieces: the file index (`.sql` files) and the UI state that
//! survives a restart (`~/.siesta/state.json`: open files, cursors, connection picked per file).
//!
//! Change watching is not here: the app polls mtimes (as Respite does), which needs no extra crate.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub fn is_sql(p: &Path) -> bool {
    p.extension().is_some_and(|e| e.eq_ignore_ascii_case("sql"))
}

/// What the window works on: a folder (every `.sql` file under it), or chosen files (one or more,
/// from anywhere; the old "single-file workspace" is one of them).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Workspace {
    Folder(PathBuf),
    /// Sorted, without repeats, never empty.
    Files(Vec<PathBuf>),
}

impl Workspace {
    /// From what the open dialog returned: the `.sql` files among `paths` if any, else the first
    /// folder; `None` when there is neither.
    pub fn from_paths(paths: impl IntoIterator<Item = PathBuf>) -> Option<Self> {
        let (mut files, mut dirs) = (vec![], vec![]);
        for p in paths {
            if p.is_dir() {
                dirs.push(p);
            } else if is_sql(&p) {
                files.push(p);
            }
        }
        if files.is_empty() {
            return dirs.into_iter().next().map(Workspace::Folder);
        }
        files.sort();
        files.dedup();
        Some(Workspace::Files(files))
    }

    /// The key in `state.json` and in history entries: the folder's path, the file's path, or the
    /// files' paths one per line (a path holds no newline in practice).
    pub fn key(&self) -> String {
        match self {
            Workspace::Folder(p) => p.to_string_lossy().into_owned(),
            Workspace::Files(f) => f.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>().join("\n"),
        }
    }

    pub fn from_key(key: &str) -> Self {
        let p = PathBuf::from(key);
        if !key.contains('\n') && p.is_dir() { Workspace::Folder(p) } else { Workspace::Files(key.split('\n').map(PathBuf::from).collect()) }
    }

    /// Still there: the folder, or at least one of the files.
    pub fn exists(&self) -> bool {
        match self {
            Workspace::Folder(p) => p.is_dir(),
            Workspace::Files(f) => f.iter().any(|p| p.is_file()),
        }
    }

    /// The folder relative paths are counted from: the workspace folder, or the deepest folder
    /// holding every chosen file.
    pub fn folder(&self) -> PathBuf {
        match self {
            Workspace::Folder(p) => p.clone(),
            Workspace::Files(f) => {
                let parent = |p: &PathBuf| p.parent().unwrap_or(p).to_path_buf();
                let first = parent(&f[0]);
                f[1..].iter().fold(first, |acc, p| acc.ancestors().find(|a| p.starts_with(a)).unwrap_or(Path::new("/")).to_path_buf())
            }
        }
    }

    /// For the sidebar header: the folder's name, the file's name, or `first.sql +N`.
    pub fn name(&self) -> String {
        let name = |p: &Path| p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned());
        match self {
            Workspace::Folder(p) => name(p),
            Workspace::Files(f) if f.len() == 1 => name(&f[0]),
            Workspace::Files(f) => format!("{} +{}", name(&f[0]), f.len() - 1),
        }
    }

    /// The `.sql` files shown: everything under the folder, or the chosen files that still exist.
    pub fn files(&self) -> Vec<PathBuf> {
        match self {
            Workspace::Folder(p) => scan(p),
            Workspace::Files(f) => f.iter().filter(|p| p.is_file() && is_sql(p)).cloned().collect(),
        }
    }
}

/// `.sql` files under `root` (nothing else is shown: configuration lives outside the project),
/// respecting `.gitignore`.
fn scan(root: &Path) -> Vec<PathBuf> {
    // ponytail: depth cap guards against a root like $HOME; raise if real projects nest deeper.
    let mut files: Vec<PathBuf> = ignore::WalkBuilder::new(root)
        .max_depth(Some(8))
        .build()
        .flatten()
        .map(|e| e.into_path())
        .filter(|p| p.is_file() && is_sql(p))
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
    /// Database picked per file (absent = the connection's default). Dropped when the file's
    /// connection changes.
    pub databases: BTreeMap<PathBuf, String>,
}

fn read(base: &Path) -> serde_json::Value {
    std::fs::read(base.join("state.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_else(|| serde_json::json!({}))
}

pub fn load_state(base: &Path, ws: &Workspace) -> WorkspaceState {
    serde_json::from_value(read(base)["workspaces"][ws.key().as_str()].clone()).unwrap_or_default()
}

/// Written through a temp file, so a crash never leaves a half-written `state.json`.
pub fn save_state(base: &Path, ws: &Workspace, state: &WorkspaceState) {
    let mut all = read(base);
    if !all["workspaces"].is_object() {
        all["workspaces"] = serde_json::json!({});
    }
    let key = ws.key();
    all["workspaces"][key.as_str()] = serde_json::to_value(state).unwrap_or_default();
    all["last"] = key.clone().into();
    // Newest first, without repeats.
    let mut recent: Vec<String> = all["recent"].as_array().into_iter().flatten().filter_map(|v| v.as_str()).filter(|r| *r != key).map(str::to_string).collect();
    recent.insert(0, key);
    recent.truncate(RECENT);
    // Only the workspaces the list still offers keep their state.
    if let Some(ws) = all["workspaces"].as_object_mut() {
        ws.retain(|k, _| recent.contains(k));
    }
    all["recent"] = recent.into();
    let write = std::fs::create_dir_all(base).and_then(|()| {
        let tmp = base.join("state.json.tmp");
        std::fs::write(&tmp, all.to_string())?;
        std::fs::rename(tmp, base.join("state.json"))
    });
    if let Err(e) = write {
        eprintln!("siesta: state not saved: {e}");
    }
}

/// How many workspaces `recent` keeps.
const RECENT: usize = 10;

/// Workspaces opened before, newest first, that still exist.
pub fn recent(base: &Path) -> Vec<Workspace> {
    read(base)["recent"].as_array().into_iter().flatten().filter_map(|v| v.as_str()).map(Workspace::from_key).filter(Workspace::exists).collect()
}

/// The workspace used last, if it still exists: a plain launch reopens it.
pub fn last_workspace(base: &Path) -> Option<Workspace> {
    Some(Workspace::from_key(read(base)["last"].as_str()?)).filter(Workspace::exists)
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
        let rel: Vec<String> = scan(r).iter().map(|p| p.strip_prefix(r).unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(rel, ["a.sql", "api/v2/b.SQL"]);
        assert_eq!(Workspace::Folder(r.to_path_buf()).files(), scan(r));
    }

    #[test]
    fn workspace_from_dialog_key_folder_and_name() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        std::fs::create_dir_all(r.join("api/v2")).unwrap();
        for f in ["a.sql", "api/v2/b.SQL", "api/notes.txt"] {
            std::fs::write(r.join(f), "").unwrap();
        }
        let (a, b) = (r.join("a.sql"), r.join("api/v2/b.SQL"));
        // Files win over folders; non-.sql files are skipped; order and repeats don't matter.
        let ws = Workspace::from_paths([b.clone(), r.join("api"), r.join("api/notes.txt"), a.clone(), b.clone()]).unwrap();
        assert_eq!(ws, Workspace::Files(vec![a.clone(), b.clone()]));
        assert_eq!((ws.folder(), ws.name(), ws.files()), (r.to_path_buf(), "a.sql +1".to_string(), vec![a.clone(), b.clone()]));
        assert_eq!(Workspace::from_key(&ws.key()), ws);
        // Only folders: the first one. Nothing usable: None.
        assert_eq!(Workspace::from_paths([r.join("api"), r.to_path_buf()]), Some(Workspace::Folder(r.join("api"))));
        assert_eq!(Workspace::from_paths([r.join("api/notes.txt")]), None);
        // One file: keyed by its own path (as single-file workspaces always were), counted from its folder.
        let one = Workspace::from_paths([b.clone()]).unwrap();
        assert_eq!((one.key(), one.folder(), one.name()), (b.to_string_lossy().into_owned(), r.join("api/v2"), "b.SQL".to_string()));
        assert_eq!(Workspace::from_key(&r.to_string_lossy()), Workspace::Folder(r.to_path_buf()));
        // A file that is gone drops out; the workspace is gone with the last one.
        std::fs::remove_file(&b).unwrap();
        assert_eq!((ws.files(), ws.exists(), one.exists()), (vec![a], true, false));
    }

    #[test]
    fn state_round_trip() {
        let (base, dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let root = &Workspace::Folder(dir.path().to_path_buf());
        assert_eq!(load_state(base.path(), root), WorkspaceState::default());
        assert_eq!(last_workspace(base.path()), None);
        let d = dir.path();
        let st = WorkspaceState {
            open: vec![OpenFile { path: d.join("a.sql"), line: 3, column: 1 }],
            current: Some(d.join("a.sql")),
            connections: BTreeMap::from([(d.join("a.sql"), Some("dev".into())), (d.join("b.sql"), None)]),
            databases: BTreeMap::from([(d.join("a.sql"), "app".to_string())]),
        };
        save_state(base.path(), root, &st);
        assert_eq!(load_state(base.path(), root), st);
        assert_eq!(last_workspace(base.path()).as_ref(), Some(root));
        // Another workspace (two chosen files) does not touch this one.
        let other = tempfile::tempdir().unwrap();
        let (x, y) = (other.path().join("x.sql"), other.path().join("y.sql"));
        std::fs::write(&x, "").unwrap();
        std::fs::write(&y, "").unwrap();
        let files = &Workspace::Files(vec![x, y]);
        save_state(base.path(), files, &WorkspaceState::default());
        assert_eq!(load_state(base.path(), root), st);
        assert_eq!(last_workspace(base.path()).as_ref(), Some(files));
        // Recent: newest first, no repeats, gone ones skipped.
        save_state(base.path(), root, &st);
        assert_eq!(recent(base.path()), [root.clone(), files.clone()]);
        drop(other);
        assert_eq!(recent(base.path()), std::slice::from_ref(root));
    }

    #[test]
    fn state_keeps_only_recent_workspaces() {
        let base = tempfile::tempdir().unwrap();
        let roots: Vec<Workspace> = (0..=RECENT).map(|i| Workspace::Folder(PathBuf::from(format!("/w/{i}")))).collect();
        for r in &roots {
            save_state(base.path(), r, &WorkspaceState { current: Some(r.folder().join("a.sql")), ..Default::default() });
        }
        let all = read(base.path());
        assert_eq!(all["workspaces"].as_object().unwrap().len(), RECENT);
        assert_eq!(load_state(base.path(), &roots[0]), WorkspaceState::default()); // the oldest is gone
        assert_eq!(load_state(base.path(), &roots[RECENT]).current, Some(roots[RECENT].folder().join("a.sql")));
    }
}
