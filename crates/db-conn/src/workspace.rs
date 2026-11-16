//! A personal password for a connection a team `siesta.json` defines: it goes to the system keychain,
//! and `siesta.private.json` next to that `siesta.json` gets a `"${keychain:ACCOUNT}"` reference.
//! The team's file is never written. GUI-free; the form in `conn_form.rs` calls it.

use std::path::{Path, PathBuf};

use ignore::Match;
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use serde_json::{Map, Value};

use crate::global::{read_map, write_atomic};
use crate::marker_text;
use crate::secrets::{self, SecretError};

const PRIVATE: &str = "siesta.private.json";

#[derive(Debug, Default, PartialEq, Eq)]
pub struct WorkspacePassword {
    pub account: String,
    pub private_file: PathBuf,
    /// Set when the keychain was unavailable and the password went into the file as is.
    pub warning: Option<String>,
    /// `Some(.gitignore)` when the private file is in a git repo and not ignored: offer to append
    /// `siesta.private.json` there with [`add_to_gitignore`].
    pub gitignore: Option<PathBuf>,
}

/// The `siesta.json` that "owns" the connection: the highest-priority one among its `sources`.
pub fn workspace_def_file(sources: &[PathBuf]) -> Option<&Path> {
    sources.iter().rev().find(|p| p.file_name().is_some_and(|n| n == "siesta.json")).map(PathBuf::as_path)
}

pub fn private_file_of(def_file: &Path) -> PathBuf {
    def_file.with_file_name(PRIVATE)
}

/// A file later in the merge chain than `def_file`'s private file that sets `name`'s password, so
/// the saved one would not take effect.
pub fn password_override(sources: &[PathBuf], def_file: &Path, name: &str) -> Option<PathBuf> {
    let private = private_file_of(def_file);
    let at = sources.iter().position(|p| p == def_file)?;
    sources[at + 1..].iter().filter(|p| **p != private).find(|p| read_map(p).is_ok_and(|m| m.get(name).and_then(|e| e.get("password")).is_some())).cloned()
}

/// `<folder of siesta.json>/<name>`: readable, and apart from other projects' connections of the
/// same name (two folders with the same name still share an account).
fn default_account(def_file: &Path, name: &str) -> String {
    match def_file.parent().and_then(Path::file_name) {
        Some(d) => format!("{}/{name}", d.to_string_lossy()),
        None => name.to_string(),
    }
}

/// Stores `password` for `name` (defined in `def_file`) and points `siesta.private.json` at it.
/// `current_account`: the account of an effective `${keychain}` marker, reused when there is one.
/// Keychain unavailable: the literal password goes into the private file and `warning` says so.
pub fn set_workspace_password(def_file: &Path, name: &str, password: &str, current_account: Option<&str>) -> Result<WorkspacePassword, String> {
    if password.is_empty() {
        return Err("password is empty".into());
    }
    let private_file = private_file_of(def_file);
    // Read first: a broken file aborts before the keychain is touched.
    let mut map = read_map(&private_file)?;
    let account = current_account.map_or_else(|| default_account(def_file, name), String::from);
    let mut warning = None;
    let value = match secrets::set(&account, password) {
        Ok(()) => marker_text(&account, name),
        Err(SecretError::Unavailable(_)) => {
            warning = Some(format!("System keychain unavailable: password saved to {}", private_file.display()));
            password.to_string()
        }
        Err(e) => return Err(e.to_string()),
    };
    let mut entry = match map.remove(name) {
        Some(Value::Object(o)) => o,
        _ => Map::new(),
    };
    entry.insert("password".into(), value.into());
    map.insert(name.into(), entry.into());
    write_atomic(&private_file, &map, true)?;
    let gitignore = unignored_in_git(&private_file);
    Ok(WorkspacePassword { account, private_file: private_file.clone(), warning, gitignore })
}

/// The `.gitignore` to append to when `file` is inside a git repo and no ignore rule covers it.
/// Pure Rust: the `.gitignore`s from the file's folder up to the repo root, then `.git/info/exclude`
/// (the user's global excludes are not consulted).
fn unignored_in_git(file: &Path) -> Option<PathBuf> {
    let dir = file.parent()?;
    let root = dir.ancestors().find(|d| d.join(".git").exists())?;
    let mut rules: Vec<Gitignore> = dir.ancestors().take_while(|d| d.starts_with(root)).map(|d| Gitignore::new(d.join(".gitignore")).0).collect();
    let mut exclude = GitignoreBuilder::new(root);
    let _ = exclude.add(root.join(".git/info/exclude"));
    rules.extend(exclude.build());
    // The nearest rule that matches decides.
    let verdict = rules.iter().map(|g| g.matched_path_or_any_parents(file, false)).find(|m| !m.is_none());
    (!matches!(verdict, Some(Match::Ignore(_)))).then(|| dir.join(".gitignore"))
}

