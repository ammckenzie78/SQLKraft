//! Claude Code integration in the window: the MCP bridge, the activity log and the menu.

use std::collections::VecDeque;

use eframe::egui;

use super::{ConnState, SqlKraftApp};
use crate::mcp::ActiveQuery;
use crate::mcp::bridge::{Bridge, GuiCall, GuiConnection};
use crate::mcp::headless::connection_name;

const LOG_LINES: usize = 300;

#[derive(Default)]
pub(super) struct Claude {
    bridge: Option<(Bridge, std::sync::mpsc::Receiver<GuiCall>)>,
    clients: usize,
    log: VecDeque<(String, String)>,
    pub(super) show_log: bool,
    error: Option<String>,
}

impl SqlKraftApp {
    /// Starts or stops the bridge to match the setting.
    pub(super) fn sync_claude_bridge(&mut self) {
        let want = self.settings.claude_bridge;
        if want && self.claude.bridge.is_none() {
            match Bridge::start(self.repaint.clone()) {
                Ok(b) => {
                    self.claude.bridge = Some(b);
                    self.claude.error = None;
                }
                Err(e) => self.claude.error = Some(format!("Could not start the Claude Code bridge: {e}")),
            }
        } else if !want && self.claude.bridge.is_some() {
            self.claude.bridge = None;
            self.claude.clients = 0;
        }
    }

    pub(super) fn pump_claude(&mut self) {
        let mut calls = Vec::new();
        if let Some((_, rx)) = &self.claude.bridge {
            while let Ok(c) = rx.try_recv() {
                calls.push(c);
            }
        }
        for call in calls {
            match call {
                GuiCall::Connections(reply) => {
                    let list = self.claude_connections();
                    let _ = reply.send(list);
                }
                GuiCall::ActiveQuery(reply) => {
                    let q = self.active_query();
                    let _ = reply.send(q);
                }
                GuiCall::OpenTab { connection, database, sql, reply } => {
                    let info = connection
                        .as_deref()
                        .and_then(|name| self.claude_connections().into_iter().find(|c| c.name == name).map(|c| c.info));
                    let r = match (connection, info) {
                        (Some(name), None) => Err(anyhow::anyhow!("No connection named `{name}`")),
                        (_, info) => {
                            let info = match (info, database) {
                                (Some(i), Some(db)) => Some(i.with_database(&db)),
                                (i, _) => i,
                            };
                            self.new_query(info, sql);
                            self.claude_log("open_query_tab", "opened a script for you to review");
                            Ok(())
                        }
                    };
                    let _ = reply.send(r);
                }
                GuiCall::Log { tool, summary } => self.claude_log(&tool, &summary),
                GuiCall::ClientConnected => self.claude.clients += 1,
                GuiCall::ClientDisconnected => self.claude.clients = self.claude.clients.saturating_sub(1),
            }
        }
    }

    fn claude_log(&mut self, tool: &str, summary: &str) {
        let line = if summary.is_empty() { tool.to_string() } else { format!("{tool}  {summary}") };
        self.claude.log.push_back((chrono::Local::now().format("%H:%M:%S").to_string(), line));
        while self.claude.log.len() > LOG_LINES {
            self.claude.log.pop_front();
        }
    }

    /// Object Explorer servers first, then query-tab connections not already listed.
    fn claude_connections(&self) -> Vec<GuiConnection> {
        let mut out: Vec<GuiConnection> = Vec::new();
        for s in self.servers.iter().filter(|s| s.state == ConnState::Connected) {
            let name = connection_name(&s.info);
            if !out.iter().any(|c| c.name == name) {
                out.push(GuiConnection { name, info: s.info.clone(), writes_allowed: s.claude_writes });
            }
        }
        for t in &self.tabs {
            let Some(sess) = &t.session else { continue };
            let name = connection_name(&sess.info);
            if !out.iter().any(|c| c.name == name) {
                let mut info = sess.info.clone();
                info.database.clear();
                out.push(GuiConnection { name, info, writes_allowed: false });
            }
        }
        out
    }

    fn active_query(&self) -> Option<ActiveQuery> {
        let tab = self.tabs.get(self.active)?;
        let selection = tab.selection.filter(|(a, b)| a != b).map(|(a, b)| {
            let (a, b) = (a.min(b), a.max(b));
            tab.text.chars().skip(a).take(b - a).collect::<String>()
        });
        Some(ActiveQuery {
            title: tab.title.clone(),
            connection: tab.session.as_ref().map(|s| connection_name(&s.info)),
            database: (!tab.current_db.is_empty()).then(|| tab.current_db.clone()),
            text: tab.text.clone(),
            selection,
        })
    }

    /// The Claude menu in the menu bar, plus a status dot while Claude Code is connected.
    pub(super) fn claude_menu(&mut self, ui: &mut egui::Ui) {
        let label = if self.claude.clients > 0 { "Claude ●" } else { "Claude" };
        ui.menu_button(label, |ui| {
            if ui.checkbox(&mut self.settings.claude_bridge, "Let Claude Code use this window").changed() {
                self.settings.save();
                self.sync_claude_bridge();
            }
            ui.label(match (&self.claude.bridge, self.claude.clients) {
                (None, _) => "Off. `sqlkraft --mcp` connects on its own.".to_string(),
                (Some(_), 0) => "Waiting for Claude Code.".to_string(),
                (Some(_), n) => format!("{n} Claude Code session{} connected.", if n == 1 { "" } else { "s" }),
            });
            if let Some(e) = &self.claude.error {
                ui.colored_label(egui::Color32::from_rgb(220, 80, 80), e);
            }
            ui.separator();
            if ui.button("Copy Claude Code setup command").on_hover_text("Run it once in a terminal").clicked() {
                ui.ctx().copy_text(setup_command());
                self.toast("Copied. Run it in a terminal, then restart Claude Code.");
                ui.close();
            }
            if ui.button("Show activity").clicked() {
                self.claude.show_log = true;
                ui.close();
            }
            ui.label("Writes: right-click a server → Allow Claude to modify data.");
        });
    }

    pub(super) fn claude_log_window(&mut self, ctx: &egui::Context) {
        let mut open = self.claude.show_log;
        egui::Window::new("Claude activity").open(&mut open).default_size([560.0, 320.0]).show(ctx, |ui| {
            if self.claude.log.is_empty() {
                ui.label("Nothing yet. Tool calls from Claude Code appear here.");
            }
            egui::ScrollArea::vertical().stick_to_bottom(true).show(ui, |ui| {
                for (at, line) in &self.claude.log {
                    ui.horizontal_wrapped(|ui| {
                        ui.weak(at);
                        ui.monospace(line);
                    });
                }
            });
        });
        self.claude.show_log = open;
    }
}

/// `claude mcp add` for this executable, at user scope so it works in every project.
pub fn setup_command() -> String {
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "sqlkraft".into());
    format!("claude mcp add --scope user sqlkraft -- \"{exe}\" --mcp")
}
