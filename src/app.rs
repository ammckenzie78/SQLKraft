//! The main window: Object Explorer on the left, query tabs on the right.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui::{
    self, Align, Color32, FontId, Key, KeyboardShortcut, Layout, Modifiers, RichText, TextFormat,
    text::LayoutJob,
};
use egui_extras::{Column, TableBuilder};
use tokio::sync::oneshot;

use crate::db::{
    AuthKind, Cell, ColumnMeta, ConnectionInfo, Encrypt, Event, FetchResult, Message, MsgKind,
    Repaint, ServerFacts, Session,
};
use crate::explorer::{self, Children, Node, NodeKind};
use crate::icons;
use crate::properties::{self, PropQuery, Property};
use crate::settings::Settings;
use crate::sql::{self, Tok};

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
}

struct ResultGrid {
    columns: Vec<ColumnMeta>,
    rows: Vec<Vec<Cell>>,
    widths: Vec<f32>,
    truncated: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum ResultView {
    Results,
    Messages,
}

#[derive(Clone, Copy, PartialEq)]
enum ExecStatus {
    Idle,
    Running,
    Succeeded,
    Errors,
    Cancelled,
}

struct QueryTab {
    id: u64,
    title: String,
    path: Option<PathBuf>,
    text: String,
    saved_text: String,
    session: Option<Session>,
    facts: Option<ServerFacts>,
    conn: ConnState,
    current_db: String,
    databases: Vec<String>,
    databases_rx: Option<oneshot::Receiver<FetchResult>>,
    results: Vec<ResultGrid>,
    messages: Vec<Message>,
    status: ExecStatus,
    started: Option<Instant>,
    elapsed: Duration,
    view: ResultView,
    show_results: bool,
    /// Char range of the editor selection as of the last frame.
    selection: Option<(usize, usize)>,
    /// (grid index, row, col)
    selected_cell: Option<(usize, usize, usize)>,
    grid_has_focus: bool,
    highlight_cache: Option<(u64, f32, bool, LayoutJob)>,
    focus_editor: bool,
}

impl QueryTab {
    fn new(id: u64, title: String, text: String) -> Self {
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

    fn dirty(&self) -> bool {
        self.text != self.saved_text
    }

    fn total_rows(&self) -> usize {
        self.results.iter().map(|r| r.rows.len()).sum()
    }

    fn connect(&mut self, rt: &tokio::runtime::Handle, info: ConnectionInfo, repaint: Repaint) {
        self.attach(Session::spawn(rt, info, repaint));
    }

    fn attach(&mut self, session: Session) {
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

#[derive(Clone, Copy, PartialEq)]
enum DialogPurpose {
    ObjectExplorer,
    NewQuery,
    ChangeConnection(u64),
}

struct ConnectDialog {
    info: ConnectionInfo,
    purpose: DialogPurpose,
    pending: Option<Session>,
    error: Option<String>,
    focus_password: bool,
}

enum ScriptJob {
    Definition { kind: NodeKind, rx: oneshot::Receiver<FetchResult> },
    Table {
        kind: NodeKind,
        cols: oneshot::Receiver<FetchResult>,
        keys: oneshot::Receiver<FetchResult>,
        cols_done: Option<Vec<Vec<Cell>>>,
        keys_done: Option<Vec<Vec<Cell>>>,
    },
    SelectTop { kind: NodeKind, rx: oneshot::Receiver<FetchResult>, run: bool },
}

struct PendingScript {
    info: ConnectionInfo,
    job: ScriptJob,
}

enum Action {
    NewQuery { server: usize, db: Option<String> },
    SelectTop { server: usize, kind: NodeKind, run: bool },
    ScriptCreate { server: usize, kind: NodeKind },
    ExecuteProc { server: usize, kind: NodeKind },
    Disconnect(usize),
    Reconnect(usize),
    CopyText(String),
    /// Select a node; `show` also opens the Properties pane.
    Select { server: usize, node: u64, kind: NodeKind, name: String, show: bool },
}

/// The node selected in Object Explorer.
#[derive(Clone, PartialEq)]
struct Selection {
    server: usize,
    node: u64,
    kind: NodeKind,
    name: String,
}

enum PropsState {
    Loading(oneshot::Receiver<FetchResult>, PropQuery),
    Loaded(Vec<Property>),
    Failed(String),
}

/// What the Properties pane is showing, and for which selection.
struct PropsView {
    for_selection: Selection,
    connection: Vec<Property>,
    state: Option<PropsState>,
}

enum Prompt {
    CloseTab(u64),
    Quit,
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
    selection: Option<Selection>,
    props: Option<PropsView>,
}

impl SqlKraftApp {
    pub fn new(cc: &eframe::CreationContext<'_>, rt: tokio::runtime::Handle) -> Self {
        let ctx = cc.egui_ctx.clone();
        let repaint: Repaint = std::sync::Arc::new(move || ctx.request_repaint());
        let settings = Settings::load();
        cc.egui_ctx.set_theme(if settings.dark_mode { egui::Theme::Dark } else { egui::Theme::Light });
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
            selection: None,
            props: None,
        };
        app.open_dialog(DialogPurpose::ObjectExplorer, info);
        app
    }

    fn open_dialog(&mut self, purpose: DialogPurpose, info: ConnectionInfo) {
        let focus_password = info.auth.needs_password() && !info.user.is_empty();
        self.dialog = Some(ConnectDialog { info, purpose, pending: None, error: None, focus_password });
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
            for ev in events {
                apply_tab_event(tab, ev, max_rows);
            }
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

        self.pump_scripts();
    }

    fn pump_dialog(&mut self) {
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

    fn add_server(&mut self, info: ConnectionInfo, session: Session, facts: ServerFacts) {
        let mut entry = server_entry(info, session, Some(facts));
        entry.root.ensure_loading(&entry.session);
        self.servers.push(entry);
    }

    fn pump_scripts(&mut self) {
        let mut done: Vec<(ConnectionInfo, String, bool)> = Vec::new();
        let mut errors = Vec::new();
        self.scripts.retain_mut(|p| {
            let finished = match &mut p.job {
                ScriptJob::Definition { kind, rx } => match rx.try_recv() {
                    Ok(Ok((_, rows))) => {
                        match rows.first().and_then(|r| r.first().cloned().flatten()) {
                            Some(def) => done.push((p.info.clone(), explorer::wrap_definition(kind, &def), false)),
                            None => errors.push("No definition available (encrypted, or no permission).".to_string()),
                        }
                        true
                    }
                    Ok(Err(e)) => {
                        errors.push(format!("{e:#}"));
                        true
                    }
                    Err(oneshot::error::TryRecvError::Empty) => false,
                    Err(_) => true,
                },
                ScriptJob::Table { kind, cols, keys, cols_done, keys_done } => {
                    let mut failed = false;
                    if cols_done.is_none() {
                        match cols.try_recv() {
                            Ok(Ok((_, rows))) => *cols_done = Some(rows),
                            Ok(Err(e)) => {
                                errors.push(format!("{e:#}"));
                                failed = true;
                            }
                            Err(oneshot::error::TryRecvError::Empty) => {}
                            Err(_) => failed = true,
                        }
                    }
                    if keys_done.is_none() && !failed {
                        match keys.try_recv() {
                            Ok(Ok((_, rows))) => *keys_done = Some(rows),
                            Ok(Err(e)) => {
                                errors.push(format!("{e:#}"));
                                failed = true;
                            }
                            Err(oneshot::error::TryRecvError::Empty) => {}
                            Err(_) => failed = true,
                        }
                    }
                    if let (Some(c), Some(k)) = (cols_done.as_ref(), keys_done.as_ref()) {
                        done.push((p.info.clone(), explorer::build_create_table(kind, c, k), false));
                        true
                    } else {
                        failed
                    }
                }
                ScriptJob::SelectTop { kind, rx, run } => match rx.try_recv() {
                    Ok(r) => {
                        let cols: Vec<String> = r
                            .map(|(_, rows)| rows.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect())
                            .unwrap_or_default();
                        if let Some(sql) = explorer::select_top_script(kind, &cols) {
                            done.push((p.info.clone(), sql, *run));
                        }
                        true
                    }
                    Err(oneshot::error::TryRecvError::Empty) => false,
                    Err(_) => true,
                },
            };
            !finished
        });
        for e in errors {
            self.toast(format!("Scripting failed: {e}"));
        }
        for (info, text, run) in done {
            self.new_query(Some(info), text);
            if run {
                let idx = self.active;
                self.execute_tab(idx, true);
            }
        }
    }

    // -----------------------------------------------------------------------------------------
    // Commands
    // -----------------------------------------------------------------------------------------

    /// Execute the active tab's selection (or whole text). `queue_if_connecting` lets a freshly
    /// opened "Select Top 1000" tab run as soon as its connection is up.
    fn execute_tab(&mut self, idx: usize, queue_if_connecting: bool) {
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

    fn cancel_tab(&mut self, idx: usize) {
        if let Some(tab) = self.tabs.get_mut(idx) {
            if let Some(s) = tab.session.as_mut() {
                s.cancel();
            }
        }
    }

    fn open_file(&mut self) {
        let Some(path) = rfd::FileDialog::new().add_filter("SQL files", &["sql"]).add_filter("All files", &["*"]).pick_file()
        else {
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
    fn save_tab(&mut self, idx: usize, save_as: bool) -> bool {
        let Some(tab) = self.tabs.get_mut(idx) else { return false };
        let path = match (&tab.path, save_as) {
            (Some(p), false) => p.clone(),
            _ => match rfd::FileDialog::new().add_filter("SQL files", &["sql"]).set_file_name(&tab.title).save_file() {
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

    fn request_close_tab(&mut self, idx: usize) {
        let Some(tab) = self.tabs.get(idx) else { return };
        if tab.dirty() {
            self.prompt = Some(Prompt::CloseTab(tab.id));
        } else {
            self.close_tab(idx);
        }
    }

    fn close_tab(&mut self, idx: usize) {
        if idx < self.tabs.len() {
            self.tabs.remove(idx);
            if self.active >= self.tabs.len() && self.active > 0 {
                self.active = self.tabs.len() - 1;
            } else if idx < self.active {
                self.active -= 1;
            }
        }
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
            Action::ScriptCreate { server, kind } => {
                let Some(s) = self.servers.get(server) else { return };
                let info = s.info.with_database(kind.database().unwrap_or_default());
                let Some(q) = explorer::script_create_query(&kind) else { return };
                let job = match explorer::script_keys_query(&kind) {
                    Some(kq) => ScriptJob::Table {
                        cols: s.session.fetch(q),
                        keys: s.session.fetch(kq),
                        kind,
                        cols_done: None,
                        keys_done: None,
                    },
                    None => ScriptJob::Definition { rx: s.session.fetch(q), kind },
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

    fn toggle_properties(&mut self) {
        self.settings.show_properties = !self.settings.show_properties;
        self.settings.save();
    }

    /// Starts loading properties when the pane is open and the selection has changed.
    fn sync_properties(&mut self) {
        if !self.settings.show_properties {
            return;
        }
        let Some(sel) = &self.selection else {
            self.props = None;
            return;
        };
        if self.props.as_ref().is_some_and(|p| &p.for_selection == sel && p.state.is_some()) {
            return;
        }
        let Some(server) = self.servers.get(sel.server) else { return };
        let connection = match sel.kind {
            NodeKind::Server => properties::connection_properties(&server.info, server.facts.as_ref()),
            _ => Vec::new(),
        };
        let state = match properties::query(&sel.kind) {
            None => PropsState::Loaded(Vec::new()),
            // Leaving the state empty retries on the next frame, once the connection is up.
            Some(_) if server.state == ConnState::Connecting => {
                self.props = Some(PropsView { for_selection: sel.clone(), connection, state: None });
                return;
            }
            Some(_) if server.state != ConnState::Connected => PropsState::Failed("Not connected.".into()),
            Some(q) => PropsState::Loading(server.session.fetch(q.sql.clone()), q),
        };
        self.props = Some(PropsView { for_selection: sel.clone(), connection, state: Some(state) });
    }

    fn pump_properties(&mut self) {
        let Some(view) = &mut self.props else { return };
        let Some(PropsState::Loading(rx, q)) = &mut view.state else { return };
        let next = match rx.try_recv() {
            Ok(Ok((_, rows))) => match q.parse(&rows) {
                Ok(p) => PropsState::Loaded(p),
                Err(e) => PropsState::Failed(e),
            },
            Ok(Err(e)) => PropsState::Failed(format!("{e:#}")),
            Err(oneshot::error::TryRecvError::Empty) => return,
            Err(oneshot::error::TryRecvError::Closed) => PropsState::Failed("Connection closed".into()),
        };
        view.state = Some(next);
    }

    fn properties_pane(&mut self, ui: &mut egui::Ui) {
        let mut close = false;
        let mut refresh = false;
        ui.horizontal(|ui| {
            ui.strong("Properties");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.small_button("✕").on_hover_text("Close (F4)").clicked() {
                    close = true;
                }
                if ui.add_enabled(self.props.is_some(), egui::Button::new("⟳").small()).on_hover_text("Refresh").clicked() {
                    refresh = true;
                }
            });
        });
        ui.separator();
        match (&self.selection, &self.props) {
            (None, _) | (_, None) => {
                ui.weak("Select an item in Object Explorer.");
            }
            (Some(_), Some(view)) => {
                let sel = &view.for_selection;
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(format!("{} {}", sel.kind.icon(), sel.name)).strong());
                    ui.weak(properties::type_name(&sel.kind));
                });
                ui.add_space(4.0);
                egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                    let loaded: &[Property] = match &view.state {
                        Some(PropsState::Loaded(p)) => p,
                        _ => &[],
                    };
                    let all: Vec<&Property> = view.connection.iter().chain(loaded.iter()).collect();
                    property_grid(ui, &all);
                    match &view.state {
                        Some(PropsState::Loading(..)) | None => {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.weak("Loading…");
                            });
                        }
                        Some(PropsState::Failed(e)) => {
                            ui.colored_label(Color32::from_rgb(220, 80, 80), e.as_str());
                        }
                        Some(PropsState::Loaded(p)) if p.is_empty() && view.connection.is_empty() => {
                            ui.weak("No properties for this item.");
                        }
                        _ => {}
                    }
                });
            }
        }
        if refresh {
            if let Some(v) = &mut self.props {
                v.state = None;
            }
        }
        if close {
            self.toggle_properties();
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
                if ui.button("New Query\tCtrl+N").clicked() {
                    self.new_query_default();
                }
                if ui.button("Connect Object Explorer…").clicked() {
                    let info = self.settings.recent.first().cloned().unwrap_or_default();
                    self.open_dialog(DialogPurpose::ObjectExplorer, info);
                }
                ui.separator();
                if ui.button("Open File…\tCtrl+O").clicked() {
                    self.open_file();
                }
                let has_tab = !self.tabs.is_empty();
                if ui.add_enabled(has_tab, egui::Button::new("Save\tCtrl+S")).clicked() {
                    self.save_tab(self.active, false);
                }
                if ui.add_enabled(has_tab, egui::Button::new("Save As…")).clicked() {
                    self.save_tab(self.active, true);
                }
                if ui.add_enabled(has_tab, egui::Button::new("Close Tab\tCtrl+W")).clicked() {
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
                if ui.add_enabled(has_tab, egui::Button::new("Show/Hide Results Pane\tCtrl+R")).clicked() {
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
            });
            ui.menu_button("Help", |ui| {
                if ui.button("Keyboard shortcuts / About").clicked() {
                    self.show_about = true;
                }
            });
        });
    }

    fn change_connection(&mut self, idx: usize) {
        let Some(tab) = self.tabs.get(idx) else { return };
        let info = tab
            .session
            .as_ref()
            .map(|s| s.info.clone())
            .or_else(|| self.settings.recent.first().cloned())
            .unwrap_or_default();
        self.open_dialog(DialogPurpose::ChangeConnection(tab.id), info);
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if ui.button("📝 New Query").on_hover_text("Ctrl+N").clicked() {
                self.new_query_default();
            }
            if ui.button("📂").on_hover_text("Open file (Ctrl+O)").clicked() {
                self.open_file();
            }
            if ui.add_enabled(!self.tabs.is_empty(), egui::Button::new("💾")).on_hover_text("Save (Ctrl+S)").clicked() {
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
                .on_hover_text("F5 / Ctrl+E — runs the selection if there is one")
                .clicked()
            {
                self.execute_tab(a, false);
            }
            if ui
                .add_enabled(running, egui::Button::new(RichText::new("■ Cancel").color(Color32::from_rgb(200, 60, 60))))
                .on_hover_text("Alt+Break / Ctrl+Shift+C")
                .clicked()
            {
                self.cancel_tab(a);
            }
            if ui.button("🔌").on_hover_text("Change connection").clicked() {
                self.change_connection(a);
            }
        });
    }

    fn object_explorer(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.strong("Object Explorer");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.small_button("Connect ▾").clicked() {
                    let info = self.settings.recent.first().cloned().unwrap_or_default();
                    self.open_dialog(DialogPurpose::ObjectExplorer, info);
                }
            });
        });
        ui.separator();
        let mut actions = Vec::new();
        egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
            if self.servers.is_empty() {
                ui.weak("Not connected. Click Connect to add a server.");
            }
            for (si, server) in self.servers.iter_mut().enumerate() {
                let label = match (&server.state, &server.facts) {
                    (ConnState::Connected, Some(f)) => {
                        format!("{} (SQL Server {} - {})", server.info.server, f.product_version, f.login)
                    }
                    (ConnState::Connecting, _) => format!("{} (connecting…)", server.info.server),
                    (ConnState::Failed(_), _) => format!("{} (failed)", server.info.server),
                    _ => format!("{} (disconnected)", server.info.server),
                };
                server.root.label = label;
                server.root.badge = match &server.facts {
                    Some(f) if f.contained_auth => explorer::Badge::Contained,
                    _ => explorer::Badge::None,
                };
                if let ConnState::Failed(e) = &server.state {
                    ui.colored_label(Color32::from_rgb(220, 80, 80), e);
                }
                let selected = self.selection.as_ref().filter(|s| s.server == si).map(|s| s.node);
                let ctx = NodeCtx { session: &server.session, server: si, selected, server_name: &server.info.server };
                show_node(ui, &mut server.root, &ctx, &mut actions);
            }
        });
        for a in actions {
            if let Action::CopyText(t) = &a {
                ui.ctx().copy_text(t.clone());
            }
            self.handle_action(a);
        }
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

    fn query_area(&mut self, ui: &mut egui::Ui) {
        if self.tabs.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.label(RichText::new("Press Ctrl+N or click “New Query” to open a query window.").weak());
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
                    export = results_pane(ui, tab);
                });
        }

        egui::CentralPanel::default_margins().show(ui, |ui| editor(ui, tab, font_size, dark));

        if export {
            self.export_csv(a);
        }
    }

    fn export_csv(&mut self, idx: usize) {
        let Some(tab) = self.tabs.get(idx) else { return };
        let grid_idx = tab.selected_cell.map(|c| c.0).unwrap_or(0);
        let Some(grid) = tab.results.get(grid_idx) else { return };
        let Some(path) = rfd::FileDialog::new().add_filter("CSV", &["csv"]).set_file_name("results.csv").save_file() else {
            return;
        };
        let result = (|| -> anyhow::Result<()> {
            let mut w = csv::Writer::from_path(&path)?;
            w.write_record(grid.columns.iter().map(|c| c.name.as_str()))?;
            for row in &grid.rows {
                w.write_record(row.iter().map(|c| c.as_deref().unwrap_or("NULL")))?;
            }
            w.flush()?;
            Ok(())
        })();
        match result {
            Ok(()) => self.toast(format!("Exported {} rows to {}", grid.rows.len(), path.display())),
            Err(e) => self.toast(format!("Export failed: {e}")),
        }
    }

    fn connect_dialog(&mut self, ctx: &egui::Context) {
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

    fn prompts(&mut self, ctx: &egui::Context) {
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

    fn about(&mut self, ctx: &egui::Context) {
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
                    ui.monospace(k);
                    ui.label(d);
                    ui.end_row();
                }
            });
        });
        self.show_about = open;
    }
}

