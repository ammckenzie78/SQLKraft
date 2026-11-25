//! Tasks → Shrink → Database… / Files…: the numbers needed to decide whether a shrink is worth
//! it, then DBCC SHRINKDATABASE / SHRINKFILE on its own connection with progress and Cancel.

use eframe::egui::{self, Color32, RichText};
use egui_extras::{Column, TableBuilder};

use super::SqlKraftApp;
use super::runner::{Pending, Runner, drain_meta};
use crate::db::{ConnectionInfo, Repaint, Session};
use crate::maintenance::{self as m, DbFile, FileType, ShrinkFileAction};

#[derive(Clone, Copy, PartialEq)]
enum FileChoice {
    Release,
    Reorganize,
    Empty,
}

pub(super) struct ShrinkDialog {
    info: ConnectionInfo,
    db: String,
    /// Shrink Database rather than Shrink Files.
    database: bool,
    meta: Session,
    files_rx: Pending,
    files: Vec<DbFile>,
    error: Option<String>,
    log_rx: Pending,
    vlf_rx: Pending,
    /// (log_reuse_wait_desc, recovery model)
    log_reuse: Option<(String, String)>,
    vlfs: Option<String>,
    file_type: FileType,
    selected: Option<i32>,
    choice: FileChoice,
    target_mb: u64,
    /// SHRINKDATABASE: reorganize to leave this much free, else TRUNCATEONLY.
    reorganize_db: bool,
    target_pct: u8,
    low_priority_ok: bool,
    low_priority: bool,
    run: Option<Runner>,
    /// Size (MB) of what was shrunk, when Run was pressed.
    before_mb: Option<f64>,
    after_mb: Option<f64>,
}

enum Outcome {
    Script(String),
    Close,
}

impl ShrinkDialog {
    pub(super) fn new(rt: &tokio::runtime::Handle, repaint: Repaint, info: ConnectionInfo, db: String, database: bool, product_version: &str) -> Self {
        let meta = Session::spawn(rt, info.clone(), repaint);
        let mut d = Self {
            info,
            db,
            database,
            meta,
            files_rx: Pending::default(),
            files: Vec::new(),
            error: None,
            log_rx: Pending::default(),
            vlf_rx: Pending::default(),
            log_reuse: None,
            vlfs: None,
            file_type: FileType::Data,
            selected: None,
            choice: FileChoice::Release,
            target_mb: 0,
            reorganize_db: false,
            target_pct: 10,
            low_priority_ok: m::supports_low_priority(product_version),
            low_priority: false,
            run: None,
            before_mb: None,
            after_mb: None,
        };
        d.reload();
        d
    }

    fn reload(&mut self) {
        self.files_rx = Pending::new(self.meta.fetch(m::files_query(&self.db)));
        self.log_rx = Pending::new(self.meta.fetch(m::log_reuse_query(&self.db)));
        self.vlf_rx = Pending::new(self.meta.fetch(m::vlf_count_query(&self.db)));
    }

    fn selected_file(&self) -> Option<&DbFile> {
        self.files.iter().find(|f| Some(f.file_id) == self.selected)
    }

    fn select(&mut self, id: i32) {
        self.selected = Some(id);
        if let Some(f) = self.selected_file() {
            let used = f.used_mb.unwrap_or(0.0).ceil() as u64;
            self.target_mb = used.max(1);
        }
    }

