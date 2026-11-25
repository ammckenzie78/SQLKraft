//! The connect dialog, the unsaved-changes prompt and the About window.

use eframe::egui::{self, Color32, Key};

use super::query_tab::apply_tab_event;
use super::{SqlKraftApp, server_entry};
use crate::db::{AuthKind, ConnectionInfo, Encrypt, Event, Session};

#[derive(Clone, Copy, PartialEq)]
pub(super) enum DialogPurpose {
    ObjectExplorer,
    NewQuery,
    ChangeConnection(u64),
}

pub(super) struct ConnectDialog {
    info: ConnectionInfo,
    purpose: DialogPurpose,
    pending: Option<Session>,
    error: Option<String>,
    focus_password: bool,
}

pub(super) enum Prompt {
    CloseTab(u64),
    Quit,
}

impl SqlKraftApp {
    pub(super) fn open_dialog(&mut self, purpose: DialogPurpose, info: ConnectionInfo) {
        let focus_password = info.auth.needs_password() && !info.user.is_empty();
        self.dialog = Some(ConnectDialog { info, purpose, pending: None, error: None, focus_password });
    }

    pub(super) fn pump_dialog(&mut self) {
        let Some(dialog) = &mut self.dialog else { return };
        let Some(pending) = &dialog.pending else { return };
        let mut connected = None;
        while let Ok(ev) = pending.events.try_recv() {
            match ev {
                Event::Connected(f) => connected = Some(f),
                Event::ConnectFailed(e) => {
                    dialog.error = Some(e);
                    dialog.pending = None;
                    return;
                }
                _ => {}
            }
        }
        let Some(facts) = connected else { return };
        let dialog = self.dialog.take().expect("dialog");
        let session = dialog.pending.expect("pending");
        let info = dialog.info.clone();
        self.settings.remember(&info);
        match dialog.purpose {
            DialogPurpose::ObjectExplorer => self.add_server(info, session, facts),
            DialogPurpose::NewQuery => {
                let idx = self.new_tab(String::new());
                let tab = &mut self.tabs[idx];
                tab.attach(session);
                apply_tab_event(tab, Event::Connected(facts), 0);
                // Also show it in Object Explorer, like SSMS does on first connect.
                if !self.servers.iter().any(|s| s.info.server == info.server && s.info.user == info.user) {
                    let s = Session::spawn(&self.rt, info.clone(), self.repaint.clone());
                    self.servers.push(server_entry(info, s, None));
                }
            }
            DialogPurpose::ChangeConnection(id) => {
                if let Some(i) = self.tab_index(id) {
                    let tab = &mut self.tabs[i];
                    tab.attach(session);
                    apply_tab_event(tab, Event::Connected(facts), 0);
                }
            }
        }
    }

    pub(super) fn change_connection(&mut self, idx: usize) {
        let Some(tab) = self.tabs.get(idx) else { return };
        let info = tab
            .session
            .as_ref()
            .map(|s| s.info.clone())
            .or_else(|| self.settings.recent.first().cloned())
            .unwrap_or_default();
        self.open_dialog(DialogPurpose::ChangeConnection(tab.id), info);
    }

