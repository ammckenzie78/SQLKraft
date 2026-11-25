//! Headless UI tests: the real `SqlKraftApp` driven through egui_kittest, with no window or GPU.
//! Widgets are found by their AccessKit label or role, like a screen reader would.
//!
//! Each test uses settings in its own temp directory (never the user's), no Claude Code bridge,
//! and stubbed Open / Save dialogs.

mod common;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use egui_kittest::kittest::{By, Queryable};
use egui_kittest::{Harness, kittest::NodeT};
use eframe::egui::{self, accesskit::Role};
use sqlkraft::app::{FileDialogs, SqlKraftApp};
use sqlkraft::settings::Settings;

/// A temp directory removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!("sqlkraft-ui-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Open / Save dialogs that answer with preset paths and record what was asked.
#[derive(Clone, Default)]
struct StubDialogs {
    open: Arc<Mutex<Option<PathBuf>>>,
    save: Arc<Mutex<Option<PathBuf>>>,
    asked: Arc<Mutex<Vec<String>>>,
}

impl FileDialogs for StubDialogs {
    fn open_sql(&self) -> Option<PathBuf> {
        self.asked.lock().unwrap().push("open_sql".into());
        self.open.lock().unwrap().clone()
    }

    fn save_sql(&self, file_name: &str) -> Option<PathBuf> {
        self.asked.lock().unwrap().push(format!("save_sql {file_name}"));
        self.save.lock().unwrap().clone()
    }

    fn save_csv(&self, file_name: &str) -> Option<PathBuf> {
        self.asked.lock().unwrap().push(format!("save_csv {file_name}"));
        self.save.lock().unwrap().clone()
    }
}

struct Ui {
    harness: Harness<'static, SqlKraftApp>,
    dialogs: StubDialogs,
    dir: TempDir,
    _rt: tokio::runtime::Runtime,
}

impl Ui {
    fn start(settings: Settings) -> Self {
        let dir = TempDir::new();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let handle = rt.handle().clone();
        let dialogs = StubDialogs::default();
        let stub = dialogs.clone();
        let settings = Settings { dir: Some(dir.0.clone()), claude_bridge: false, ..settings };
        let harness = Harness::builder()
            .with_size(egui::vec2(1400.0, 900.0))
            // Not every input in the app has an accessible name yet (e.g. the editor).
            .with_accessibility_check(false)
            // The test context has only egui's bundled fonts, without emoji (📝, 💾, ...).
            .allow_missing_glyphs()
            .build_eframe(move |cc| {
                let mut app = SqlKraftApp::with_settings(&cc.egui_ctx, handle, settings);
                app.set_file_dialogs(stub);
                app
            });
        Self { harness, dialogs, dir, _rt: rt }
    }

    fn has(&self, label: &str) -> bool {
        self.harness.query_by_label(label).is_some()
    }

    fn click(&mut self, label: &str) {
        self.harness.get_by_label(label).click();
        self.harness.run_ok();
    }

    /// Steps frames (in real time) until `done` holds, for work on background threads.
    fn wait_until(&mut self, what: &str, timeout: Duration, done: impl Fn(&Self) -> bool) {
        let start = Instant::now();
        while !done(self) {
            assert!(start.elapsed() < timeout, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(50));
            self.harness.step();
        }
        self.harness.run_ok();
    }

    fn editor_text(&self) -> String {
        self.harness.get_by_role(Role::MultilineTextInput).value().unwrap_or_default()
    }

    /// Writes `text` to a `.sql` file and opens it through File > Open (the stubbed dialog).
    fn open_file(&mut self, name: &str, text: &str) -> PathBuf {
        let path = self.dir.0.join(name);
        std::fs::write(&path, text).unwrap();
        *self.dialogs.open.lock().unwrap() = Some(path.clone());
        self.click("File");
        self.click(&keys("Open File…\tCtrl+O"));
        path
    }
}

/// A node's visible text: AccessKit keeps a `Role::Label`'s text in its value.
fn text_of(n: &egui_kittest::kittest::AccessKitNode<'_>) -> Option<String> {
    if n.role() == Role::Label { n.value() } else { n.label() }
}

/// A label that is `text`, or ends with ` text` (tree nodes carry an icon prefix).
fn tree_label(text: &str) -> By<'_> {
    By::new().predicate(move |n| text_of(n).is_some_and(|l| l == text || l.ends_with(&format!(" {text}"))))
}

