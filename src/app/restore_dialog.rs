//! Tasks → Restore → Database…: backup sets from msdb history or from files on the server,
//! an automatic restore chain you can untick, WITH MOVE targets, recovery options, and
//! Script / Run with `STATS` progress and Cancel. Object Explorer refreshes afterwards.

use eframe::egui::{self, Color32};

use super::SqlKraftApp;
use super::file_browser::{Browse, FileBrowser};
use super::runner::{FolderLoader, Pending, Runner, drain_meta};
use crate::backup::{self as b, BackupKind, BackupSet, LogicalFile, RecoveryState, RestoreOptions, RestoreStep};
use crate::db::{ConnectionInfo, Repaint, Session};
use crate::explorer::{Children, NodeKind};

#[derive(Clone, Copy, PartialEq)]
enum Source {
    Database,
    Device,
}

#[derive(Clone, Copy, PartialEq)]
enum RecoveryChoice {
    Recovery,
    NoRecovery,
    Standby,
}

#[derive(Clone, Copy, PartialEq)]
enum BrowseFor {
    Media,
    Move(usize),
    Standby,
}

pub(super) struct RestoreDialog {
    server: usize,
    info: ConnectionInfo,
    meta: Session,
    folders: FolderLoader,
    pub(super) recent: Vec<String>,
    error: Option<String>,
    source: Source,
    history_dbs: Vec<String>,
    history_dbs_rx: Pending,
    existing: Vec<String>,
    existing_rx: Pending,
    source_db: String,
    history_rx: Pending,
    media_rx: Vec<(Vec<String>, Pending)>,
    sets: Vec<BackupSet>,
    include: Vec<bool>,
    filelist_rx: Pending,
    filelist_key: Option<(Vec<String>, i32)>,
    logical: Vec<LogicalFile>,
    target: String,
    moves: Vec<(String, String)>,
    moves_edited: bool,
    replace: bool,
    recovery: RecoveryChoice,
    standby_file: String,
    use_stop_at: bool,
    stop_at: String,
    close_connections: bool,
    keep_replication: bool,
    checksum: bool,
    browser: Option<(BrowseFor, FileBrowser)>,
    run: Option<Runner>,
}

enum Outcome {
    Script(String),
    Close,
    Folder(String),
    Restored,
}

impl RestoreDialog {
    pub(super) fn new(rt: &tokio::runtime::Handle, repaint: Repaint, server: usize, info: ConnectionInfo, db: Option<String>) -> Self {
        let meta = Session::spawn(rt, info.clone(), repaint);
        let mut d = Self {
            server,
            folders: FolderLoader::new(&meta),
            history_dbs_rx: Pending::new(meta.fetch(b::HISTORY_DATABASES_QUERY)),
            existing_rx: Pending::new(meta.fetch("SELECT name FROM sys.databases ORDER BY name")),
            meta,
            info,
            recent: Vec::new(),
            error: None,
            source: if db.is_some() { Source::Database } else { Source::Device },
            history_dbs: Vec::new(),
            existing: Vec::new(),
            source_db: String::new(),
            history_rx: Pending::default(),
            media_rx: Vec::new(),
            sets: Vec::new(),
            include: Vec::new(),
            filelist_rx: Pending::default(),
            filelist_key: None,
            logical: Vec::new(),
            target: db.clone().unwrap_or_default(),
            moves: Vec::new(),
            moves_edited: false,
            replace: false,
            recovery: RecoveryChoice::Recovery,
            standby_file: String::new(),
            use_stop_at: false,
            stop_at: chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
            close_connections: false,
            keep_replication: false,
            checksum: false,
            browser: None,
            run: None,
        };
        if let Some(db) = db {
            d.load_history(&db);
        }
        d
    }

    fn load_history(&mut self, db: &str) {
        self.source_db = db.to_string();
        self.set_sets(Vec::new());
        self.history_rx = Pending::new(self.meta.fetch(b::history_query(db)));
    }

    fn set_sets(&mut self, mut sets: Vec<BackupSet>) {
        sets.sort_by(|a, b| a.finish.cmp(&b.finish).then(a.last_lsn.cmp(&b.last_lsn)));
        let chain = b::restore_chain(&sets);
        self.include = (0..sets.len()).map(|i| chain.contains(&i)).collect();
        self.sets = sets;
    }

