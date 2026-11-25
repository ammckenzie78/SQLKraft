//! Query tab state, execution, the tab strip, toolbar and status bar.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui::{self, Align, Color32, Layout, RichText, text::LayoutJob};
use tokio::sync::oneshot;

use super::dialogs::{DialogPurpose, Prompt};
use super::editor::editor;
use super::results::{ResultGrid, ResultView, results_pane};
use super::{ConnState, SqlKraftApp, keys};
use crate::db::{ConnectionInfo, Event, FetchResult, Message, Repaint, ServerFacts, Session};
use crate::sql;

#[derive(Clone, Copy, PartialEq)]
pub(super) enum ExecStatus {
    Idle,
    Running,
    Succeeded,
    Errors,
    Cancelled,
}

pub(super) struct QueryTab {
    pub(super) id: u64,
    pub(super) title: String,
    pub(super) path: Option<PathBuf>,
    pub(super) text: String,
    pub(super) saved_text: String,
    pub(super) session: Option<Session>,
    pub(super) facts: Option<ServerFacts>,
    pub(super) conn: ConnState,
    pub(super) current_db: String,
    pub(super) databases: Vec<String>,
    pub(super) databases_rx: Option<oneshot::Receiver<FetchResult>>,
    pub(super) results: Vec<ResultGrid>,
    pub(super) messages: Vec<Message>,
    pub(super) status: ExecStatus,
    pub(super) started: Option<Instant>,
    pub(super) elapsed: Duration,
    pub(super) view: ResultView,
    pub(super) show_results: bool,
    /// Char range of the editor selection as of the last frame.
    pub(super) selection: Option<(usize, usize)>,
    /// (grid index, row, col)
    pub(super) selected_cell: Option<(usize, usize, usize)>,
    pub(super) grid_has_focus: bool,
    pub(super) highlight_cache: Option<(u64, f32, bool, LayoutJob)>,
    pub(super) focus_editor: bool,
}

impl QueryTab {
    pub(super) fn new(id: u64, title: String, text: String) -> Self {
        Self {
            id,
            title,
            path: None,
            saved_text: text.clone(),
            text,
            session: None,
            facts: None,
            conn: ConnState::Disconnected,
            current_db: String::new(),
            databases: Vec::new(),
            databases_rx: None,
            results: Vec::new(),
            messages: Vec::new(),
            status: ExecStatus::Idle,
            started: None,
            elapsed: Duration::ZERO,
            view: ResultView::Results,
            show_results: false,
            selection: None,
            selected_cell: None,
            grid_has_focus: false,
            highlight_cache: None,
            focus_editor: true,
        }
    }

    pub(super) fn dirty(&self) -> bool {
        self.text != self.saved_text
    }

    fn total_rows(&self) -> usize {
        self.results.iter().map(|r| r.rows.len()).sum()
    }

    pub(super) fn connect(&mut self, rt: &tokio::runtime::Handle, info: ConnectionInfo, repaint: Repaint) {
        self.attach(Session::spawn(rt, info, repaint));
    }

    pub(super) fn attach(&mut self, session: Session) {
        self.session = Some(session);
        self.conn = ConnState::Connecting;
        self.facts = None;
        self.databases.clear();
    }

    fn server_label(&self) -> String {
        match &self.session {
            Some(s) => format!("{} ({})", s.info.server, s.info.display_user()),
            None => "not connected".into(),
        }
    }
}

impl SqlKraftApp {
    // -----------------------------------------------------------------------------------------
    // Commands
    // -----------------------------------------------------------------------------------------