fn server_entry(info: ConnectionInfo, session: Session, facts: Option<ServerFacts>) -> ServerEntry {
    let state = if facts.is_some() { ConnState::Connected } else { ConnState::Connecting };
    ServerEntry { root: Node::new(info.server.clone(), NodeKind::Server), info, session, facts, state }
}

fn apply_tab_event(tab: &mut QueryTab, ev: Event, max_rows: usize) {
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
            let widths = columns.iter().map(|c| (c.name.chars().count() as f32 * 8.0 + 24.0).clamp(60.0, 300.0)).collect();
            tab.results.push(ResultGrid { columns, rows: Vec::new(), widths, truncated: false });
        }
        Event::Rows(rows) => {
            if let Some(grid) = tab.results.last_mut() {
                let sizing = grid.rows.len() < 100;
                for r in rows {
                    if grid.rows.len() >= max_rows.max(1) {
                        grid.truncated = true;
                        break;
                    }
                    if sizing {
                        for (w, c) in grid.widths.iter_mut().zip(&r) {
                            let len = c.as_deref().map(|s| s.chars().take(60).count()).unwrap_or(4);
                            *w = w.max((len as f32 * 7.5 + 16.0).clamp(60.0, 420.0));
                        }
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

// ---------------------------------------------------------------------------------------------
// Widgets
// ---------------------------------------------------------------------------------------------

/// What `show_node` needs from the server it is drawing.
struct NodeCtx<'a> {
    session: &'a Session,
    server: usize,
    server_name: &'a str,
    /// The selected node's id, if it is in this server's tree.
    selected: Option<u64>,
}

/// The name the Properties pane shows: the object's own name, without the extras in its label.
fn node_name(node: &Node, server_name: &str) -> String {
    match &node.kind {
        NodeKind::Server => server_name.to_string(),
        NodeKind::Column { name, .. } | NodeKind::Index { name, .. } | NodeKind::Login { name } => name.clone(),
        _ => node.label.clone(),
    }
}

fn show_node(ui: &mut egui::Ui, node: &mut Node, ctx: &NodeCtx, actions: &mut Vec<Action>) {
    let icon = node.kind.icon();
    let is_selected = ctx.selected == Some(node.id);
    let (node_id, node_kind, name) = (node.id, node.kind.clone(), node_name(node, ctx.server_name));
    let select = |show: bool| Action::Select {
        server: ctx.server,
        node: node_id,
        kind: node_kind.clone(),
        name: name.clone(),
        show,
    };
    if node.is_leaf() {
        ui.horizontal(|ui| {
            ui.add_space(18.0);
            let r = ui.selectable_label(is_selected, format!("{icon} {}", node.label));
            if r.clicked() || r.secondary_clicked() {
                actions.push(select(false));
            }
            r.context_menu(|ui| {
                if ui.button("Copy").clicked() {
                    let name = node.label.split(" (").next().unwrap_or(&node.label).to_string();
                    actions.push(Action::CopyText(name));
                    ui.close();
                }
                if properties::query(&node.kind).is_some() && ui.button("Properties").clicked() {
                    actions.push(select(true));
                    ui.close();
                }
            });
        });
        return;
    }
    let id = ui.make_persistent_id(("node", node.id));
    let state = egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, false);
    if state.is_open() {
        node.ensure_loading(ctx.session);
    }
    let kind = node.kind.clone();
    let badge = node.badge;
    let painted = icons::is_painted(&kind);
    let label = if painted { node.label.clone() } else { format!("{icon} {}", node.label) };
    let select_action = select(false);
    let show_action = select(true);
    let header = state.show_header(ui, |ui| {
        let icon_r = painted.then(|| icons::show(ui, &kind, badge));
        let r = ui.selectable_label(is_selected, label);
        let r = match icon_r {
            Some(i) => r.union(i),
            None => r,
        };
        if r.double_clicked() {
            match &kind {
                NodeKind::Table { .. } | NodeKind::View { .. } => {
                    actions.push(Action::SelectTop { server: ctx.server, kind: kind.clone(), run: true })
                }
                _ => {}
            }
        }
        r
    });
    let refresh = std::cell::Cell::new(false);
    let (_, header_inner, _) = header.body(|ui| match &mut node.children {
        Children::NotLoaded | Children::Loading(_) => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.weak("Loading…");
            });
        }
        Children::Failed(e) => {
            ui.colored_label(Color32::from_rgb(220, 80, 80), e.as_str());
        }
        Children::Loaded(children) => {
            if children.is_empty() {
                ui.weak("(empty)");
            }
            for c in children {
                show_node(ui, c, ctx, actions);
            }
        }
    });
    let header_label = header_inner.inner;
    if header_label.clicked() || header_label.secondary_clicked() {
        actions.push(select_action);
    }
    header_label.context_menu(|ui| {
        node_menu(ui, &kind, ctx.server, actions, &refresh);
        if properties::query(&kind).is_some() {
            ui.separator();
            if ui.button("Properties").clicked() {
                actions.push(show_action);
                ui.close();
            }
        }
    });
    if header_label.clicked() {
        // Click on the label toggles, like a tree view.
        let mut s = egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, false);
        s.toggle(ui);
        s.store(ui.ctx());
    }
    if refresh.get() {
        node.refresh();
    }
}