    /// Included sets in restore order: the full, then a differential, then logs by LSN.
    fn steps(&self) -> Vec<&BackupSet> {
        let mut v: Vec<&BackupSet> = self.sets.iter().zip(&self.include).filter(|(_, inc)| **inc).map(|(s, _)| s).collect();
        let rank = |k: BackupKind| match k {
            BackupKind::Full => 0,
            BackupKind::Differential => 1,
            _ => 2,
        };
        v.sort_by_key(|s| (rank(s.kind()), s.first_lsn));
        v
    }

    fn recompute_moves(&mut self) {
        let Some(folders) = &self.folders.folders else { return };
        let source = self.steps().first().map(|s| s.database.clone()).unwrap_or_default();
        self.moves = b::default_moves(&self.logical, &source, self.target.trim(), folders);
    }

    fn pump(&mut self, ctx: &egui::Context) {
        if let Some(e) = drain_meta(&self.meta) {
            self.error = Some(e);
        }
        if self.folders.poll() && !self.moves_edited {
            self.recompute_moves();
        }
        let names = |rows: Vec<Vec<Option<String>>>| rows.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect();
        match self.history_dbs_rx.poll() {
            Some(Ok((_, rows))) => self.history_dbs = names(rows),
            Some(Err(e)) => self.error = Some(format!("Backup history (msdb) is not readable: {e}")),
            None => {}
        }
        if let Some(Ok((_, rows))) = self.existing_rx.poll() {
            self.existing = names(rows);
        }
        match self.history_rx.poll() {
            Some(Ok((_, rows))) => {
                let sets = b::parse_history(&self.source_db, &rows);
                self.set_sets(sets);
            }
            Some(Err(e)) => self.error = Some(e),
            None => {}
        }
        let mut arrived = Vec::new();
        self.media_rx.retain_mut(|(files, rx)| match rx.poll() {
            Some(r) => {
                arrived.push((files.clone(), r));
                false
            }
            None => true,
        });
        for (files, r) in arrived {
            match r {
                Ok((cols, rows)) => {
                    let mut sets = std::mem::take(&mut self.sets);
                    sets.extend(b::parse_headers(&cols, &rows, &files));
                    self.set_sets(sets);
                }
                Err(e) => self.error = Some(format!("{}: {e}", files.join(", "))),
            }
        }
        // The file list comes from the full backup being restored.
        let key = self.steps().first().filter(|s| s.kind() == BackupKind::Full).map(|s| (s.files.clone(), s.position));
        if key != self.filelist_key && !self.filelist_rx.busy() {
            self.filelist_key = key.clone();
            self.logical.clear();
            if let Some((files, pos)) = key {
                self.filelist_rx = Pending::new(self.meta.fetch(b::filelistonly_query(&files, pos)));
            }
        }
        match self.filelist_rx.poll() {
            Some(Ok((cols, rows))) => {
                self.logical = b::parse_filelist(&cols, &rows);
                self.moves_edited = false;
                self.recompute_moves();
            }
            Some(Err(e)) => self.error = Some(e),
            None => {}
        }
        if let Some(r) = &mut self.run {
            r.pump(&self.meta, ctx);
        }
    }

    fn options(&self) -> RestoreOptions {
        RestoreOptions {
            target: self.target.trim().to_string(),
            steps: self.steps().into_iter().map(RestoreStep::from).collect(),
            moves: self.moves.clone(),
            replace: self.replace,
            recovery: match self.recovery {
                RecoveryChoice::Recovery => RecoveryState::Recovery,
                RecoveryChoice::NoRecovery => RecoveryState::NoRecovery,
                RecoveryChoice::Standby => RecoveryState::Standby(self.standby_file.clone()),
            },
            stop_at: self.use_stop_at.then(|| self.stop_at.trim().to_string()),
            close_connections: self.close_connections,
            keep_replication: self.keep_replication,
            checksum: self.checksum,
        }
    }

    fn script(&self) -> Result<String, String> {
        if self.use_stop_at && chrono::NaiveDateTime::parse_from_str(self.stop_at.trim(), "%Y-%m-%d %H:%M:%S").is_err() {
            return Err("Enter the point in time as yyyy-mm-dd hh:mm:ss.".into());
        }
        if self.recovery == RecoveryChoice::Standby && self.standby_file.trim().is_empty() {
            return Err("Choose a standby (undo) file.".into());
        }
        b::restore_script(&self.options())
    }

