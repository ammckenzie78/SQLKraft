//! Persistent settings: recent connections (never passwords) and editor preferences.

use std::path::PathBuf;

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

fn path() -> Option<PathBuf> {
    config_dir().map(|d| d.join("settings.json"))
}

impl Settings {
    pub fn load() -> Self {
        path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let Some(p) = path() else { return };
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
}
