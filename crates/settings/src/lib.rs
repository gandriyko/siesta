//! Settings: `~/.config/siesta/settings.json`, the only layer. Also the two folders every other
//! path comes from: [`config_dir`] (what the user writes) and [`data_dir`] (what Siesta writes).
//!
//! JSON with `//` and `/* */` comments, nested by group (`results.page_size` is
//! `{"results": {"page_size": …}}`). Missing keys take defaults, unknown keys are ignored.

use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// Which variant of the theme is shown; `System` follows the OS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Appearance {
    #[default]
    System,
    Light,
    Dark,
}

/// Extra shortcuts on top of Siesta's own; `keymap.json` next to the settings file wins over both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Keymap {
    #[default]
    Default,
    Vscode,
    Jetbrains,
}

/// Editor and result panel side by side (`horizontal`) or stacked (`vertical`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Split {
    Horizontal,
    #[default]
    Vertical,
}

/// When a destructive statement (`DROP`, `TRUNCATE`, `DELETE`/`UPDATE` without `WHERE`) asks for confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ConfirmDestructive {
    /// Only on connections with `"prod": true`.
    #[default]
    Prod,
    Always,
    Never,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub appearance: Appearance,
    pub keymap: Keymap,
    /// Cmd+\ toggles it.
    pub split: Split,
    /// The editor's share of the editor + result area, 0.2–0.8.
    pub split_ratio: f32,
    /// Theme family (`"macOS Classic"` by default, `"Gruvbox"`, …); `"Default"` = gpui-component's built-in.
    pub theme: String,
    /// UI text size, px.
    pub ui_font_size: f32,
    /// Editor and result font; empty = the theme's monospace font.
    pub font_family: String,
    /// Editor and result font size, px.
    pub font_size: f32,
    pub editor: Editor,
    pub results: Results,
    pub run: Run,
    pub history: History,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Editor {
    pub line_numbers: bool,
    pub soft_wrap: bool,
    /// Background on the line under the cursor.
    pub highlight_current_line: bool,
    /// A green ▶ next to each statement's line number (as Respite's `send_button`).
    pub run_button: bool,
}

/// The result table (`"results": {…}`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Results {
    /// Rows per page, fetched from the server.
    pub page_size: usize,
    /// Hard cap on rows a single statement returns (CLI / export).
    pub max_rows: usize,
    /// Cell values longer than this are cut in the grid; the full value opens in a popup.
    pub cell_preview_kb: usize,
    /// How SQL NULL is shown.
    pub null_text: String,
    /// JSON cells show this many lines of pretty-printed JSON (rows get taller); 0 = off.
    pub json_preview_lines: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Run {
    /// A failed statement stops the rest of a multi-statement run.
    pub stop_on_error: bool,
    pub confirm_destructive: ConfirmDestructive,
    /// Server-side timeout per statement; `null` = no limit.
    pub statement_timeout_ms: Option<u64>,
    /// How long opening a connection (also cancel and reconnect) may take before it fails.
    pub connect_timeout_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct History {
    pub total: usize,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            appearance: Appearance::System,
            keymap: Keymap::Default,
            split: Split::Vertical,
            split_ratio: 0.55,
            theme: "macOS Classic".into(),
            ui_font_size: 14.,
            font_family: String::new(),
            font_size: 14.,
            editor: Editor::default(),
            results: Results::default(),
            run: Run::default(),
            history: History::default(),
        }
    }
}

impl Default for Editor {
    fn default() -> Self {
        Self { line_numbers: true, soft_wrap: false, highlight_current_line: false, run_button: true }
    }
}

impl Default for Results {
    fn default() -> Self {
        Self { page_size: 100, max_rows: 10_000, cell_preview_kb: 8, null_text: "NULL".into(), json_preview_lines: 0 }
    }
}

impl Default for Run {
    fn default() -> Self {
        Self { stop_on_error: true, confirm_destructive: ConfirmDestructive::Prod, statement_timeout_ms: None, connect_timeout_ms: 10_000 }
    }
}

impl Default for History {
    fn default() -> Self {
        Self { total: 500 }
    }
}

/// The settings file as written, plus the result merged over the defaults.
#[derive(Clone)]
pub struct Loaded {
    pub global_path: Option<PathBuf>,
    global: Value,
    pub settings: Settings,
    /// Unreadable / invalid file (then the defaults apply).
    pub errors: Vec<String>,
}

/// A folder from `var` (non-empty), else `~/<home_rel>`.
fn dir_from(var: &str, home_rel: &str) -> Option<PathBuf> {
    match std::env::var_os(var) {
        Some(d) if !d.is_empty() => Some(PathBuf::from(d)),
        _ => Some(std::env::home_dir()?.join(home_rel)),
    }
}

/// `~/.config/siesta` (`SIESTA_CONFIG_DIR` replaces it): only what the user writes — `settings.json`,
/// `keymap.json`, `connections/`, `collections/`. Safe to keep in a dotfiles repo.
pub fn config_dir() -> Option<PathBuf> {
    dir_from("SIESTA_CONFIG_DIR", ".config/siesta")
}

/// `~/.siesta` (`SIESTA_DATA_DIR` replaces it): only what Siesta writes for itself — `state.json`,
/// `history/`, `cache/`, the password `key`. Never for a dotfiles repo.
pub fn data_dir() -> Option<PathBuf> {
    dir_from("SIESTA_DATA_DIR", ".siesta")
}

/// `<config>/collections`: the central folder of the user's `.sql` files (`~/.config/siesta/collections`).
pub fn collections_dir_in(config: &Path) -> PathBuf {
    config.join("collections")
}

/// `<config_dir>/settings.json`.
pub fn global_path() -> Option<PathBuf> {
    Some(config_dir()?.join("settings.json"))
}

