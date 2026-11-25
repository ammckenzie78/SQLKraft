//! Tasks → Back Up…: type, destination files on the server (picked with the server file
//! browser), options, Script and Run with `STATS` progress and Cancel.

use eframe::egui::{self, Color32, RichText};

use super::SqlKraftApp;
use super::file_browser::{Browse, BrowseMode, FileBrowser};
use super::runner::{FolderLoader, Pending, Runner, drain_meta};
use crate::backup::{self as b, BackupOptions, BackupType, Compression};
use crate::db::{ConnectionInfo, Repaint, Session};

pub(super) struct BackupDialog {
    info: ConnectionInfo,
    db: String,
    meta: Session,
    folders: FolderLoader,
    recovery_rx: Pending,
    certs_rx: Pending,
    recovery_model: Option<String>,
    certs: Vec<String>,
    error: Option<String>,
    kind: BackupType,
    copy_only: bool,
    files: Vec<String>,
    /// The single file is still the generated default, so it follows the backup type.
    auto_file: bool,
    compression: Compression,
    checksum: bool,
    verify: bool,
    init: bool,
    certificate: Option<String>,
    browser: Option<FileBrowser>,
    /// Recently used server folders, for the browser's quick links.
    pub(super) recent: Vec<String>,
    run: Option<Runner>,
}

enum Outcome {
    Script(String),
    Close,
    Folder(String),
}

impl BackupDialog {
    pub(super) fn new(rt: &tokio::runtime::Handle, repaint: Repaint, info: ConnectionInfo, db: String) -> Self {
        let meta = Session::spawn(rt, info.clone(), repaint);
        Self {
            folders: FolderLoader::new(&meta),
            recovery_rx: Pending::new(meta.fetch(b::recovery_model_query(&db))),
            certs_rx: Pending::new(meta.fetch(b::CERTIFICATES_QUERY)),
            meta,
            info,
            db,
            recovery_model: None,
            certs: Vec::new(),
            error: None,
            kind: BackupType::Full,
            copy_only: false,
            files: Vec::new(),
            auto_file: true,
            compression: Compression::ServerDefault,
            checksum: true,
            verify: false,
            init: false,
            certificate: None,
            browser: None,
            recent: Vec::new(),
            run: None,
        }
    }

    fn default_file(&self) -> Option<String> {
        let f = self.folders.folders.as_ref()?;
        let name = b::default_file_name(&self.db, self.kind, chrono::Local::now().naive_local());
        Some(f.style.join(&f.backup, &name))
    }

    fn pump(&mut self, ctx: &egui::Context) {
        if let Some(e) = drain_meta(&self.meta) {
            self.error = Some(e);
        }
        if self.folders.poll() && self.files.is_empty() {
            self.files.extend(self.default_file());
        }
        if let Some(e) = self.folders.error.take() {
            self.error = Some(format!("Could not read the server's default folders: {e}"));
        }
        if let Some(Ok((_, rows))) = self.recovery_rx.poll() {
            self.recovery_model = rows.first().and_then(|r| r.first().cloned().flatten());
        }
        if let Some(Ok((_, rows))) = self.certs_rx.poll() {
            self.certs = rows.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect();
        }
        if let Some(r) = &mut self.run {
            r.pump(&self.meta, ctx);
        }
    }

