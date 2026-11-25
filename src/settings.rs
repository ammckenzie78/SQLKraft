//! Persistent settings: recent connections (never passwords) and editor preferences.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::db::ConnectionInfo;
use crate::scripting::ScriptTarget;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub recent: Vec<ConnectionInfo>,
    pub editor_font_size: f32,
    pub dark_mode: bool,
    /// Rows kept per result set; the rest is discarded (SSMS keeps everything, but a GUI grid
    /// holding 50M rows is not useful).
    pub max_rows_per_result: usize,
    /// Server version that Script as CREATE / ALTER / DROP write for.
    pub script_target: ScriptTarget,
    /// Let `sqlkraft --mcp` (Claude Code) use this window's connections.
    pub claude_bridge: bool,
    /// Show the completion list while typing (Ctrl+Space always shows it).
    pub completion_auto_popup: bool,
    /// The directory these settings load from and save to; `None` means [`config_dir`]. Tests
    /// point it at a temp dir so they never touch the user's real settings.
    #[serde(skip)]
    pub dir: Option<PathBuf>,
    /// Server-side folders recently used in the backup file browser, per server name.
    pub recent_folders: std::collections::BTreeMap<String, Vec<String>>,
    /// The Properties pane (F4) is open.
    pub show_properties: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            recent: Vec::new(),
            editor_font_size: 14.0,
            dark_mode: true,
            max_rows_per_result: 1_000_000,
            script_target: ScriptTarget::Connected,
            claude_bridge: true,
            completion_auto_popup: true,
            dir: None,
            recent_folders: Default::default(),
            show_properties: false,
        }
    }
}

/// Where settings and the MCP bridge file live. `SQLKRAFT_CONFIG_DIR` overrides it (tests,
/// portable installs).
pub fn config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("SQLKRAFT_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    directories::ProjectDirs::from("", "", "SQLKraft").map(|d| d.config_dir().to_path_buf())
}

impl Settings {
    pub fn load() -> Self {
        match config_dir() {
            Some(dir) => Self::load_from(&dir),
            None => Self::default(),
        }
    }

    /// Loads `settings.json` from `dir` (defaults when missing); [`Settings::save`] writes back
    /// there.
    pub fn load_from(dir: &Path) -> Self {
        let mut settings: Self = std::fs::read_to_string(dir.join("settings.json"))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        settings.dir = Some(dir.to_path_buf());
        settings
    }

    fn path(&self) -> Option<PathBuf> {
        self.dir.clone().or_else(config_dir).map(|d| d.join("settings.json"))
    }

    pub fn save(&self) {
        let Some(p) = self.path() else { return };
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(json) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(p, json);
        }
    }

    pub fn remember(&mut self, info: &ConnectionInfo) {
        let mut info = info.clone();
        info.password.clear();
        self.recent.retain(|r| !(r.server.eq_ignore_ascii_case(&info.server) && r.user == info.user && r.auth == info.auth));
        self.recent.insert(0, info);
        self.recent.truncate(15);
        self.save();
    }

    /// Most recent first, at most 8 per server; saved straight away.
    pub fn remember_folder(&mut self, server: &str, folder: &str) {
        if folder.trim().is_empty() {
            return;
        }
        let list = self.recent_folders.entry(server.to_lowercase()).or_default();
        list.retain(|f| f != folder);
        list.insert(0, folder.to_string());
        list.truncate(8);
        self.save();
    }

    pub fn folders_for(&self, server: &str) -> Vec<String> {
        self.recent_folders.get(&server.to_lowercase()).cloned().unwrap_or_default()
    }
}