    fn ui(&mut self, ui: &mut egui::Ui, rt: &tokio::runtime::Handle, repaint: &Repaint) -> Option<Outcome> {
        let running = self.run.as_ref().is_some_and(|r| r.running());
        let mut out = None;
        if let Some(e) = &self.error {
            ui.colored_label(Color32::from_rgb(220, 80, 80), e);
        }
        ui.add_enabled_ui(!running, |ui| {
            self.source_ui(ui);
            self.sets_table(ui);
            ui.add_space(4.0);
            egui::Grid::new("restore_target").num_columns(2).spacing([12.0, 6.0]).show(ui, |ui| {
                ui.label("Restore as database:");
                if ui.add(egui::TextEdit::singleline(&mut self.target).desired_width(300.0)).changed() && !self.moves_edited {
                    self.recompute_moves();
                }
                ui.end_row();
            });
            let exists = self.existing.iter().any(|d| d.eq_ignore_ascii_case(self.target.trim()));
            if exists && !self.replace {
                ui.colored_label(Color32::from_rgb(220, 140, 60), "This database exists. Tick REPLACE to overwrite it.");
            }
            self.moves_ui(ui);
            self.options_ui(ui);
        });
        let script = self.script();
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
    pub(super) fn open_restore(&mut self, server: usize, db: Option<String>) {
        let Some(s) = self.servers.get(server) else { return };
        let mut d = RestoreDialog::new(&self.rt, self.repaint.clone(), server, s.info.clone(), db);
        d.recent = self.settings.folders_for(&s.info.server);
        self.restore = Some(d);
    }

    pub(super) fn restore_window(&mut self, ctx: &egui::Context) {
        let Some(d) = &mut self.restore else { return };
        let was_running = d.run.as_ref().is_some_and(|r| r.running());
        d.pump(ctx);
        let mut outcome = None;
        if was_running && d.run.as_ref().is_some_and(|r| !r.running()) {
            outcome = Some(Outcome::Restored);
        }
        if let Some((purpose, fb)) = &mut d.browser {
            let purpose = *purpose;
            match fb.show(ctx, &d.meta) {
                Some(Browse::Picked { dir, paths }) => {
                    match purpose {
                        BrowseFor::Media => {
                            let rx = Pending::new(d.meta.fetch(b::headeronly_query(&paths)));
                            d.media_rx.push((paths, rx));
                        }
                        BrowseFor::Move(i) => {
                            if let (Some(m), Some(p)) = (d.moves.get_mut(i), paths.into_iter().next()) {
                                m.1 = p;
                                d.moves_edited = true;
                            }
                        }
                        BrowseFor::Standby => d.standby_file = paths.into_iter().next().unwrap_or_default(),
                    }
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
        egui::Window::new("Restore Database")
            .id(egui::Id::unique("restore_dialog"))
            .open(&mut open)
            .resizable(true)
            .collapsible(false)
            .default_width(780.0)
            .show(ctx, |ui| {
                ui.add_enabled_ui(!browsing, |ui| {
                    if let Some(o) = d.ui(ui, &rt, &repaint) {
                        outcome = Some(o);
                    }
                });
            });
        let (info, server, server_name) = (d.info.with_database("master"), d.server, d.info.server.clone());
        match outcome {
            Some(Outcome::Script(text)) => self.new_query(Some(info), text),
            Some(Outcome::Close) => self.restore = None,
            Some(Outcome::Folder(dir)) => self.settings.remember_folder(&server_name, &dir),
            Some(Outcome::Restored) => self.refresh_databases(server),
            None if !open => self.restore = None,
            None => {}
        }
    }
    /// Reloads a server's Databases folder (after a restore).
    fn refresh_databases(&mut self, server: usize) {
        let Some(s) = self.servers.get_mut(server) else { return };
        if let Children::Loaded(children) = &mut s.root.children {
            for c in children.iter_mut().filter(|c| c.kind == NodeKind::DatabasesFolder) {
                c.refresh();
            }
        }
    }
}

mod view;