/// Appends `siesta.private.json` to `gitignore` (created if missing).
pub fn add_to_gitignore(gitignore: &Path) -> Result<(), String> {
    let mut text = std::fs::read_to_string(gitignore).unwrap_or_default();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(PRIVATE);
    text.push('\n');
    std::fs::write(gitignore, text).map_err(|e| format!("{}: {e}", gitignore.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn json(p: &Path) -> Value {
        serde_json::from_str(&fs::read_to_string(p).unwrap()).unwrap()
    }

    #[test]
    fn writes_marker_to_private_file_only() {
        secrets::use_memory_store();
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("proj");
        fs::create_dir(&dir).unwrap();
        let def = dir.join("siesta.json");
        let team = r#"{"wp-dev": {"driver": "postgres", "host": "h"}}"#;
        fs::write(&def, team).unwrap();
        fs::write(dir.join(PRIVATE), r#"{"other": {"password": "x"}, "wp-dev": {"user": "me", "password": "literal"}}"#).unwrap();
        let out = set_workspace_password(&def, "wp-dev", "s3cret", None).unwrap();
        assert_eq!((out.account.as_str(), out.warning, out.gitignore), ("proj/wp-dev", None, None));
        assert_eq!(secrets::get("proj/wp-dev").as_deref(), Ok("s3cret"));
        assert_eq!(json(&dir.join(PRIVATE)), serde_json::json!({"other": {"password": "x"}, "wp-dev": {"user": "me", "password": "${keychain:proj/wp-dev}"}}));
        assert_eq!(fs::read_to_string(&def).unwrap(), team);
        // The same account is reused; an effective marker's account wins; empty is refused.
        assert_eq!(set_workspace_password(&def, "wp-dev", "n", Some("shared")).unwrap().account, "shared");
        assert_eq!(secrets::get("shared").as_deref(), Ok("n"));
        assert!(set_workspace_password(&def, "wp-dev", "", None).is_err());
        // Created when missing; a broken file is left alone.
        fs::remove_file(dir.join(PRIVATE)).unwrap();
        set_workspace_password(&def, "wp-new", "p", None).unwrap();
        assert_eq!(json(&dir.join(PRIVATE)), serde_json::json!({"wp-new": {"password": "${keychain:proj/wp-new}"}}));
        fs::write(dir.join(PRIVATE), "{ nope").unwrap();
        assert!(set_workspace_password(&def, "wp-new", "p", None).unwrap_err().contains("invalid JSON"));
        assert_eq!(fs::read_to_string(dir.join(PRIVATE)).unwrap(), "{ nope");
    }

    #[test]
    fn unavailable_keychain_writes_the_literal() {
        let tmp = tempfile::tempdir().unwrap();
        let def = tmp.path().join("siesta.json");
        secrets::set_unavailable(true);
        let out = set_workspace_password(&def, "wp-off", "lit", None);
        secrets::set_unavailable(false);
        let out = out.unwrap();
        assert!(out.warning.unwrap().starts_with("System keychain unavailable"));
        assert_eq!(json(&tmp.path().join(PRIVATE)), serde_json::json!({"wp-off": {"password": "lit"}}));
    }

    #[test]
    fn gitignore_check_and_append() {
        secrets::use_memory_store();
        let tmp = tempfile::tempdir().unwrap();
        let (repo, sub) = (tmp.path().join("repo"), tmp.path().join("repo/svc"));
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::create_dir_all(&sub).unwrap();
        let def = sub.join("siesta.json");
        // Not ignored: offered, appended to the folder's own .gitignore (created), then ignored.
        let out = set_workspace_password(&def, "wp-git", "p", None).unwrap();
        assert_eq!(out.gitignore, Some(sub.join(".gitignore")));
        add_to_gitignore(&sub.join(".gitignore")).unwrap();
        assert_eq!(fs::read_to_string(sub.join(".gitignore")).unwrap(), "siesta.private.json\n");
        assert_eq!(set_workspace_password(&def, "wp-git", "p", None).unwrap().gitignore, None);
        // A rule at the repo root counts, and appending keeps the existing lines.
        fs::remove_file(sub.join(".gitignore")).unwrap();
        fs::write(repo.join(".gitignore"), "*.private.json").unwrap();
        assert_eq!(set_workspace_password(&def, "wp-git", "p", None).unwrap().gitignore, None);
        fs::write(repo.join(".gitignore"), "target").unwrap();
        add_to_gitignore(&repo.join(".gitignore")).unwrap();
        assert_eq!(fs::read_to_string(repo.join(".gitignore")).unwrap(), "target\nsiesta.private.json\n");
        // Outside a repo: no prompt.
        let plain = tempfile::tempdir().unwrap();
        assert_eq!(set_workspace_password(&plain.path().join("siesta.json"), "wp-git", "p", None).unwrap().gitignore, None);
    }

    #[test]
    fn finds_the_def_file_and_overrides() {
        let tmp = tempfile::tempdir().unwrap();
        let d = tmp.path();
        let (a, ap, extra) = (d.join("siesta.json"), d.join(PRIVATE), d.join("extra.json"));
        fs::write(&ap, r#"{"n": {"password": "x"}}"#).unwrap();
        fs::write(&extra, r#"{"n": {"password": "y"}, "m": {"host": "h"}}"#).unwrap();
        let sources = vec![d.join("g.json"), a.clone(), ap.clone(), extra.clone()];
        assert_eq!(workspace_def_file(&sources), Some(a.as_path()));
        assert_eq!(password_override(&sources, &a, "n"), Some(extra));
        assert_eq!(password_override(&sources, &a, "m"), None);
        assert_eq!(password_override(&sources[..3], &a, "n"), None);
        assert_eq!(workspace_def_file(&sources[..1]), None);
    }
}
