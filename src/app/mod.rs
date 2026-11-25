//! The main window: Object Explorer on the left, query tabs on the right.

mod backup_dialog;
mod claude;
mod completion_popup;
mod designer_view;
mod dialogs;
mod editor;
mod explorer_panel;
mod file_dialogs;
mod file_browser;
mod properties_pane;
mod query_tab;
mod restore_dialog;
mod results;
mod runner;
mod scripts;
mod shrink_dialog;

use std::time::{Duration, Instant};

use eframe::egui::{self, Key, KeyboardShortcut, Modifiers};
use tokio::sync::oneshot;

use crate::db::{ConnectionInfo, Event, Repaint, ServerFacts, Session};
use crate::explorer::{self, Node, NodeKind};
use crate::scripting::{self, ScriptMode, ScriptTarget, Syntax};
use crate::settings::Settings;

use dialogs::{ConnectDialog, DialogPurpose, Prompt};
use query_tab::{QueryTab, apply_tab_event};
use properties_pane::{PropsView, Selection};
use scripts::{PendingScript, ScriptJob};

pub use file_dialogs::{FileDialogs, NativeFileDialogs};

const EXECUTE: KeyboardShortcut = KeyboardShortcut::new(Modifiers::NONE, Key::F5);
const EXECUTE_ALT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::E);
const CANCEL: KeyboardShortcut = KeyboardShortcut::new(Modifiers::ALT, Key::Pause);
const CANCEL_ALT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::C);
const NEW_QUERY: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::N);
const OPEN: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::O);
const SAVE: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::S);
const CLOSE_TAB: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::W);
const TOGGLE_RESULTS: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::R);
const ZOOM_IN: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Equals);
const ZOOM_OUT: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND, Key::Minus);
const PROPERTIES: KeyboardShortcut = KeyboardShortcut::new(Modifiers::NONE, Key::F4);

/// A shortcut hint written with "Ctrl+", shown as "Cmd+" on macOS, where `Modifiers::COMMAND`
/// is the Command key.
pub(super) fn keys(text: &str) -> String {
    if cfg!(target_os = "macos") { text.replace("Ctrl+", "Cmd+") } else { text.to_owned() }
}

// ---------------------------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum ConnState {
    Disconnected,
    Connecting,
    Connected,
    Failed(String),
}

struct ServerEntry {
    info: ConnectionInfo,
    session: Session,
    facts: Option<ServerFacts>,
    state: ConnState,
    root: Node,
    /// The user let Claude change data on this connection (never saved).
    claude_writes: bool,
}

enum Action {
    NewQuery { server: usize, db: Option<String> },
    SelectTop { server: usize, kind: NodeKind, run: bool },
    Script { server: usize, kind: NodeKind, mode: ScriptMode },
    ExecuteProc { server: usize, kind: NodeKind },
    Disconnect(usize),
    Reconnect(usize),
    CopyText(String),
    ToggleClaudeWrites(usize),
    /// Tasks → Shrink → Database… (`database`) or Files….
    Shrink { server: usize, db: String, database: bool },
    BackUp { server: usize, db: String },
    /// From a database (its history), or from the Databases folder (`db: None`).
    Restore { server: usize, db: Option<String> },
    /// Tables → New Table…, and a table → Design.
    NewTable { server: usize, db: String },
    Design { server: usize, kind: NodeKind },
    /// Select a node; `show` also opens the Properties pane.
    Select { server: usize, node: u64, kind: NodeKind, name: String, show: bool },
}

pub struct SqlKraftApp {
    rt: tokio::runtime::Handle,
    repaint: Repaint,
    settings: Settings,
    servers: Vec<ServerEntry>,
    tabs: Vec<QueryTab>,
    active: usize,
    next_tab_id: u64,
    query_counter: usize,
    dialog: Option<ConnectDialog>,
    scripts: Vec<PendingScript>,
    prompt: Option<Prompt>,
    allow_quit: bool,
    toast: Option<(String, Instant)>,
    show_about: bool,
    claude: claude::Claude,
    file_dialogs: Box<dyn FileDialogs>,
    shrink: Option<shrink_dialog::ShrinkDialog>,
    backup: Option<backup_dialog::BackupDialog>,
    restore: Option<restore_dialog::RestoreDialog>,
    /// Schema metadata for completion, per (connection, database).
    meta: completion_popup::MetaCache,
    selection: Option<Selection>,
    props: Option<PropsView>,
    designers: Vec<designer_view::TableDesigner>,
}