    fn pump(&mut self, ctx: &egui::Context) {
        if let Some(e) = drain_meta(&self.meta) {
            self.error = Some(e);
        }
        if let Some(r) = self.files_rx.poll() {
            match r {
                Ok((_, rows)) => {
                    self.files = m::parse_files(&rows);
                    if self.before_mb.is_some() && self.run.as_ref().is_some_and(|r| !r.running()) {
                        self.after_mb = Some(self.measured_mb());
                    }
                    let keep = self.selected.filter(|id| self.files.iter().any(|f| f.file_id == *id));
                    match keep {
                        Some(id) => self.selected = Some(id),
                        None => {
                            if let Some(id) = self.files.iter().find(|f| f.file_type == self.file_type).map(|f| f.file_id) {
                                self.select(id);
                            }
                        }
                    }
                }
                Err(e) => self.error = Some(e),
            }
        }
        if let Some(Ok((_, rows))) = self.log_rx.poll()
            && let Some(r) = rows.first()
        {
            let s = |i: usize| r.get(i).cloned().flatten().unwrap_or_default();
            self.log_reuse = Some((s(0), s(1)));
        }
        if let Some(r) = self.vlf_rx.poll() {
            self.vlfs = Some(match r {
                Ok((_, rows)) => rows.first().and_then(|r| r.first().cloned().flatten()).unwrap_or_default(),
                Err(e) if m::is_permission_error(&e) => "n/a (needs VIEW SERVER STATE)".into(),
                Err(_) => "n/a (needs SQL Server 2016 SP2 or later)".into(),
            });
        }
        let mut finished = false;
        if let Some(run) = &mut self.run {
            let was = run.running();
            run.pump(&self.meta, ctx);
            finished = was && !run.running();
        }
        if finished {
            self.reload();
        }
    }

    /// The size being shrunk: the selected file, or the whole database.
    fn measured_mb(&self) -> f64 {
        if self.database {
            m::total_size_mb(&self.files)
        } else {
            self.selected_file().map(|f| f.size_mb).unwrap_or(0.0)
        }
    }

    /// The script for the current choices, or why it can't be built.
    fn script(&self) -> Result<String, String> {
        if self.database {
            return Ok(m::shrink_database_script(&self.db, self.target_pct, !self.reorganize_db, self.low_priority));
        }
        let f = self.selected_file().ok_or("Select a file.")?;
        let action = match self.choice {
            FileChoice::Release => ShrinkFileAction::TruncateOnly,
            FileChoice::Reorganize => {
                m::validate_target(f, self.target_mb)?;
                ShrinkFileAction::Reorganize { target_mb: self.target_mb }
            }
            FileChoice::Empty => {
                if !m::can_empty(&self.files, f) {
                    return Err("EMPTYFILE needs another file of the same type in the filegroup.".into());
                }
                ShrinkFileAction::EmptyFile
            }
        };
        Ok(m::shrink_file_script(&self.db, &f.name, action, self.low_priority))
    }