    /// Execute the active tab's selection (or whole text). `queue_if_connecting` lets a freshly
    /// opened "Select Top 1000" tab run as soon as its connection is up.
    pub(super) fn execute_tab(&mut self, idx: usize, queue_if_connecting: bool) {
        let Some(tab) = self.tabs.get_mut(idx) else { return };
        if tab.status == ExecStatus::Running {
            return;
        }
        let Some(session) = tab.session.as_mut() else {
            let id = tab.id;
            let info = self.default_connection().unwrap_or_else(|| self.settings.recent.first().cloned().unwrap_or_default());
            self.open_dialog(DialogPurpose::ChangeConnection(id), info);
            return;
        };
        if !queue_if_connecting && matches!(tab.conn, ConnState::Failed(_)) {
            let info = session.info.clone();
            let (rt, rp) = (self.rt.clone(), self.repaint.clone());
            tab.connect(&rt, info, rp);
        }
        let (text, offset) = match tab.selection {
            Some((a, b)) if a != b => {
                let (a, b) = (char_to_byte(&tab.text, a), char_to_byte(&tab.text, b));
                let offset = tab.text[..a].matches('\n').count();
                (tab.text[a..b].to_string(), offset)
            }
            _ => (tab.text.clone(), 0),
        };
        let batches = sql::split_batches(&text, offset);
        if batches.is_empty() {
            return;
        }
        let Some(session) = tab.session.as_mut() else { return };
        session.execute(batches);
        tab.results.clear();
        tab.messages.clear();
        tab.selected_cell = None;
        tab.status = ExecStatus::Running;
        tab.started = Some(Instant::now());
        tab.elapsed = Duration::ZERO;
        tab.show_results = true;
        tab.view = ResultView::Results;
    }

    pub(super) fn cancel_tab(&mut self, idx: usize) {
        if let Some(tab) = self.tabs.get_mut(idx) {
            if let Some(s) = tab.session.as_mut() {
                s.cancel();
            }
        }
    }

    pub(super) fn open_file(&mut self) {
        let Some(path) = self.file_dialogs.open_sql() else {
            return;
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => {
                let info = self.default_connection();
                self.new_query(info, text.clone());
                let tab = &mut self.tabs[self.active];
                tab.title = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                tab.saved_text = text;
                tab.path = Some(path);
            }
            Err(e) => self.toast(format!("Could not open file: {e}")),
        }
    }

    /// Returns true if saved.
    pub(super) fn save_tab(&mut self, idx: usize, save_as: bool) -> bool {
        let Some(tab) = self.tabs.get_mut(idx) else { return false };
        let path = match (&tab.path, save_as) {
            (Some(p), false) => p.clone(),
            _ => match self.file_dialogs.save_sql(&tab.title) {
                Some(p) => p,
                None => return false,
            },
        };
        match std::fs::write(&path, &tab.text) {
            Ok(()) => {
                tab.saved_text = tab.text.clone();
                tab.title = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
                tab.path = Some(path);
                true
            }
            Err(e) => {
                self.toast(format!("Could not save: {e}"));
                false
            }
        }
    }

    pub(super) fn request_close_tab(&mut self, idx: usize) {
        let Some(tab) = self.tabs.get(idx) else { return };
        if tab.dirty() {
            self.prompt = Some(Prompt::CloseTab(tab.id));
        } else {
            self.close_tab(idx);
        }
    }

    pub(super) fn close_tab(&mut self, idx: usize) {
        if idx < self.tabs.len() {
            self.tabs.remove(idx);
            if self.active >= self.tabs.len() && self.active > 0 {
                self.active = self.tabs.len() - 1;
            } else if idx < self.active {
                self.active -= 1;
            }
        }
    }