impl SqlKraftApp {
    pub fn new(cc: &eframe::CreationContext<'_>, rt: tokio::runtime::Handle) -> Self {
        Self::with_settings(&cc.egui_ctx, rt, Settings::load())
    }

    /// The app on `ctx` with the given settings (tests pass settings with a temp `dir`), showing
    /// the connect dialog.
    pub fn with_settings(ctx: &egui::Context, rt: tokio::runtime::Handle, settings: Settings) -> Self {
        let repaint: Repaint = {
            let ctx = ctx.clone();
            std::sync::Arc::new(move || ctx.request_repaint())
        };
        ctx.set_theme(if settings.dark_mode { egui::Theme::Dark } else { egui::Theme::Light });
        let info = settings.recent.first().cloned().unwrap_or_default();
        let mut app = Self {
            rt,
            repaint,
            settings,
            servers: Vec::new(),
            tabs: Vec::new(),
            active: 0,
            next_tab_id: 1,
            query_counter: 0,
            dialog: None,
            scripts: Vec::new(),
            prompt: None,
            allow_quit: false,
            toast: None,
            show_about: false,
            claude: Default::default(),
            file_dialogs: Box::new(NativeFileDialogs),
            shrink: None,
            backup: None,
            restore: None,
            meta: Default::default(),
            selection: None,
            props: None,
            designers: Vec::new(),
        };
        app.sync_claude_bridge();
        app.open_dialog(DialogPurpose::ObjectExplorer, info);
        app
    }

    /// Replaces the native Open / Save dialogs (tests use a stub).
    pub fn set_file_dialogs(&mut self, dialogs: impl FileDialogs + 'static) {
        self.file_dialogs = Box::new(dialogs);
    }

    fn toast(&mut self, msg: impl Into<String>) {
        self.toast = Some((msg.into(), Instant::now()));
    }

    fn new_tab(&mut self, text: String) -> usize {
        self.query_counter += 1;
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        let tab = QueryTab::new(id, format!("SQLQuery{}.sql", self.query_counter), text);
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        self.active
    }

    fn new_query(&mut self, info: Option<ConnectionInfo>, text: String) {
        let idx = self.new_tab(text);
        if let Some(info) = info {
            let (rt, rp) = (self.rt.clone(), self.repaint.clone());
            self.tabs[idx].connect(&rt, info, rp);
        }
    }

    /// New Query: reuse the current connection, or ask for one.
    fn new_query_default(&mut self) {
        match self.default_connection() {
            Some(info) => self.new_query(Some(info), String::new()),
            None => {
                let info = self.settings.recent.first().cloned().unwrap_or_default();
                self.open_dialog(DialogPurpose::NewQuery, info);
            }
        }
    }

    /// The connection a new query should use: the active tab's, else the first server's.
    fn default_connection(&self) -> Option<ConnectionInfo> {
        if let Some(tab) = self.tabs.get(self.active) {
            if let Some(s) = &tab.session {
                return Some(s.info.with_database(&tab.current_db));
            }
        }
        self.servers.iter().find(|s| s.state == ConnState::Connected).map(|s| s.info.clone())
    }

    fn tab_index(&self, id: u64) -> Option<usize> {
        self.tabs.iter().position(|t| t.id == id)
    }

    // -----------------------------------------------------------------------------------------
    // Event pumping
    // -----------------------------------------------------------------------------------------