    fn ui(&mut self, ui: &mut egui::Ui, rt: &tokio::runtime::Handle, repaint: &Repaint) -> Option<Outcome> {
        let running = self.run.as_ref().is_some_and(|r| r.running());
        let mut out = None;
        if let Some(e) = &self.error {
            ui.colored_label(Color32::from_rgb(220, 80, 80), e);
        }
        ui.add_enabled_ui(!running, |ui| {
            if !self.database {
                ui.horizontal(|ui| {
                    ui.label("File type:");
                    for t in FileType::ALL {
                        if ui.radio_value(&mut self.file_type, t, t.label()).changed() {
                            if let Some(id) = self.files.iter().find(|f| f.file_type == t).map(|f| f.file_id) {
                                self.select(id);
                            } else {
                                self.selected = None;
                            }
                        }
                    }
                });
            }
            self.files_table(ui);
            if self.database {
                self.database_options(ui);
            } else {
                self.file_details(ui);
                self.file_options(ui);
            }
            if self.low_priority_ok {
                ui.checkbox(&mut self.low_priority, "Wait at low priority, so the shrink doesn't block the workload");
            }
        });
        if self.database || self.file_type != FileType::Log {
            ui.label(
                RichText::new(
                    "Shrinking data files fragments indexes, and the files regrow (which takes time) if the \
                     space is needed again. Consider rebuilding indexes afterwards.",
                )
                .weak()
                .small(),
            );
        }
        let script = self.script();
        if let Err(e) = &script {
            ui.colored_label(Color32::from_rgb(220, 140, 60), e);
        }
        if let Some(run) = &self.run {
            ui.separator();
            run.ui(ui);
            if let (Some(b), Some(a)) = (self.before_mb, self.after_mb) {
                ui.label(format!("Before: {b:.2} MB   After: {a:.2} MB   Released: {:.2} MB", (b - a).max(0.0)));
            }
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let ok = script.is_ok() && !running && !self.files.is_empty();
            let script_btn = ui.add_enabled(script.is_ok(), egui::Button::new("Script"));
            if script_btn.on_hover_text("Open the T-SQL in a new query window").clicked()
                && let Ok(s) = &script
            {
                out = Some(Outcome::Script(s.clone()));
            }
            if ui.add_enabled(ok, egui::Button::new("▶ Run")).clicked()
                && let Ok(s) = &script
            {
                self.before_mb = Some(self.measured_mb());
                self.after_mb = None;
                self.run = Some(Runner::start(rt, self.info.with_database(&self.db), repaint.clone(), s));
            }
            if running
                && ui.button("■ Cancel").on_hover_text("Stops the shrink; the work already done is kept").clicked()
                && let Some(r) = &mut self.run
            {
                r.cancel();
            }
            if ui.add_enabled(!running, egui::Button::new("Refresh")).clicked() {
                self.reload();
            }
            if ui.add_enabled(!running, egui::Button::new("Close")).clicked() {
                out = Some(Outcome::Close);
            }
            if self.files_rx.busy() {
                ui.spinner();
            }
        });
        out
    }

    fn files_table(&mut self, ui: &mut egui::Ui) {
        let rows: Vec<DbFile> = if self.database {
            self.files.clone()
        } else {
            self.files.iter().filter(|f| f.file_type == self.file_type).cloned().collect()
        };
        let mut clicked = None;
        let mb = |v: Option<f64>| v.map(|v| format!("{v:.2}")).unwrap_or_else(|| "-".into());
        ui.push_id("shrink_files", |ui| {
            TableBuilder::new(ui)
                .striped(true)
                .sense(egui::Sense::click())
                .max_scroll_height(140.0)
                .column(Column::auto().at_least(110.0))
                .column(Column::auto().at_least(80.0))
                .columns(Column::auto().at_least(64.0), 4)
                .column(Column::remainder().at_least(160.0).clip(true))
                .header(18.0, |mut h| {
                    for t in ["Logical name", "Filegroup", "Size (MB)", "Used (MB)", "Free (MB)", "% free", "Physical path"] {
                        h.col(|ui| {
                            ui.strong(t);
                        });
                    }
                })
                .body(|mut body| {
                    for f in &rows {
                        body.row(18.0, |mut row| {
                            row.set_selected(!self.database && Some(f.file_id) == self.selected);
                            row.col(|ui| {
                                ui.label(&f.name);
                            });
                            row.col(|ui| {
                                ui.label(&f.filegroup);
                            });
                            row.col(|ui| {
                                ui.monospace(format!("{:.2}", f.size_mb));
                            });
                            row.col(|ui| {
                                ui.monospace(mb(f.used_mb));
                            });
                            row.col(|ui| {
                                ui.monospace(mb(f.free_mb()));
                            });
                            row.col(|ui| {
                                ui.monospace(f.free_pct().map(|p| format!("{p:.1}%")).unwrap_or_else(|| "-".into()));
                            });
                            row.col(|ui| {
                                ui.label(&f.physical_name).on_hover_text(&f.physical_name);
                            });
                            if row.response().clicked() {
                                clicked = Some(f.file_id);
                            }
                        });
                    }
                });
        });
        if let Some(id) = clicked.filter(|_| !self.database) {
            self.select(id);
        }
        if rows.is_empty() && !self.files_rx.busy() {
            ui.weak(format!("No {} files.", self.file_type.label().to_lowercase()));
        }
    }

    fn database_options(&mut self, ui: &mut egui::Ui) {
        let size = m::total_size_mb(&self.files);
        let free: f64 = self.files.iter().filter_map(|f| f.free_mb()).sum();
        ui.label(format!("Database size: {size:.2} MB, of which {free:.2} MB is free."));
        ui.checkbox(&mut self.reorganize_db, "Reorganize files before releasing unused space");
        ui.add_enabled_ui(self.reorganize_db, |ui| {
            ui.horizontal(|ui| {
                ui.label("Maximum free space in files after shrinking:");
                ui.add(egui::DragValue::new(&mut self.target_pct).range(0..=99).suffix(" %"));
            });
        });
        if !self.reorganize_db {
            ui.weak("Releases unused space at the end of each file (TRUNCATEONLY); no pages are moved.");
        }
    }

    fn file_details(&self, ui: &mut egui::Ui) {
        let Some(f) = self.selected_file() else { return };
        if f.file_type != FileType::Log {
            return;
        }
        egui::Grid::new("log_info").num_columns(2).spacing([12.0, 4.0]).show(ui, |ui| {
            if let Some((wait, model)) = &self.log_reuse {
                ui.label("Recovery model:");
                ui.label(model);
                ui.end_row();
                ui.label("Log reuse wait:");
                ui.monospace(wait);
                ui.end_row();
            }
            ui.label("Virtual log files:");
            ui.label(self.vlfs.as_deref().unwrap_or("…"));
            ui.end_row();
        });
        if let Some(hint) = self.log_reuse.as_ref().and_then(|(w, _)| m::log_reuse_hint(w)) {
            ui.colored_label(Color32::from_rgb(220, 140, 60), hint);
        }
    }

    fn file_options(&mut self, ui: &mut egui::Ui) {
        let Some(f) = self.selected_file().cloned() else { return };
        ui.label(RichText::new("Shrink action").strong());
        ui.radio_value(&mut self.choice, FileChoice::Release, "Release unused space (TRUNCATEONLY)");
        ui.horizontal(|ui| {
            ui.radio_value(&mut self.choice, FileChoice::Reorganize, "Reorganize pages, then shrink file to");
            let min = f.used_mb.unwrap_or(0.0).ceil() as u64;
            let max = (f.size_mb.floor() as u64).max(min);
            ui.add_enabled(
                self.choice == FileChoice::Reorganize,
                egui::DragValue::new(&mut self.target_mb).range(min..=max).suffix(" MB"),
            );
            ui.weak(format!("(minimum {min} MB)"));
        });
        let can_empty = f.file_type != FileType::Log && m::can_empty(&self.files, &f);
        ui.add_enabled_ui(can_empty, |ui| {
            ui.radio_value(&mut self.choice, FileChoice::Empty, "Empty file by migrating the data to other files in the same filegroup")
                .on_disabled_hover_text("Needs another file of the same type in this filegroup");
        });
    }
}