    pub(super) fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("📝 New Query").on_hover_text(keys("Ctrl+N")).clicked() {
                self.new_query_default();
            }
            if ui.button("📂").on_hover_text(keys("Open file (Ctrl+O)")).clicked() {
                self.open_file();
            }
            if ui.add_enabled(!self.tabs.is_empty(), egui::Button::new("💾")).on_hover_text(keys("Save (Ctrl+S)")).clicked() {
                self.save_tab(self.active, false);
            }
            ui.separator();
            let a = self.active;
            let Some(tab) = self.tabs.get_mut(a) else { return };
            let connected = tab.conn == ConnState::Connected;
            let mut chosen = None;
            ui.add_enabled_ui(connected && tab.status != ExecStatus::Running, |ui| {
                egui::ComboBox::from_id_salt("db_combo")
                    .width(180.0)
                    .selected_text(if tab.current_db.is_empty() { "<database>" } else { &tab.current_db })
                    .show_ui(ui, |ui| {
                        for db in &tab.databases {
                            if ui.selectable_label(*db == tab.current_db, db).clicked() {
                                chosen = Some(db.clone());
                            }
                        }
                    });
            });
            if let (Some(db), Some(s)) = (chosen, &tab.session) {
                s.use_database(&db);
            }
            let running = tab.status == ExecStatus::Running;
            if ui
                .add_enabled(!running, egui::Button::new(RichText::new("▶ Execute").color(Color32::from_rgb(60, 170, 60))))
                .on_hover_text(keys("F5 / Ctrl+E — runs the selection if there is one"))
                .clicked()
            {
                self.execute_tab(a, false);
            }
            if ui
                .add_enabled(running, egui::Button::new(RichText::new("■ Cancel").color(Color32::from_rgb(200, 60, 60))))
                .on_hover_text(keys("Alt+Break / Ctrl+Shift+C"))
                .clicked()
            {
                self.cancel_tab(a);
            }
            if ui.button("🔌").on_hover_text("Change connection").clicked() {
                self.change_connection(a);
            }
        });
    }

    fn tab_strip(&mut self, ui: &mut egui::Ui) {
        let mut close = None;
        egui::ScrollArea::horizontal().id_salt("tabstrip").show(ui, |ui| {
            ui.horizontal(|ui| {
                for (i, tab) in self.tabs.iter().enumerate() {
                    let mut title = tab.title.clone();
                    if let Some(s) = &tab.session {
                        let db = if tab.current_db.is_empty() { "" } else { &tab.current_db };
                        title = format!("{title} - {}.{db} ({})", s.info.server, s.info.display_user());
                    }
                    if tab.dirty() {
                        title.push('*');
                    }
                    if tab.status == ExecStatus::Running {
                        title = format!("⏳ {title}");
                    }
                    let r = ui.selectable_label(i == self.active, title);
                    if r.clicked() {
                        self.active = i;
                    }
                    if r.middle_clicked() {
                        close = Some(i);
                    }
                    if ui.small_button("×").clicked() {
                        close = Some(i);
                    }
                    ui.separator();
                }
            });
        });
        if let Some(i) = close {
            self.request_close_tab(i);
        }
    }

    pub(super) fn query_area(&mut self, ui: &mut egui::Ui) {
        if self.tabs.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.label(RichText::new(keys("Press Ctrl+N or click “New Query” to open a query window.")).weak());
            });
            return;
        }
        self.tab_strip(ui);
        // The tab strip may just have closed the last tab.
        if self.tabs.is_empty() {
            return;
        }
        ui.separator();
        let font_size = self.settings.editor_font_size;
        let dark = ui.visuals().dark_mode;
        let a = self.active.min(self.tabs.len() - 1);
        self.active = a;
        let mut export = false;
        let tab = &mut self.tabs[a];

        // Status bar, SSMS style.
        egui::Panel::bottom(egui::Id::unique(("status", tab.id))).show(ui, |ui| status_bar(ui, tab));

        if tab.show_results {
            egui::Panel::bottom(egui::Id::unique(("results", tab.id)))
                .resizable(true)
                .default_size(ui.available_height() * 0.45)
                .size_range(80.0..=ui.available_height() - 60.0)
                .show(ui, |ui| {
                    // egui stores a panel's size from what its content used, so fill the panel:
                    // otherwise it shrinks to a sliver whenever the pane is nearly empty (as at
                    // the start of every run) and stays that small.
                    ui.set_min_height(ui.available_height());
                    export = results_pane(ui, tab);
                });
        }

        egui::CentralPanel::default_margins().show(ui, |ui| editor(ui, tab, font_size, dark));

        if export {
            self.export_csv(a);
        }
    }
}