    fn pump(&mut self) {
        self.pump_dialog();

        for server in &mut self.servers {
            while let Ok(ev) = server.session.events.try_recv() {
                match ev {
                    Event::Connected(f) => {
                        server.facts = Some(f);
                        server.state = ConnState::Connected;
                    }
                    Event::ConnectFailed(e) => server.state = ConnState::Failed(e),
                    Event::Disconnected(_) => server.state = ConnState::Disconnected,
                    _ => {}
                }
            }
            server.root.poll();
        }

        let max_rows = self.settings.max_rows_per_result;
        for tab in &mut self.tabs {
            let Some(session) = &tab.session else { continue };
            let mut events = Vec::new();
            while let Ok(ev) = session.events.try_recv() {
                events.push(ev);
            }
            let finished = events.iter().any(|e| matches!(e, Event::ExecFinished { .. }));
            for ev in events {
                apply_tab_event(tab, ev, max_rows);
            }
            self.meta.track(tab, finished);
            if let Some(rx) = &mut tab.databases_rx {
                match rx.try_recv() {
                    Ok(Ok((_, rows))) => {
                        tab.databases = rows.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect();
                        tab.databases_rx = None;
                    }
                    Ok(Err(_)) | Err(oneshot::error::TryRecvError::Closed) => tab.databases_rx = None,
                    Err(oneshot::error::TryRecvError::Empty) => {}
                }
            }
            if tab.conn == ConnState::Connected && tab.databases.is_empty() && tab.databases_rx.is_none() {
                if let Some(s) = &tab.session {
                    tab.databases_rx = Some(s.fetch("SELECT name FROM sys.databases WHERE state = 0 ORDER BY name"));
                    // Mark as requested so we don't loop if the server returns nothing.
                    tab.databases.push(tab.current_db.clone());
                }
            }
        }

        self.meta.poll();
        self.pump_scripts();
        self.pump_designers();
        self.pump_claude();
    }

    fn add_server(&mut self, info: ConnectionInfo, session: Session, facts: ServerFacts) {
        let mut entry = server_entry(info, session, Some(facts));
        entry.root.ensure_loading(&entry.session);
        self.servers.push(entry);
    }