/// Category headers, each with a two-column grid of label and value.
fn property_grid(ui: &mut egui::Ui, props: &[&Property]) {
    let mut categories: Vec<&'static str> = Vec::new();
    for p in props {
        // A category with no values at all (password policy on a Windows login) is left out.
        if !categories.contains(&p.category) && props.iter().any(|q| q.category == p.category && !q.value.is_empty()) {
            categories.push(p.category);
        }
    }
    // One label width for every category, so the values line up down the whole pane.
    let font = egui::TextStyle::Body.resolve(ui.style());
    let label_width = props
        .iter()
        .map(|p| ui.painter().layout_no_wrap(p.label.to_string(), font.clone(), Color32::WHITE).size().x)
        .fold(0.0, f32::max);
    for cat in categories {
        egui::CollapsingHeader::new(RichText::new(cat).strong()).id_salt(("props", cat)).default_open(true).show(ui, |ui| {
            egui::Grid::new(("props_grid", cat))
                .num_columns(2)
                .striped(true)
                .spacing([12.0, 4.0])
                .min_col_width(label_width)
                .show(ui, |ui| {
                    for p in props.iter().filter(|p| p.category == cat) {
                        ui.label(p.label);
                        ui.add(egui::Label::new(&p.value).wrap());
                        ui.end_row();
                    }
                });
        });
    }
}