#[test]
fn starts_with_no_servers_and_the_connect_dialog() {
    let mut ui = Ui::start(Settings::default());
    assert!(ui.has("Connect to Server"));
    assert!(ui.has("Not connected. Click Connect to add a server."));
    assert!(ui.has(&keys("Press Ctrl+N or click “New Query” to open a query window.")));

    ui.click("Cancel");
    assert!(!ui.has("Connect to Server"));

    // With nothing connected, New Query asks for a connection first (like SSMS).
    ui.click("📝 New Query");
    assert!(ui.has("Connect to Server"));
    ui.click("Cancel");
    assert!(!ui.has("Connect to Server"));
    assert!(ui.has(&keys("Press Ctrl+N or click “New Query” to open a query window.")));
}

#[test]
fn settings_stay_in_the_given_directory() {
    let mut ui = Ui::start(Settings::default());
    ui.click("Cancel");
    ui.click("View");
    ui.click("Dark theme");
    let saved = std::fs::read_to_string(ui.dir.0.join("settings.json")).expect("settings saved in the temp dir");
    assert!(saved.contains("\"dark_mode\": false"), "{saved}");
}

#[test]
fn open_file_typing_marks_the_tab_dirty() {
    let mut ui = Ui::start(Settings::default());
    ui.click("Cancel");
    ui.open_file("hello.sql", "SELECT 1");
    assert_eq!(ui.dialogs.asked.lock().unwrap().as_slice(), ["open_sql"]);
    assert!(ui.has("hello.sql"), "the tab is named after the file");
    assert_eq!(ui.editor_text(), "SELECT 1");

    let editor = ui.harness.get_by_role(Role::MultilineTextInput);
    editor.focus();
    ui.harness.run_ok();
    ui.harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::End);
    ui.harness.get_by_role(Role::MultilineTextInput).type_text(" + 1");
    ui.harness.run_ok();

    assert_eq!(ui.editor_text(), "SELECT 1 + 1");
    assert!(ui.has("hello.sql*"), "a changed tab is marked with *");
}

#[test]
fn closing_a_dirty_tab_asks_to_save() {
    let mut ui = Ui::start(Settings::default());
    ui.click("Cancel");
    let path = ui.open_file("dirty.sql", "");
    ui.harness.get_by_role(Role::MultilineTextInput).focus();
    ui.harness.run_ok();
    ui.harness.get_by_role(Role::MultilineTextInput).type_text("SELECT 2");
    ui.harness.run_ok();
    assert!(ui.has("dirty.sql*"));

    // Cancel keeps the tab and its text.
    ui.click("×");
    assert!(ui.has("Save changes to dirty.sql?"));
    ui.click("Cancel");
    assert!(!ui.has("Save changes to dirty.sql?"));
    assert!(ui.has("dirty.sql*"));

    // Save writes the file (it already has a path, so no dialog) and closes the tab.
    ui.click("×");
    ui.click("Save");
    assert!(!ui.has("Save changes to dirty.sql?"));
    assert!(ui.harness.query_by_role(Role::MultilineTextInput).is_none(), "tab closed");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "SELECT 2");
    assert_eq!(ui.dialogs.asked.lock().unwrap().as_slice(), ["open_sql"]);
}

#[test]
fn dont_save_closes_without_writing_and_save_as_uses_the_dialog() {
    let mut ui = Ui::start(Settings::default());
    ui.click("Cancel");
    let path = ui.open_file("keep.sql", "SELECT 3");
    ui.harness.get_by_role(Role::MultilineTextInput).focus();
    ui.harness.run_ok();
    ui.harness.get_by_role(Role::MultilineTextInput).type_text("-- ");
    ui.harness.run_ok();

    // Save As asks the (stubbed) dialog for a new path.
    let copy = ui.dir.0.join("copy.sql");
    *ui.dialogs.save.lock().unwrap() = Some(copy.clone());
    ui.click("File");
    ui.click("Save As…");
    assert!(ui.has("copy.sql"), "the tab is renamed to the saved file");
    let saved = std::fs::read_to_string(&copy).unwrap();
    assert!(saved.contains("SELECT 3") && saved.len() > "SELECT 3".len(), "{saved:?}");
    assert_eq!(ui.dialogs.asked.lock().unwrap().last().unwrap(), "save_sql keep.sql");

    ui.harness.get_by_role(Role::MultilineTextInput).focus();
    ui.harness.run_ok();
    ui.harness.get_by_role(Role::MultilineTextInput).type_text("x");
    ui.harness.run_ok();
    ui.click("×");
    ui.click("Don't Save");
    assert!(ui.harness.query_by_role(Role::MultilineTextInput).is_none(), "tab closed");
    assert_eq!(std::fs::read_to_string(&copy).unwrap(), saved, "Don't Save leaves the file alone");
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "SELECT 3");
}