    fn handle_action(&mut self, action: Action) {
        match action {
            Action::NewQuery { server, db } => {
                if let Some(s) = self.servers.get(server) {
                    let info = match db {
                        Some(db) => s.info.with_database(&db),
                        None => s.info.clone(),
                    };
                    self.new_query(Some(info), String::new());
                }
            }
            Action::SelectTop { server, kind, run } => {
                let Some(s) = self.servers.get(server) else { return };
                let Some(q) = explorer::column_names_query(&kind) else { return };
                let info = s.info.with_database(kind.database().unwrap_or_default());
                let rx = s.session.fetch(q);
                self.scripts.push(PendingScript { info, job: ScriptJob::SelectTop { kind, rx, run } });
            }
            Action::Script { server, kind, mode } => {
                let Some(s) = self.servers.get(server) else { return };
                let info = s.info.with_database(kind.database().unwrap_or_default());
                let syntax = match &s.facts {
                    Some(f) => Syntax::for_target(self.settings.script_target, &f.product_version, f.engine_edition),
                    None => Syntax::for_target(self.settings.script_target, "", 0),
                };
                if mode == ScriptMode::Drop {
                    if let Some(text) = scripting::drop_script(&kind, syntax) {
                        self.new_query(Some(info), text);
                    }
                    return;
                }
                let Some(q) = explorer::script_create_query(&kind) else { return };
                let job = match explorer::script_keys_query(&kind) {
                    Some(kq) => ScriptJob::Table {
                        cols: s.session.fetch(q),
                        keys: s.session.fetch(kq),
                        kind,
                        mode,
                        syntax,
                        cols_done: None,
                        keys_done: None,
                    },
                    None => ScriptJob::Definition { rx: s.session.fetch(q), kind, mode, syntax },
                };
                self.scripts.push(PendingScript { info, job });
            }
            Action::ExecuteProc { server, kind } => {
                let Some(s) = self.servers.get(server) else { return };
                if let Some(text) = explorer::execute_proc_script(&kind) {
                    let info = s.info.with_database(kind.database().unwrap_or_default());
                    self.new_query(Some(info), text);
                }
            }
            Action::Disconnect(i) => {
                if i < self.servers.len() {
                    self.servers.remove(i);
                    // Server indexes after `i` shift down, so drop the selection rather than
                    // point it at the wrong server.
                    self.selection = None;
                }
            }
            Action::Reconnect(i) => {
                if self.selection.as_ref().is_some_and(|s| s.server == i) {
                    self.selection = None;
                }
                if let Some(s) = self.servers.get_mut(i) {
                    s.session = Session::spawn(&self.rt, s.info.clone(), self.repaint.clone());
                    s.state = ConnState::Connecting;
                    s.root = Node::new(s.info.server.clone(), NodeKind::Server);
                }
            }
            Action::ToggleClaudeWrites(i) => {
                if let Some(s) = self.servers.get_mut(i) {
                    s.claude_writes = !s.claude_writes;
                }
            }
            Action::Shrink { server, db, database } => self.open_shrink(server, db, database),
            Action::BackUp { server, db } => self.open_backup(server, db),
            Action::Restore { server, db } => self.open_restore(server, db),
            Action::NewTable { server, db } => self.open_new_table(server, db),
            Action::Design { server, kind } => self.open_design(server, kind),
            Action::CopyText(t) => {
                // handled by the caller with access to ctx
                let _ = t;
            }
            Action::Select { server, node, kind, name, show } => {
                self.selection = Some(Selection { server, node, kind, name });
                if show && !self.settings.show_properties {
                    self.toggle_properties();
                }
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // UI
    // -----------------------------------------------------------------------------------------

    fn shortcuts(&mut self, ctx: &egui::Context) {
        let (exec, cancel, new_q, open, save, close, toggle, zin, zout, props) = ctx.input_mut(|i| {
            (
                i.consume_shortcut(&EXECUTE) | i.consume_shortcut(&EXECUTE_ALT),
                i.consume_shortcut(&CANCEL) | i.consume_shortcut(&CANCEL_ALT),
                i.consume_shortcut(&NEW_QUERY),
                i.consume_shortcut(&OPEN),
                i.consume_shortcut(&SAVE),
                i.consume_shortcut(&CLOSE_TAB),
                i.consume_shortcut(&TOGGLE_RESULTS),
                i.consume_shortcut(&ZOOM_IN),
                i.consume_shortcut(&ZOOM_OUT),
                i.consume_shortcut(&PROPERTIES),
            )
        });
        if self.dialog.is_some() {
            return;
        }
        if props {
            self.toggle_properties();
        }
        let a = self.active;
        if exec {
            self.execute_tab(a, false);
        }
        if cancel {
            self.cancel_tab(a);
        }
        if new_q {
            self.new_query_default();
        }
        if open {
            self.open_file();
        }
        if save {
            self.save_tab(a, false);
        }
        if close {
            self.request_close_tab(a);
        }
        if toggle {
            if let Some(t) = self.tabs.get_mut(a) {
                t.show_results = !t.show_results;
            }
        }
        if zin {
            self.settings.editor_font_size = (self.settings.editor_font_size + 1.0).min(40.0);
        }
        if zout {
            self.settings.editor_font_size = (self.settings.editor_font_size - 1.0).max(8.0);
        }
    }

    fn menu_bar(&mut self, ui: &mut egui::Ui) {
        egui::MenuBar::new().ui(ui, |ui| {
            ui.menu_button("File", |ui| {
                if ui.button(keys("New Query\tCtrl+N")).clicked() {
                    self.new_query_default();
                }
                if ui.button("Connect Object Explorer…").clicked() {
                    let info = self.settings.recent.first().cloned().unwrap_or_default();
                    self.open_dialog(DialogPurpose::ObjectExplorer, info);
                }
                ui.separator();
                if ui.button(keys("Open File…\tCtrl+O")).clicked() {
                    self.open_file();
                }
                let has_tab = !self.tabs.is_empty();
                if ui.add_enabled(has_tab, egui::Button::new(keys("Save\tCtrl+S"))).clicked() {
                    self.save_tab(self.active, false);
                }
                if ui.add_enabled(has_tab, egui::Button::new("Save As…")).clicked() {
                    self.save_tab(self.active, true);
                }
                if ui.add_enabled(has_tab, egui::Button::new(keys("Close Tab\tCtrl+W"))).clicked() {
                    self.request_close_tab(self.active);
                }
                ui.separator();
                if ui.button("Exit").clicked() {
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            ui.menu_button("Query", |ui| {
                let has_tab = !self.tabs.is_empty();
                if ui.add_enabled(has_tab, egui::Button::new("Execute\tF5")).clicked() {
                    self.execute_tab(self.active, false);
                }
                if ui.add_enabled(has_tab, egui::Button::new("Cancel Executing Query\tAlt+Break")).clicked() {
                    self.cancel_tab(self.active);
                }
                if ui.add_enabled(has_tab, egui::Button::new("Change Connection…")).clicked() {
                    self.change_connection(self.active);
                }
                ui.separator();
                if ui.add_enabled(has_tab, egui::Button::new(keys("Show/Hide Results Pane\tCtrl+R"))).clicked() {
                    if let Some(t) = self.tabs.get_mut(self.active) {
                        t.show_results = !t.show_results;
                    }
                }
                if ui
                    .add_enabled(self.tabs.get(self.active).is_some_and(|t| !t.results.is_empty()), egui::Button::new("Export Results to CSV…"))
                    .clicked()
                {
                    self.export_csv(self.active);
                }
            });
            ui.menu_button("View", |ui| {
                let mut shown = self.settings.show_properties;
                if ui.checkbox(&mut shown, "Properties Window\tF4").changed() {
                    self.toggle_properties();
                }
                ui.separator();
                if ui.checkbox(&mut self.settings.dark_mode, "Dark theme").changed() {
                    ui.ctx().set_theme(if self.settings.dark_mode { egui::Theme::Dark } else { egui::Theme::Light });
                    self.settings.save();
                }
                ui.menu_button("Script for server version", |ui| {
                    for t in [ScriptTarget::Connected, ScriptTarget::Modern, ScriptTarget::Legacy] {
                        if ui.radio_value(&mut self.settings.script_target, t, t.label()).changed() {
                            self.settings.save();
                        }
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Editor font size");
                    if ui.add(egui::DragValue::new(&mut self.settings.editor_font_size).range(8.0..=40.0)).changed() {
                        self.settings.save();
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Max rows per result");
                    if ui
                        .add(egui::DragValue::new(&mut self.settings.max_rows_per_result).range(1000..=50_000_000).speed(1000))
                        .changed()
                    {
                        self.settings.save();
                    }
                });
                self.completion_menu(ui);
            });
            self.claude_menu(ui);
            ui.menu_button("Help", |ui| {
                if ui.button("Keyboard shortcuts / About").clicked() {
                    self.show_about = true;
                }
            });
        });
    }
}

fn server_entry(info: ConnectionInfo, session: Session, facts: Option<ServerFacts>) -> ServerEntry {
    let state = if facts.is_some() { ConnState::Connected } else { ConnState::Connecting };
    ServerEntry { root: Node::new(info.server.clone(), NodeKind::Server), info, session, facts, state, claude_writes: false }
}

impl eframe::App for SqlKraftApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.pump();
        self.completion_shortcuts(&ctx);
        self.shortcuts(&ctx);

        if ctx.input(|i| i.viewport().close_requested()) && !self.allow_quit && self.tabs.iter().any(|t| t.dirty()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.prompt = Some(Prompt::Quit);
        }

        egui::Panel::top("menu").show(ui, |ui| {
            self.menu_bar(ui);
            self.toolbar(ui);
        });
        egui::Panel::left("explorer").resizable(true).default_size(300.0).show(ui, |ui| self.object_explorer(ui));
        self.sync_properties();
        self.pump_properties();
        if self.settings.show_properties {
            egui::Panel::right("properties").resizable(true).default_size(300.0).show(ui, |ui| self.properties_pane(ui));
        }
        egui::CentralPanel::default_margins().show(ui, |ui| self.query_area(ui));

        self.connect_dialog(&ctx);
        self.prompts(&ctx);
        self.about(&ctx);
        self.claude_log_window(&ctx);
        self.shrink_window(&ctx);
        self.backup_window(&ctx);
        self.restore_window(&ctx);
        self.designer_windows(&ctx);

        if let Some((msg, at)) = &self.toast {
            if at.elapsed() > Duration::from_secs(6) {
                self.toast = None;
            } else {
                let msg = msg.clone();
                egui::Area::new(egui::Id::unique("toast")).anchor(egui::Align2::RIGHT_BOTTOM, [-16.0, -40.0]).show(&ctx, |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.label(msg);
                    });
                });
                ctx.request_repaint_after(Duration::from_millis(500));
            }
        }
    }

    fn on_exit(&mut self) {
        self.settings.save();
    }
}