fn node_menu(ui: &mut egui::Ui, kind: &NodeKind, server: usize, actions: &mut Vec<Action>, refresh: &std::cell::Cell<bool>) {
    let mut item = |ui: &mut egui::Ui, label: &str, action: Action| {
        if ui.button(label).clicked() {
            actions.push(action);
            ui.close();
        }
    };
    match kind {
        NodeKind::Server => {
            item(ui, "New Query", Action::NewQuery { server, db: None });
            item(ui, "Reconnect", Action::Reconnect(server));
            item(ui, "Disconnect", Action::Disconnect(server));
        }
        NodeKind::Database { db } => {
            item(ui, "New Query", Action::NewQuery { server, db: Some(db.clone()) });
            item(ui, "Copy name", Action::CopyText(db.clone()));
        }
        NodeKind::Table { .. } | NodeKind::View { .. } => {
            item(ui, "Select Top 1000 Rows", Action::SelectTop { server, kind: kind.clone(), run: true });
            item(ui, "Script as SELECT", Action::SelectTop { server, kind: kind.clone(), run: false });
            item(ui, "Script as CREATE", Action::ScriptCreate { server, kind: kind.clone() });
            if let Some(n) = kind.object_name() {
                item(ui, "Copy name", Action::CopyText(n));
            }
        }
        NodeKind::Procedure { .. } => {
            item(ui, "Execute Stored Procedure…", Action::ExecuteProc { server, kind: kind.clone() });
            item(ui, "Script as CREATE", Action::ScriptCreate { server, kind: kind.clone() });
            if let Some(n) = kind.object_name() {
                item(ui, "Copy name", Action::CopyText(n));
            }
        }
        NodeKind::Function { .. } => {
            item(ui, "Script as CREATE", Action::ScriptCreate { server, kind: kind.clone() });
            if let Some(n) = kind.object_name() {
                item(ui, "Copy name", Action::CopyText(n));
            }
        }
        _ => {}
    }
    if ui.button("Refresh").clicked() {
        refresh.set(true);
        ui.close();
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

fn editor(ui: &mut egui::Ui, tab: &mut QueryTab, font_size: f32, dark: bool) {
    let font = FontId::monospace(font_size);
    let cache = &mut tab.highlight_cache;
    let mut layouter = |ui: &egui::Ui, buf: &dyn egui::TextBuffer, wrap_width: f32| {
        let text = buf.as_str();
        let hash = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            text.hash(&mut h);
            h.finish()
        };
        let mut job = match cache {
            Some((h, s, d, job)) if *h == hash && *s == font_size && *d == dark => job.clone(),
            _ => {
                let job = highlight(text, &font, dark);
                *cache = Some((hash, font_size, dark, job.clone()));
                job
            }
        };
        job.wrap.max_width = wrap_width;
        ui.fonts_mut(|f| f.layout_job(job))
    };

    let line_count = tab.text.split('\n').count().max(1);
    let gutter: String = (1..=line_count).map(|n| format!("{n}\n")).collect();
    let gutter_width = (line_count.to_string().len() as f32 + 1.0) * font_size * 0.62;

    egui::ScrollArea::both().id_salt(("editor", tab.id)).auto_shrink(false).show(ui, |ui| {
        ui.horizontal_top(|ui| {
            ui.add_sized(
                [gutter_width, 0.0],
                egui::Label::new(RichText::new(gutter).font(font.clone()).color(Color32::from_gray(120))).selectable(false),
            );
            let output = egui::TextEdit::multiline(&mut tab.text)
                .id(egui::Id::unique(("editor_text", tab.id)))
                .code_editor()
                .font(font.clone())
                .desired_width(f32::INFINITY)
                .desired_rows(30)
                .lock_focus(true)
                .frame(egui::Frame::NONE)
                .layouter(&mut layouter)
                .show(ui);
            if tab.focus_editor {
                output.response.response.request_focus();
                tab.focus_editor = false;
            }
            if output.response.response.has_focus() || output.response.response.clicked() {
                tab.grid_has_focus = false;
            }
            tab.selection = output.cursor_range.map(|r| {
                let (a, b) = (r.primary.index.0, r.secondary.index.0);
                (a.min(b), a.max(b))
            });
        });
    });
}

fn highlight(text: &str, font: &FontId, dark: bool) -> LayoutJob {
    let (kw, func, string, comment, number, var, op, ident) = if dark {
        (
            Color32::from_rgb(86, 156, 214),
            Color32::from_rgb(220, 140, 220),
            Color32::from_rgb(214, 157, 133),
            Color32::from_rgb(87, 166, 74),
            Color32::from_rgb(181, 206, 168),
            Color32::from_rgb(156, 220, 254),
            Color32::from_gray(160),
            Color32::from_gray(220),
        )
    } else {
        (
            Color32::from_rgb(0, 0, 255),
            Color32::from_rgb(255, 0, 255),
            Color32::from_rgb(163, 21, 21),
            Color32::from_rgb(0, 128, 0),
            Color32::from_rgb(9, 134, 88),
            Color32::from_rgb(0, 112, 140),
            Color32::from_gray(110),
            Color32::from_gray(10),
        )
    };
    let mut job = LayoutJob::default();
    for (tok, range) in sql::tokenize(text) {
        let color = match tok {
            Tok::Keyword => kw,
            Tok::Function => func,
            Tok::String => string,
            Tok::Comment => comment,
            Tok::Number => number,
            Tok::Variable => var,
            Tok::Operator => op,
            Tok::Identifier | Tok::Whitespace => ident,
        };
        job.append(&text[range], 0.0, TextFormat::simple(font.clone(), color));
    }
    job
}

/// Returns true when the user asked to export to CSV.
fn results_pane(ui: &mut egui::Ui, tab: &mut QueryTab) -> bool {
    let mut export = false;
    ui.horizontal(|ui| {
        ui.selectable_value(&mut tab.view, ResultView::Results, format!("▦ Results ({})", tab.results.len()));
        let errs = tab.messages.iter().any(|m| m.kind == MsgKind::Error);
        let label = if errs { RichText::new("💬 Messages ⚠").color(Color32::from_rgb(220, 80, 80)) } else { RichText::new("💬 Messages") };
        ui.selectable_value(&mut tab.view, ResultView::Messages, label);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.add_enabled(!tab.results.is_empty(), egui::Button::new("Export CSV…").small()).clicked() {
                export = true;
            }
        });
    });
    ui.separator();
    match tab.view {
        ResultView::Messages => messages_view(ui, tab),
        ResultView::Results => grids_view(ui, tab),
    }
    export
}