    pub(super) fn connect_dialog(&mut self, ctx: &egui::Context) {
        let Some(dialog) = &mut self.dialog else { return };
        let mut close = false;
        let mut connect = false;
        let connecting = dialog.pending.is_some();
        let recent = self.settings.recent.clone();
        egui::Window::new("Connect to Server")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.add_enabled_ui(!connecting, |ui| {
                    egui::Grid::new("connect_grid").num_columns(2).spacing([12.0, 8.0]).show(ui, |ui| {
                        ui.label("Server name:");
                        ui.horizontal(|ui| {
                            let r = ui.add(
                                egui::TextEdit::singleline(&mut dialog.info.server)
                                    .desired_width(260.0)
                                    .hint_text("host, host,port or host\\INSTANCE"),
                            );
                            if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                                connect = true;
                            }
                            egui::ComboBox::from_id_salt("recent").selected_text("").width(20.0).show_ui(ui, |ui| {
                                for r in &recent {
                                    let label = format!("{}  ({})", r.server, r.display_user());
                                    if ui.selectable_label(false, label).clicked() {
                                        dialog.info = r.clone();
                                        dialog.focus_password = r.auth.needs_password();
                                    }
                                }
                            });
                        });
                        ui.end_row();

                        ui.label("Authentication:");
                        egui::ComboBox::from_id_salt("auth").width(300.0).selected_text(dialog.info.auth.label()).show_ui(
                            ui,
                            |ui| {
                                for k in AuthKind::available() {
                                    ui.selectable_value(&mut dialog.info.auth, k, k.label());
                                }
                            },
                        );
                        ui.end_row();

                        let needs_pw = dialog.info.auth.needs_password();
                        ui.label("Login:");
                        ui.add_enabled(
                            needs_pw,
                            egui::TextEdit::singleline(&mut dialog.info.user).desired_width(300.0).hint_text(
                                if dialog.info.auth == AuthKind::Windows { "DOMAIN\\user" } else { "sa" },
                            ),
                        );
                        ui.end_row();

                        ui.label("Password:");
                        let pw = ui.add_enabled(
                            needs_pw,
                            egui::TextEdit::singleline(&mut dialog.info.password).password(true).desired_width(300.0),
                        );
                        if dialog.focus_password {
                            pw.request_focus();
                            dialog.focus_password = false;
                        }
                        if pw.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
                            connect = true;
                        }
                        ui.end_row();

                        ui.label("Database:");
                        ui.add(
                            egui::TextEdit::singleline(&mut dialog.info.database)
                                .desired_width(300.0)
                                .hint_text("<default>"),
                        );
                        ui.end_row();

                        ui.label("Encrypt:");
                        egui::ComboBox::from_id_salt("encrypt").width(300.0).selected_text(dialog.info.encrypt.label()).show_ui(
                            ui,
                            |ui| {
                                for e in [Encrypt::Mandatory, Encrypt::Optional, Encrypt::Off] {
                                    ui.selectable_value(&mut dialog.info.encrypt, e, e.label());
                                }
                            },
                        );
                        ui.end_row();

                        ui.label("");
                        ui.checkbox(&mut dialog.info.trust_server_certificate, "Trust server certificate");
                        ui.end_row();

                        ui.label("Timeout (s):");
                        ui.add(egui::DragValue::new(&mut dialog.info.connect_timeout_secs).range(1..=300));
                        ui.end_row();
                    });
                });
                if let Some(e) = &dialog.error {
                    ui.add_space(6.0);
                    egui::ScrollArea::vertical().max_height(120.0).show(ui, |ui| {
                        ui.colored_label(Color32::from_rgb(220, 80, 80), e);
                    });
                }
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if connecting {
                        ui.spinner();
                        ui.label("Connecting…");
                        if ui.button("Cancel").clicked() {
                            dialog.pending = None;
                        }
                    } else {
                        if ui.button("Connect").clicked() {
                            connect = true;
                        }
                        if ui.button("Cancel").clicked() {
                            close = true;
                        }
                    }
                });
            });
        if ctx.input(|i| i.key_pressed(Key::Escape)) && !connecting {
            close = true;
        }
        if connect && !connecting {
            dialog.error = None;
            dialog.pending = Some(Session::spawn(&self.rt, dialog.info.clone(), self.repaint.clone()));
        }
        if close {
            self.dialog = None;
        }
    }

    pub(super) fn prompts(&mut self, ctx: &egui::Context) {
        let Some(prompt) = &self.prompt else { return };
        enum Choice {
            Save,
            Discard,
            Cancel,
        }
        let mut choice = None;
        let text = match prompt {
            Prompt::CloseTab(id) => {
                let name = self.tabs.iter().find(|t| t.id == *id).map(|t| t.title.clone()).unwrap_or_default();
                format!("Save changes to {name}?")
            }
            Prompt::Quit => {
                let n = self.tabs.iter().filter(|t| t.dirty()).count();
                format!("{n} query window(s) have unsaved changes. Quit anyway?")
            }
        };
        let is_quit = matches!(prompt, Prompt::Quit);
        egui::Modal::new(egui::Id::unique("prompt")).show(ctx, |ui| {
            ui.label(text);
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if !is_quit && ui.button("Save").clicked() {
                    choice = Some(Choice::Save);
                }
                if ui.button(if is_quit { "Quit without saving" } else { "Don't Save" }).clicked() {
                    choice = Some(Choice::Discard);
                }
                if ui.button("Cancel").clicked() {
                    choice = Some(Choice::Cancel);
                }
            });
        });
        let Some(choice) = choice else { return };
        let prompt = self.prompt.take().expect("prompt");
        match (prompt, choice) {
            (_, Choice::Cancel) => {}
            (Prompt::CloseTab(id), Choice::Save) => {
                if let Some(i) = self.tab_index(id) {
                    if self.save_tab(i, false) {
                        self.close_tab(i);
                    }
                }
            }
            (Prompt::CloseTab(id), Choice::Discard) => {
                if let Some(i) = self.tab_index(id) {
                    self.close_tab(i);
                }
            }
            (Prompt::Quit, _) => {
                self.allow_quit = true;
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }

    pub(super) fn about(&mut self, ctx: &egui::Context) {
        if !self.show_about {
            return;
        }
        let mut open = true;
        egui::Window::new("About SQLKraft").open(&mut open).collapsible(false).show(ctx, |ui| {
            ui.heading(format!("SQLKraft {}", env!("CARGO_PKG_VERSION")));
            ui.label("A SQL Server management studio for Linux, written in Rust.");
            ui.add_space(8.0);
            egui::Grid::new("keys").striped(true).show(ui, |ui| {
                for (k, d) in [
                    ("F5 / Ctrl+E", "Execute (selection, if any)"),
                    ("Alt+Break / Ctrl+Shift+C", "Cancel executing query"),
                    ("Ctrl+N", "New query (same connection)"),
                    ("Ctrl+O / Ctrl+S", "Open / save .sql file"),
                    ("Ctrl+W, middle-click tab", "Close query window"),
                    ("Ctrl+R", "Show/hide results pane"),
                    ("F4", "Show/hide Properties window"),
                    ("Ctrl+= / Ctrl+-", "Editor font size"),
                    ("Ctrl+C in grid", "Copy selected cell"),
                ] {
                    ui.monospace(super::keys(k));
                    ui.label(d);
                    ui.end_row();
                }
            });
        });
        self.show_about = open;
    }
}