/// Connects through the dialog, opens a query, then expands the fixture database down to a
/// stored procedure and scripts it with its context menu.
#[test]
fn live_connect_and_script_procedure_as_create() {
    let info = live_or_skip!();
    let fixture = common::Fixture::new(info.clone());
    let mut ui = Ui::start(Settings::default());

    // Fill in the connect dialog. The fields have no accessible names, so find them by what
    // they hold (the defaults are `localhost` / `sa`) and by role.
    let field = |value: &'static str| By::new().role(Role::TextInput).value(value);
    ui.harness.get(field("localhost")).set_text_value_accesskit(&info.server);
    ui.harness.run_ok();
    ui.harness.get(field("sa")).set_text_value_accesskit(&info.user);
    ui.harness.run_ok();
    ui.harness.get_by_role(Role::PasswordInput).set_text_value_accesskit(&info.password);
    ui.harness.run_ok();
    ui.harness.get_by_label("Connect").click();
    ui.harness.step();

    let connected = format!("{} (SQL Server", info.server);
    ui.wait_until("the server to connect", Duration::from_secs(30), |ui| {
        ui.harness.query_by_label_contains(&connected).is_some()
    });
    assert!(!ui.has("Connect to Server"));

    // New Query now reuses the connection.
    ui.click("📝 New Query");
    assert!(ui.harness.query_by_label_contains("SQLQuery1.sql").is_some());

    // Expand Server > Databases > <fixture db> > Programmability > Stored Procedures.
    let server_label = text_of(&ui.harness.get_by_label_contains(&connected).accesskit_node()).unwrap();
    for step in [server_label.as_str(), "Databases", fixture.db.as_str(), "Programmability", "Stored Procedures"] {
        let what = format!("tree node {step}");
        ui.wait_until(&what, Duration::from_secs(30), |ui| ui.harness.query(tree_label(step)).is_some());
        let node = ui.harness.get(tree_label(step));
        node.scroll_to_me();
        ui.harness.run_ok();
        ui.harness.get(tree_label(step)).click();
        ui.harness.run_ok();
    }

    ui.wait_until("dbo.GetCustomer", Duration::from_secs(30), |ui| ui.harness.query(tree_label("dbo.GetCustomer")).is_some());
    ui.harness.get(tree_label("dbo.GetCustomer")).scroll_to_me();
    ui.harness.run_ok();
    ui.harness.get(tree_label("dbo.GetCustomer")).click_secondary();
    ui.harness.run_ok();
    ui.click("Script as CREATE");

    ui.wait_until("the script tab", Duration::from_secs(30), |ui| {
        ui.harness.query_by_label_contains("SQLQuery2.sql").is_some() && ui.editor_text().contains("CREATE")
    });
    let text = ui.editor_text();
    // `CREATE OR ALTER PROCEDURE` on SQL Server 2016 SP1+, `CREATE PROCEDURE` before.
    assert!(text.contains("CREATE") && text.contains("PROCEDURE dbo.GetCustomer"), "{text}");
    assert!(text.contains(&format!("USE [{}]", fixture.db)), "{text}");
}

/// The results pane keeps its height while it has little to show (e.g. the moment a query
/// starts and the old results are cleared). It used to shrink to its content and stay small.
#[test]
fn results_pane_keeps_its_height_when_empty() {
    let mut ui = Ui::start(Settings::default());
    ui.click("Cancel");
    ui.open_file("q.sql", "SELECT 1");
    // Show the (empty) results pane, let a few frames settle, then hide and show it again.
    for _ in 0..2 {
        ui.harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::R);
        for _ in 0..5 {
            ui.harness.step();
        }
    }
    ui.harness.key_press_modifiers(egui::Modifiers::COMMAND, egui::Key::R);
    for _ in 0..5 {
        ui.harness.step();
    }
    let top = ui.harness.get_by_label_contains("Results (").rect().min.y;
    // The window is 900 px high; the pane starts at roughly 45% of the editor area, not a sliver.
    assert!(top < 650.0, "results pane collapsed: its top edge is at y = {top}");
}

/// Shortcut labels say Cmd+ on macOS, like the app's own `keys` helper.
fn keys(text: &str) -> String {
    if cfg!(target_os = "macos") { text.replace("Ctrl+", "Cmd+") } else { text.to_owned() }
}