fn messages_view(ui: &mut egui::Ui, tab: &QueryTab) {
    egui::ScrollArea::both().id_salt(("messages", tab.id)).auto_shrink(false).stick_to_bottom(true).show(ui, |ui| {
        for m in &tab.messages {
            let text = RichText::new(&m.text).monospace();
            match m.kind {
                MsgKind::Info => ui.label(text),
                MsgKind::Error => ui.label(text.color(Color32::from_rgb(230, 80, 80))),
            };
        }
        if tab.status == ExecStatus::Running {
            ui.spinner();
        }
    });
}

fn grids_view(ui: &mut egui::Ui, tab: &mut QueryTab) {
    if tab.results.is_empty() {
        if tab.status == ExecStatus::Running {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label("Executing…");
            });
        } else {
            ui.weak("No result sets. See the Messages tab.");
        }
        return;
    }

    // Copy the selected cell with Ctrl+C when the grid (not the editor) has focus.
    if tab.grid_has_focus {
        let copy = ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Copy)));
        if copy {
            if let Some((g, r, c)) = tab.selected_cell {
                if let Some(v) = tab.results.get(g).and_then(|grid| grid.rows.get(r)).and_then(|row| row.get(c)) {
                    ui.ctx().copy_text(v.clone().unwrap_or_else(|| "NULL".into()));
                }
            }
        }
    }

    let n = tab.results.len();
    let avail = ui.available_height();
    let per_grid = if n == 1 { avail } else { (avail / n as f32).max(160.0) };
    let mut clicked: Option<(usize, usize, usize)> = None;
    let mut copy: Option<String> = None;
    let selected = tab.selected_cell;
    let tab_id = tab.id;

    egui::ScrollArea::vertical().id_salt(("grids", tab_id)).auto_shrink(false).show(ui, |ui| {
        for (gi, grid) in tab.results.iter().enumerate() {
            ui.push_id(("grid", gi), |ui| {
                ui.set_height(per_grid - 6.0);
                if grid.truncated {
                    ui.colored_label(
                        Color32::from_rgb(220, 160, 60),
                        format!("Showing first {} rows (limit set in View menu).", grid.rows.len()),
                    );
                }
                result_grid(ui, gi, grid, selected, &mut clicked, &mut copy, per_grid - 10.0);
            });
            if n > 1 {
                ui.separator();
            }
        }
    });
    if let Some(c) = clicked {
        tab.selected_cell = Some(c);
        tab.grid_has_focus = true;
    }
    if let Some(t) = copy {
        ui.ctx().copy_text(t);
    }
}