pub(super) fn apply_tab_event(tab: &mut QueryTab, ev: Event, max_rows: usize) {
    match ev {
        Event::Connected(f) => {
            tab.current_db = f.database.clone();
            tab.facts = Some(f);
            tab.conn = ConnState::Connected;
            tab.databases.clear();
        }
        Event::ConnectFailed(e) => {
            tab.conn = ConnState::Failed(e.clone());
            tab.messages.push(Message::error(e));
            tab.show_results = true;
            tab.view = ResultView::Messages;
            if tab.status == ExecStatus::Running {
                tab.status = ExecStatus::Errors;
            }
        }
        Event::Disconnected(e) => {
            tab.conn = ConnState::Disconnected;
            tab.messages.push(Message::error(e));
        }
        Event::ExecStarted => {
            tab.status = ExecStatus::Running;
            tab.started = Some(Instant::now());
        }
        Event::ResultSet(columns) => {
            // Column widths are measured from the data when the grid is drawn.
            tab.results.push(ResultGrid::new(columns));
        }
        Event::Rows(rows) => {
            if let Some(grid) = tab.results.last_mut() {
                for r in rows {
                    if grid.rows.len() >= max_rows.max(1) {
                        grid.truncated = true;
                        break;
                    }
                    grid.rows.push(r);
                }
            }
        }
        Event::Message(m) => tab.messages.push(m),
        Event::DatabaseChanged(db) => {
            tab.current_db = db;
            if !tab.databases.contains(&tab.current_db) {
                tab.databases.clear();
            }
        }
        Event::ExecFinished { elapsed, cancelled, had_errors } => {
            tab.elapsed = elapsed;
            tab.started = None;
            tab.status = if cancelled {
                ExecStatus::Cancelled
            } else if had_errors {
                ExecStatus::Errors
            } else {
                ExecStatus::Succeeded
            };
            if (had_errors || tab.results.is_empty()) && !tab.messages.is_empty() {
                tab.view = ResultView::Messages;
            }
        }
    }
}

fn status_bar(ui: &mut egui::Ui, tab: &QueryTab) {
    ui.horizontal(|ui| {
        let (text, color) = match (&tab.conn, tab.status) {
            (ConnState::Connecting, _) => ("Connecting…".to_string(), Color32::from_rgb(200, 160, 40)),
            (ConnState::Failed(_), _) => ("Connection failed.".to_string(), Color32::from_rgb(220, 80, 80)),
            (ConnState::Disconnected, _) if tab.session.is_none() => ("Not connected.".to_string(), Color32::GRAY),
            (ConnState::Disconnected, _) => ("Disconnected.".to_string(), Color32::from_rgb(200, 160, 40)),
            (_, ExecStatus::Running) => ("Executing query…".to_string(), Color32::from_rgb(200, 160, 40)),
            (_, ExecStatus::Succeeded) => ("✔ Query executed successfully.".to_string(), Color32::from_rgb(80, 180, 80)),
            (_, ExecStatus::Errors) => ("⚠ Query completed with errors.".to_string(), Color32::from_rgb(220, 80, 80)),
            (_, ExecStatus::Cancelled) => ("Query was cancelled.".to_string(), Color32::from_rgb(220, 140, 60)),
            (_, ExecStatus::Idle) => ("Connected.".to_string(), Color32::from_rgb(80, 180, 80)),
        };
        ui.colored_label(color, text);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let rows = tab.total_rows();
            ui.label(format!("{rows} row{}", if rows == 1 { "" } else { "s" }));
            ui.separator();
            let elapsed = match tab.started {
                Some(s) => s.elapsed(),
                None => tab.elapsed,
            };
            let secs = elapsed.as_secs();
            ui.monospace(format!("{:02}:{:02}:{:02}", secs / 3600, (secs / 60) % 60, secs % 60));
            ui.separator();
            ui.label(if tab.current_db.is_empty() { "-" } else { &tab.current_db });
            ui.separator();
            if let Some(f) = &tab.facts {
                ui.label(format!("{} ({})", f.login, f.spid));
                ui.separator();
                ui.label(format!("{} ({})", tab.session.as_ref().map(|s| s.info.server.as_str()).unwrap_or(""), f.product_version))
                    .on_hover_text(&f.version);
            } else {
                ui.label(tab.server_label());
            }
        });
    });
    if tab.status == ExecStatus::Running {
        ui.ctx().request_repaint_after(Duration::from_millis(250));
    }
}

fn char_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices().nth(char_idx).map(|(b, _)| b).unwrap_or(s.len())
}