impl Loaded {
    pub fn load() -> Self {
        Self::load_from(global_path())
    }

    pub fn load_from(global_path: Option<PathBuf>) -> Self {
        let mut errors = vec![];
        let global = match global_path.as_deref().map(read_jsonc) {
            Some(Ok(v)) => v,
            Some(Err(e)) => {
                errors.push(format!("{}: {e}", global_path.as_ref().unwrap().display()));
                Value::Object(Map::new())
            }
            None => Value::Object(Map::new()),
        };
        let mut loaded = Self { global_path, global, settings: Settings::default(), errors };
        loaded.merge();
        loaded
    }

    fn merge(&mut self) {
        let mut v = serde_json::to_value(Settings::default()).expect("defaults serialize");
        deep_merge(&mut v, &self.global);
        match serde_json::from_value(v) {
            Ok(s) => self.settings = s,
            Err(e) => self.errors.push(format!("settings: {e}")),
        }
    }

    /// Set a dotted key (`"results.page_size"`) in the global file and re-merge.
    /// Comments in the global file are not preserved once the UI writes it.
    pub fn set(&mut self, key: &str, value: Value) -> io::Result<()> {
        let path = self.global_path.clone().ok_or_else(|| io::Error::other("no home directory"))?;
        let mut node = &mut self.global;
        for part in key.split('.') {
            if !node.is_object() {
                *node = Value::Object(Map::new());
            }
            node = node.as_object_mut().unwrap().entry(part).or_insert(Value::Null);
        }
        *node = value;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(&self.global)? + "\n")?;
        self.merge();
        Ok(())
    }
}

fn read_jsonc(path: &Path) -> Result<Value, String> {
    match std::fs::File::open(path) {
        Ok(f) => serde_json::from_reader(json_comments::StripComments::new(f)).map_err(|e| e.to_string()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Value::Object(Map::new())),
        Err(e) => Err(e.to_string()),
    }
}

/// Objects merge key by key; anything else (arrays included) replaces.
fn deep_merge(base: &mut Value, over: &Value) {
    match (base, over) {
        (Value::Object(b), Value::Object(o)) => {
            for (k, v) in o {
                deep_merge(b.entry(k.clone()).or_insert(Value::Null), v);
            }
        }
        (b, o) => *b = o.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults() {
        let s = Settings::default();
        assert_eq!((s.results.page_size, s.results.max_rows, s.results.cell_preview_kb), (100, 10_000, 8));
        assert_eq!(s.results.null_text, "NULL");
        assert!(s.run.stop_on_error);
        assert_eq!((s.run.confirm_destructive, s.run.statement_timeout_ms), (ConfirmDestructive::Prod, None));
        assert_eq!(s.run.connect_timeout_ms, 10_000);
        assert_eq!((s.results.json_preview_lines, s.history.total), (0, 500));
        // an empty layer set yields exactly the defaults
        assert_eq!(Loaded::load_from(None).settings, s);
    }

    #[test]
    fn comments_and_set() {
        let dir = tempfile::tempdir().unwrap();
        let g = dir.path().join("cfg/settings.json");
        std::fs::create_dir_all(g.parent().unwrap()).unwrap();
        std::fs::write(
            &g,
            "{\n // small pages\n \"results\": {\"page_size\": 50}, \"appearance\": \"light\", /* why not */\n \"run\": {\"confirm_destructive\": \"always\", \"statement_timeout_ms\": 5000}\n}",
        )
        .unwrap();

        let mut l = Loaded::load_from(Some(g.clone()));
        assert!(l.errors.is_empty(), "{:?}", l.errors);
        assert_eq!((l.settings.appearance, l.settings.results.page_size), (Appearance::Light, 50));
        assert_eq!((l.settings.run.confirm_destructive, l.settings.run.statement_timeout_ms), (ConfirmDestructive::Always, Some(5000)));
        assert_eq!(l.settings.results.max_rows, 10_000); // default kept beside the override

        l.set("font_size", 16.into()).unwrap();
        l.set("results.page_size", 200.into()).unwrap();
        assert_eq!((l.settings.font_size, l.settings.results.page_size), (16., 200));
        let written: Value = serde_json::from_str(&std::fs::read_to_string(&g).unwrap()).unwrap();
        assert_eq!(written["results"], serde_json::json!({"page_size": 200}));
        assert_eq!(written["font_size"], 16);

        std::fs::write(&g, "{ broken").unwrap();
        let l = Loaded::load_from(Some(g));
        assert_eq!((l.errors.len(), l.settings.font_size), (1, 14.));
    }

    #[test]
    fn dirs_come_from_their_variables() {
        // ponytail: one test touches these variables, so no lock is needed.
        unsafe { std::env::set_var("SIESTA_CONFIG_DIR", "/c") };
        unsafe { std::env::set_var("SIESTA_DATA_DIR", "/d") };
        assert_eq!((config_dir(), data_dir(), global_path()), (Some("/c".into()), Some("/d".into()), Some("/c/settings.json".into())));
        unsafe { std::env::set_var("SIESTA_DATA_DIR", "") };
        let home = std::env::home_dir().unwrap();
        assert_eq!(data_dir(), Some(home.join(".siesta")));
        unsafe { std::env::remove_var("SIESTA_CONFIG_DIR") };
        assert_eq!(config_dir(), Some(home.join(".config/siesta")));
    }

    #[test]
    fn invalid_value_keeps_defaults_and_unknown_keys_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let w = dir.path().join("s.json");
        std::fs::write(&w, r#"{"mystery": 1, "run": {"confirm_destructive": "sometimes"}}"#).unwrap();
        let l = Loaded::load_from(Some(w));
        assert_eq!(l.errors.len(), 1);
        assert_eq!(l.settings, Settings::default());
    }
}
