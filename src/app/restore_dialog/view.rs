//! The Restore dialog's sections: source, backup sets, WITH MOVE targets and options.

use eframe::egui::{self, Color32, RichText};
use egui_extras::{Column, TableBuilder};

use super::super::file_browser::{BrowseMode, FileBrowser, human_size};
use super::{BrowseFor, RecoveryChoice, RestoreDialog, Source};
use crate::backup::BackupKind;

impl RestoreDialog {
    pub(super) fn source_ui(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("Source").strong());
        ui.horizontal(|ui| {
            let mut chosen = None;
            if ui.radio_value(&mut self.source, Source::Database, "Database backup history:").changed() && !self.source_db.is_empty() {
                chosen = Some(self.source_db.clone());
            }
            ui.add_enabled_ui(self.source == Source::Database, |ui| {
                egui::ComboBox::from_id_salt("restore_src_db").width(220.0).selected_text(self.source_db.as_str()).show_ui(ui, |ui| {
                    for d in &self.history_dbs {
                        if ui.selectable_label(*d == self.source_db, d).clicked() {
                            chosen = Some(d.clone());
                        }
                    }
                });
            });
            if let Some(d) = chosen {
                if self.target.trim().is_empty() || self.target == self.source_db {
                    self.target = d.clone();
                }
                self.load_history(&d);
            }
            if self.history_rx.busy() {
                ui.spinner();
            }
        });
        ui.horizontal(|ui| {
            if ui.radio_value(&mut self.source, Source::Device, "Backup files on the server").changed() {
                self.set_sets(Vec::new());
            }
            let ready = self.folders.folders.is_some() && self.source == Source::Device;
            if ui.add_enabled(ready, egui::Button::new("Add…")).on_hover_text("Several files picked together form one striped media set").clicked()
                && let Some(f) = &self.folders.folders
            {
                self.browser = Some((BrowseFor::Media, FileBrowser::new("Select backup files", BrowseMode::Open, &f.backup, f, &self.recent, &self.meta)));
            }
            if !self.media_rx.is_empty() {
                ui.spinner();
            }
        });
    }

    pub(super) fn sets_table(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("Backup sets to restore").strong());
        if self.sets.is_empty() {
            ui.weak(match self.source {
                Source::Database => "No disk backups of this database in msdb.",
                Source::Device => "Add backup files to see their backup sets.",
            });
            return;
        }
        ui.push_id("restore_sets", |ui| {
            TableBuilder::new(ui)
                .striped(true)
                .max_scroll_height(150.0)
                .column(Column::auto())
                .column(Column::auto().at_least(90.0))
                .column(Column::auto().at_least(100.0))
                .column(Column::auto())
                .column(Column::auto().at_least(130.0))
                .column(Column::auto().at_least(70.0))
                .column(Column::auto().at_least(120.0))
                .column(Column::remainder().clip(true))
                .header(18.0, |mut h| {
                    for t in ["", "Type", "Database", "#", "Finished", "Size", "Last LSN", "File(s)"] {
                        h.col(|ui| {
                            ui.strong(t);
                        });
                    }
                })
                .body(|mut body| {
                    for (s, inc) in self.sets.iter().zip(self.include.iter_mut()) {
                        body.row(18.0, |mut row| {
                            row.col(|ui| {
                                ui.checkbox(inc, "");
                            });
                            row.col(|ui| {
                                ui.label(format!("{}{}", s.kind().label(), if s.copy_only { " (copy)" } else { "" }));
                            });
                            row.col(|ui| {
                                ui.label(&s.database);
                            });
                            row.col(|ui| {
                                ui.label(s.position.to_string());
                            });
                            row.col(|ui| {
                                ui.monospace(&s.finish);
                            });
                            row.col(|ui| {
                                ui.monospace(human_size(s.size));
                            });
                            row.col(|ui| {
                                ui.monospace(s.last_lsn.to_string());
                            });
                            row.col(|ui| {
                                ui.label(s.files.join(", ")).on_hover_text(s.files.join("\n"));
                            });
                        });
                    }
                });
        });
    }

    pub(super) fn moves_ui(&mut self, ui: &mut egui::Ui) {
        if self.logical.is_empty() {
            return;
        }
        ui.label(RichText::new("Restore database files as (WITH MOVE)").strong());
        let mut browse = None;
        egui::Grid::new("restore_moves").num_columns(3).spacing([8.0, 4.0]).show(ui, |ui| {
            for (i, (logical, path)) in self.moves.iter_mut().enumerate() {
                let kind = self.logical.iter().find(|f| f.logical == *logical).map(|f| f.kind.as_str()).unwrap_or("");
                ui.label(format!("{logical} [{kind}]"));
                if ui.add(egui::TextEdit::singleline(path).desired_width(440.0).font(egui::TextStyle::Monospace)).changed() {
                    self.moves_edited = true;
                }
                if ui.small_button("…").on_hover_text("Choose on the server").clicked() {
                    browse = Some(i);
                }
                ui.end_row();
            }
        });
        if let (Some(i), Some(f)) = (browse, &self.folders.folders) {
            let start = self.moves[i].1.clone();
            self.browser = Some((BrowseFor::Move(i), FileBrowser::new("Restore file as", BrowseMode::Save, &start, f, &self.recent, &self.meta)));
        }
    }

    pub(super) fn options_ui(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("Options").strong());
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.replace, "Overwrite the existing database (REPLACE)");
            ui.checkbox(&mut self.keep_replication, "Preserve replication settings (KEEP_REPLICATION)");
            ui.checkbox(&mut self.checksum, "Verify checksums");
        });
        ui.horizontal(|ui| {
            ui.label("Recovery state:");
            ui.radio_value(&mut self.recovery, RecoveryChoice::Recovery, "RECOVERY").on_hover_text("Ready to use; no more backups can be restored");
            ui.radio_value(&mut self.recovery, RecoveryChoice::NoRecovery, "NORECOVERY").on_hover_text("Left restoring, so more logs can be restored");
            ui.radio_value(&mut self.recovery, RecoveryChoice::Standby, "STANDBY").on_hover_text("Read-only between log restores");
        });
        if self.recovery == RecoveryChoice::Standby {
            ui.horizontal(|ui| {
                ui.label("Standby file:");
                ui.add(egui::TextEdit::singleline(&mut self.standby_file).desired_width(400.0).font(egui::TextStyle::Monospace));
                if ui.small_button("…").clicked()
                    && let Some(f) = &self.folders.folders
                {
                    let start = f.style.join(&f.backup, &format!("{}_undo.bak", self.target.trim()));
                    self.browser = Some((BrowseFor::Standby, FileBrowser::new("Standby file", BrowseMode::Save, &start, f, &self.recent, &self.meta)));
                }
            });
        }
        let has_logs = self.steps().iter().any(|s| s.kind() == BackupKind::Log);
        ui.add_enabled_ui(has_logs, |ui| {
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.use_stop_at, "Stop at point in time (STOPAT):");
                ui.add_enabled(self.use_stop_at, egui::TextEdit::singleline(&mut self.stop_at).desired_width(160.0));
            });
        });
        ui.checkbox(&mut self.close_connections, "Close existing connections to the target database");
        if self.close_connections {
            ui.colored_label(
                Color32::from_rgb(220, 140, 60),
                "⚠ Sets the database SINGLE_USER WITH ROLLBACK IMMEDIATE: every other session is disconnected and \
                 its open transactions are rolled back.",
            );
        }
    }
}