fn result_grid(
    ui: &mut egui::Ui,
    gi: usize,
    grid: &ResultGrid,
    selected: Option<(usize, usize, usize)>,
    clicked: &mut Option<(usize, usize, usize)>,
    copy: &mut Option<String>,
    height: f32,
) {
    let row_h = ui.text_style_height(&egui::TextStyle::Body) + 4.0;
    let num_w = (grid.rows.len().max(1).to_string().len() as f32) * 8.0 + 14.0;
    let sel_bg = ui.visuals().selection.bg_fill;
    let null_color = ui.visuals().weak_text_color();

    let mut tb = TableBuilder::new(ui)
        .id_salt(("table", gi))
        .striped(true)
        .resizable(true)
        .auto_shrink(false)
        .min_scrolled_height(0.0)
        .max_scroll_height(height)
        .sense(egui::Sense::click())
        .cell_layout(Layout::left_to_right(Align::Center))
        .column(Column::exact(num_w));
    for w in &grid.widths {
        tb = tb.column(Column::initial(*w).at_least(30.0).clip(true));
    }
    tb.header(row_h + 2.0, |mut header| {
        header.col(|_| {});
        for c in &grid.columns {
            header.col(|ui| {
                let name = if c.name.is_empty() { "(No column name)" } else { &c.name };
                ui.strong(name).on_hover_text(&c.type_name);
            });
        }
    })
    .body(|body| {
        body.rows(row_h, grid.rows.len(), |mut row| {
            let ri = row.index();
            let data = &grid.rows[ri];
            row.col(|ui| {
                ui.weak((ri + 1).to_string());
            });
            for (ci, cell) in data.iter().enumerate() {
                let is_sel = selected == Some((gi, ri, ci));
                let numeric = grid.columns.get(ci).is_some_and(|c| c.numeric);
                let (_, resp) = row.col(|ui| {
                    if is_sel {
                        ui.painter().rect_filled(ui.max_rect(), 0.0, sel_bg);
                    }
                    let layout = if numeric { Layout::right_to_left(Align::Center) } else { Layout::left_to_right(Align::Center) };
                    ui.with_layout(layout, |ui| match cell {
                        Some(v) => {
                            let shown: String = v.chars().take(300).map(|c| if c == '\n' || c == '\r' || c == '\t' { ' ' } else { c }).collect();
                            ui.add(egui::Label::new(shown).truncate().selectable(false));
                        }
                        None => {
                            ui.add(egui::Label::new(RichText::new("NULL").italics().color(null_color)).selectable(false));
                        }
                    });
                });
                if resp.clicked() || resp.secondary_clicked() {
                    *clicked = Some((gi, ri, ci));
                }
                resp.context_menu(|ui| {
                    let val = cell.clone().unwrap_or_else(|| "NULL".into());
                    if ui.button("Copy").clicked() {
                        *copy = Some(val.clone());
                        ui.close();
                    }
                    if ui.button("Copy row").clicked() {
                        *copy = Some(row_tsv(data));
                        ui.close();
                    }
                    if ui.button("Copy with headers").clicked() {
                        *copy = Some(format!("{}\n{}", header_tsv(&grid.columns), row_tsv(data)));
                        ui.close();
                    }
                    if ui.button("Copy all with headers").clicked() {
                        let mut s = header_tsv(&grid.columns);
                        for r in &grid.rows {
                            s.push('\n');
                            s.push_str(&row_tsv(r));
                        }
                        *copy = Some(s);
                        ui.close();
                    }
                    if ui.button("Copy column").clicked() {
                        let s: Vec<String> = grid
                            .rows
                            .iter()
                            .map(|r| r.get(ci).cloned().flatten().unwrap_or_else(|| "NULL".into()))
                            .collect();
                        *copy = Some(s.join("\n"));
                        ui.close();
                    }
                });
            }
        });
    });
}

fn header_tsv(cols: &[ColumnMeta]) -> String {
    cols.iter().map(|c| c.name.as_str()).collect::<Vec<_>>().join("\t")
}

fn row_tsv(row: &[Cell]) -> String {
    row.iter().map(|c| c.as_deref().unwrap_or("NULL")).collect::<Vec<_>>().join("\t")
}

fn char_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices().nth(char_idx).map(|(b, _)| b).unwrap_or(s.len())
}

impl eframe::App for SqlKraftApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.pump();
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