impl SqlKraftApp {
    pub(super) fn open_shrink(&mut self, server: usize, db: String, database: bool) {
        let Some(s) = self.servers.get(server) else { return };
        let version = s.facts.as_ref().map(|f| f.product_version.clone()).unwrap_or_default();
        let info = s.info.clone();
        self.shrink = Some(ShrinkDialog::new(&self.rt, self.repaint.clone(), info, db, database, &version));
    }

    pub(super) fn shrink_window(&mut self, ctx: &egui::Context) {
        let Some(d) = &mut self.shrink else { return };
        d.pump(ctx);
        let title = format!("Shrink {} - {}", if d.database { "Database" } else { "Files" }, d.db);
        let mut open = true;
        let mut outcome = None;
        let (rt, repaint) = (self.rt.clone(), self.repaint.clone());
        egui::Window::new(title)
            .id(egui::Id::unique("shrink_dialog"))
            .open(&mut open)
            .resizable(true)
            .collapsible(false)
            .default_width(720.0)
            .show(ctx, |ui| outcome = d.ui(ui, &rt, &repaint));
        let info = d.info.with_database(&d.db);
        match outcome {
            Some(Outcome::Script(text)) => self.new_query(Some(info), text),
            Some(Outcome::Close) => self.shrink = None,
            None if !open => self.shrink = None,
            None => {}
        }
    }
}