    fn options(&self) -> BackupOptions {
        BackupOptions {
            db: self.db.clone(),
            kind: self.kind,
            copy_only: self.copy_only,
            files: self.files.clone(),
            compression: self.compression,
            checksum: self.checksum,
            init: self.init,
            verify: self.verify,
            certificate: self.certificate.clone(),
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, rt: &tokio::runtime::Handle, repaint: &Repaint) -> Option<Outcome> {
        let running = self.run.as_ref().is_some_and(|r| r.running());
        let mut out = None;
        if let Some(e) = &self.error {
            ui.colored_label(Color32::from_rgb(220, 80, 80), e);
        }
        let simple = self.recovery_model.as_deref() == Some("SIMPLE");
        ui.add_enabled_ui(!running, |ui| {
            egui::Grid::new("backup_grid").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                ui.label("Database:");
                ui.label(format!("{}  (recovery model: {})", self.db, self.recovery_model.as_deref().unwrap_or("…")));
                ui.end_row();
                ui.label("Backup type:");
                ui.horizontal(|ui| {
                    let before = self.kind;
                    for k in [BackupType::Full, BackupType::Differential, BackupType::Log] {
                        let enabled = !(k == BackupType::Log && simple);
                        ui.add_enabled_ui(enabled, |ui| {
                            ui.radio_value(&mut self.kind, k, k.label())
                                .on_disabled_hover_text("Log backups need the FULL or BULK_LOGGED recovery model");
                        });
                    }
                    if self.kind != before
                        && self.auto_file
                        && let Some(d) = self.default_file()
                    {
                        self.files = vec![d];
                    }
                    ui.checkbox(&mut self.copy_only, "Copy-only").on_hover_text("Doesn't affect the differential base or the log chain");
                });
                ui.end_row();
            });
            ui.add_space(4.0);
            ui.label(RichText::new("Back up to (paths on the server)").strong());
            let mut remove = None;
            let mut edited = false;
            for (i, f) in self.files.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    edited |= ui.add(egui::TextEdit::singleline(f).desired_width(520.0).font(egui::TextStyle::Monospace)).changed();
                    if ui.small_button("×").on_hover_text("Remove").clicked() {
                        remove = Some(i);
                    }
                });
            }
            if edited {
                self.auto_file = false;
            }
            if let Some(i) = remove {
                self.files.remove(i);
                self.auto_file = false;
            }
            ui.horizontal(|ui| {
                let ready = self.folders.folders.is_some();
                if ui.add_enabled(ready, egui::Button::new("Add…")).on_hover_text("Browse the server's folders").clicked()
                    && let Some(folders) = &self.folders.folders
                {
                    let start = self.files.last().cloned().or_else(|| self.default_file()).unwrap_or_default();
                    self.browser = Some(FileBrowser::new("Back up to", BrowseMode::Save, &start, folders, &self.recent, &self.meta));
                }
                if self.files.len() > 1 {
                    ui.weak(format!("Striped across {} files: all of them are needed to restore.", self.files.len()));
                }
            });
            ui.add_space(4.0);
            ui.label(RichText::new("Options").strong());
            ui.horizontal(|ui| {
                ui.radio_value(&mut self.init, false, "Append to the existing backup set (NOINIT)");
                ui.radio_value(&mut self.init, true, "Overwrite all existing backup sets (INIT)");
            });
            ui.horizontal(|ui| {
                ui.label("Compression:");
                ui.radio_value(&mut self.compression, Compression::ServerDefault, "Server default");
                ui.radio_value(&mut self.compression, Compression::On, "Compress");
                ui.radio_value(&mut self.compression, Compression::Off, "Don't compress");
            });
            ui.horizontal(|ui| {
                ui.checkbox(&mut self.checksum, "Perform checksum");
                ui.checkbox(&mut self.verify, "Verify backup when finished (RESTORE VERIFYONLY)");
            });
            ui.horizontal(|ui| {
                let mut encrypt = self.certificate.is_some();
                ui.add_enabled_ui(!self.certs.is_empty(), |ui| {
                    if ui.checkbox(&mut encrypt, "Encrypt with server certificate").on_disabled_hover_text("No certificate with a private key in master").changed() {
                        self.certificate = if encrypt { self.certs.first().cloned() } else { None };
                    }
                });
                if let Some(cert) = &mut self.certificate {
                    egui::ComboBox::from_id_salt("backup_cert").selected_text(cert.as_str()).show_ui(ui, |ui| {
                        for c in &self.certs {
                            ui.selectable_value(cert, c.clone(), c);
                        }
                    });
                    ui.weak("AES 256; overwrites the media (FORMAT, INIT)");
                }
            });
        });
        let script = b::backup_script(&self.options());
        if let Err(e) = &script {
            ui.colored_label(Color32::from_rgb(220, 140, 60), e);
        }
        if let Some(run) = &self.run {
            ui.separator();
            run.ui(ui);
        }
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui.add_enabled(script.is_ok(), egui::Button::new("Script")).clicked()
                && let Ok(s) = &script
            {
                out = Some(Outcome::Script(s.clone()));
            }
            if ui.add_enabled(script.is_ok() && !running, egui::Button::new("▶ Run")).clicked()
                && let Ok(s) = &script
            {
                self.run = Some(Runner::start(rt, self.info.with_database("master"), repaint.clone(), s));
            }
            if running && ui.button("■ Cancel").clicked()
                && let Some(r) = &mut self.run
            {
                r.cancel();
            }
            if ui.add_enabled(!running, egui::Button::new("Close")).clicked() {
                out = Some(Outcome::Close);
            }
        });
        out
    }
}

impl SqlKraftApp {
    pub(super) fn open_backup(&mut self, server: usize, db: String) {
        let Some(s) = self.servers.get(server) else { return };
        let mut d = BackupDialog::new(&self.rt, self.repaint.clone(), s.info.clone(), db);
        d.recent = self.settings.folders_for(&s.info.server);
        self.backup = Some(d);
    }

    pub(super) fn backup_window(&mut self, ctx: &egui::Context) {
        let Some(d) = &mut self.backup else { return };
        d.pump(ctx);
        let mut outcome = None;
        if let Some(fb) = &mut d.browser {
            match fb.show(ctx, &d.meta) {
                Some(Browse::Picked { dir, paths }) => {
                    if d.auto_file {
                        d.files.clear();
                        d.auto_file = false;
                    }
                    d.files.extend(paths);
                    d.browser = None;
                    outcome = Some(Outcome::Folder(dir));
                }
                Some(Browse::Cancelled) => d.browser = None,
                None => {}
            }
        }
        let mut open = true;
        let (rt, repaint) = (self.rt.clone(), self.repaint.clone());
        let browsing = d.browser.is_some();
        egui::Window::new(format!("Back Up Database - {}", d.db))
            .id(egui::Id::unique("backup_dialog"))
            .open(&mut open)
            .resizable(true)
            .collapsible(false)
            .default_width(640.0)
            .show(ctx, |ui| {
                ui.add_enabled_ui(!browsing, |ui| {
                    if let Some(o) = d.ui(ui, &rt, &repaint) {
                        outcome = Some(o);
                    }
                });
            });
        let info = d.info.with_database("master");
        let server = d.info.server.clone();
        match outcome {
            Some(Outcome::Script(text)) => self.new_query(Some(info), text),
            Some(Outcome::Close) => self.backup = None,
            Some(Outcome::Folder(dir)) => self.settings.remember_folder(&server, &dir),
            None if !open => self.backup = None,
            None => {}
        }
    }
}
