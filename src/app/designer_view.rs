//! The table designer window (New Table… and Design): a grid of columns that turns into a
//! CREATE TABLE or ALTER TABLE script in a new query tab. It never changes the table itself.
//!
//! It is a window rather than a tab: query tabs are `QueryTab`s throughout (execute, save,
//! close prompts, completion, the Claude bridge), and the designer has no editor, connection or
//! results of its own. As a window it sits beside the tabs like the Shrink and Back Up dialogs,
//! several can be open at once, and its output is an ordinary query tab.

use eframe::egui::{self, Color32, RichText};
use tokio::sync::oneshot;

use super::SqlKraftApp;
use crate::db::{ConnectionInfo, FetchResult};
use crate::designer::{self, COMMON_TYPES, Column, Design, TypeParams, type_params};
use crate::explorer::NodeKind;

enum Load {
    Loading(oneshot::Receiver<FetchResult>),
    Ready,
    Failed(String),
}

pub(super) struct TableDesigner {
    id: u64,
    /// The server's connection; the script tab opens on `design.db`.
    info: ConnectionInfo,
    load: Load,
    design: Design,
}

enum Outcome {
    Script(String),
    Close,
}

impl TableDesigner {
    fn title(&self) -> String {
        let t = &self.design.table;
        match (&self.load, &self.design.original) {
            (Load::Ready, None) => format!("New Table - {}", self.design.db),
            _ => format!("Design {}.{} - {}", t.schema, t.name, self.design.db),
        }
    }

    fn pump(&mut self) {
        let Load::Loading(rx) = &mut self.load else { return };
        let (schema, name) = (self.design.table.schema.clone(), self.design.table.name.clone());
        self.load = match rx.try_recv() {
            Ok(Ok((_, rows))) => match designer::table_from_rows(&schema, &name, &rows) {
                Ok(t) => {
                    self.design = Design::existing(self.design.db.clone(), t);
                    Load::Ready
                }
                Err(e) => Load::Failed(e),
            },
            Ok(Err(e)) => Load::Failed(format!("{e:#}")),
            Err(oneshot::error::TryRecvError::Empty) => return,
            Err(oneshot::error::TryRecvError::Closed) => Load::Failed("Connection closed".into()),
        };
    }

    fn ui(&mut self, ui: &mut egui::Ui) -> Option<Outcome> {
        match &self.load {
            Load::Loading(_) => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Loading the table definition…");
                });
                return None;
            }
            Load::Failed(e) => {
                ui.colored_label(Color32::from_rgb(220, 80, 80), e);
                return ui.button("Close").clicked().then_some(Outcome::Close);
            }
            Load::Ready => {}
        }
        let is_new = self.design.is_new();
        ui.horizontal(|ui| {
            let t = &mut self.design.table;
            ui.add_enabled_ui(is_new, |ui| {
                ui.label("Schema:");
                ui.add(egui::TextEdit::singleline(&mut t.schema).desired_width(100.0)).accessible_name("Schema");
                ui.label("Table name:");
                ui.add(egui::TextEdit::singleline(&mut t.name).desired_width(220.0)).accessible_name("Table name");
            });
            ui.separator();
            if ui.button("Add Column").clicked() {
                let name = t.next_column_name();
                t.columns.push(Column::new(name));
            }
        });
        if !is_new {
            ui.label(
                RichText::new(
                    "Schema and table name can't be changed here. Identity changes and moving columns \
                     need a table rebuild, which the designer doesn't script yet.",
                )
                .weak()
                .small(),
            );
        }
        ui.add_space(4.0);
        egui::ScrollArea::both().max_height(360.0).auto_shrink([false, true]).show(ui, |ui| self.grid(ui));
        ui.add_space(6.0);

        let script = self.design.script();
        if let Err(problems) = &script {
            for p in problems {
                ui.colored_label(Color32::from_rgb(220, 140, 60), p);
            }
        }
        let mut out = None;
        ui.horizontal(|ui| {
            let what = if is_new { "Open the CREATE TABLE script in a new query window" } else { "Open the ALTER TABLE script in a new query window" };
            if ui.add_enabled(script.is_ok(), egui::Button::new("Generate Script")).on_hover_text(what).clicked()
                && let Ok(s) = &script
            {
                out = Some(Outcome::Script(s.clone()));
            }
            if ui.button("Close").clicked() {
                out = Some(Outcome::Close);
            }
        });
        out
    }

    fn grid(&mut self, ui: &mut egui::Ui) {
        let headers = [
            "", "Column Name", "Data Type", "Length", "Precision", "Scale", "Allow Nulls", "Default", "Identity", "Seed",
            "Increment", "Primary Key", "",
        ];
        let cols = &mut self.design.table.columns;
        let count = cols.len();
        let mut action: Option<(usize, RowAction)> = None;
        egui::Grid::new(("designer_grid", self.id)).striped(true).spacing([6.0, 4.0]).show(ui, |ui| {
            for h in headers {
                ui.strong(h);
            }
            ui.end_row();
            for (i, c) in cols.iter_mut().enumerate() {
                let n = i + 1;
                ui.label(if c.source.is_none() && self.design.original.is_some() { format!("{n}*") } else { n.to_string() })
                    .on_hover_text(if c.source.is_none() { "New column" } else { "Existing column" });
                ui.add(egui::TextEdit::singleline(&mut c.name).desired_width(140.0)).accessible_name(format!("Column name {n}"));
                if let Some(expr) = &c.computed {
                    ui.label(RichText::new(format!("AS {expr}")).italics()).on_hover_text("Computed column: it can be renamed or removed here");
                    for _ in 0..9 {
                        ui.label("");
                    }
                } else {
                    column_cells(ui, c, n);
                }
                ui.horizontal(|ui| {
                    if ui.add_enabled(i > 0, egui::Button::new("↑").small()).accessible_name(format!("Move column {n} up")).clicked() {
                        action = Some((i, RowAction::Up));
                    }
                    if ui.add_enabled(i + 1 < count, egui::Button::new("↓").small()).accessible_name(format!("Move column {n} down")).clicked() {
                        action = Some((i, RowAction::Down));
                    }
                    if ui.add(egui::Button::new("✖").small()).accessible_name(format!("Remove column {n}")).clicked() {
                        action = Some((i, RowAction::Remove));
                    }
                });
                ui.end_row();
            }
        });
        match action {
            Some((i, RowAction::Up)) => cols.swap(i, i - 1),
            Some((i, RowAction::Down)) => cols.swap(i, i + 1),
            Some((i, RowAction::Remove)) => {
                cols.remove(i);
            }
            None => {}
        }
        if cols.is_empty() {
            ui.weak("No columns yet: click Add Column.");
        }
    }
}

enum RowAction {
    Up,
    Down,
    Remove,
}

/// Type through Primary Key for an ordinary (not computed) column.
fn column_cells(ui: &mut egui::Ui, c: &mut Column, n: usize) {
    let before = type_params(&c.data_type);
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(&mut c.data_type).desired_width(110.0)).accessible_name(format!("Data type {n}"));
        ui.menu_button("▾", |ui| {
            egui::ScrollArea::vertical().max_height(300.0).show(ui, |ui| {
                for t in COMMON_TYPES {
                    if ui.button(*t).clicked() {
                        c.data_type = t.to_string();
                        ui.close();
                    }
                }
            });
        });
    });
    let params = type_params(&c.data_type);
    if params != before {
        // Sensible sizes for the new kind of type.
        match params {
            TypeParams::Length if c.length.trim().is_empty() => c.length = "50".into(),
            TypeParams::PrecisionScale => {
                c.precision = "18".into();
                c.scale = "0".into();
            }
            TypeParams::Scale => c.scale = "7".into(),
            _ => {}
        }
    }
    let field = |ui: &mut egui::Ui, value: &mut String, enabled: bool, name: String| {
        ui.add_enabled(enabled, egui::TextEdit::singleline(value).desired_width(48.0)).accessible_name(name);
    };
    field(ui, &mut c.length, params == TypeParams::Length, format!("Length {n}"));
    field(ui, &mut c.precision, params == TypeParams::PrecisionScale, format!("Precision {n}"));
    field(ui, &mut c.scale, matches!(params, TypeParams::PrecisionScale | TypeParams::Scale), format!("Scale {n}"));
    ui.checkbox(&mut c.nullable, "").accessible_name(format!("Allow nulls {n}"));
    ui.add(egui::TextEdit::singleline(&mut c.default).desired_width(120.0).hint_text("none"))
        .accessible_name(format!("Default {n}"));
    if ui.checkbox(&mut c.identity, "").accessible_name(format!("Identity {n}")).changed() && c.identity {
        c.nullable = false;
    }
    field(ui, &mut c.seed, c.identity, format!("Seed {n}"));
    field(ui, &mut c.increment, c.identity, format!("Increment {n}"));
    if ui.checkbox(&mut c.primary_key, "").accessible_name(format!("Primary key {n}")).changed() && c.primary_key {
        // Key columns can't allow nulls, as in SSMS.
        c.nullable = false;
    }
}

impl SqlKraftApp {
    fn next_designer_id(&mut self) -> u64 {
        let id = self.next_tab_id;
        self.next_tab_id += 1;
        id
    }

    /// Tables → New Table….
    pub(super) fn open_new_table(&mut self, server: usize, db: String) {
        let Some(s) = self.servers.get(server) else { return };
        let info = s.info.clone();
        let id = self.next_designer_id();
        let mut design = Design::new_table(db);
        design.table.columns.push(Column::new("Column1"));
        self.designers.push(TableDesigner { id, info, load: Load::Ready, design });
    }

    /// A table → Design: loads its definition on the Object Explorer connection.
    pub(super) fn open_design(&mut self, server: usize, kind: NodeKind) {
        let NodeKind::Table { db, schema, name } = kind else { return };
        let Some(s) = self.servers.get(server) else { return };
        let rx = s.session.fetch(designer::load_query(&db, &schema, &name));
        let info = s.info.clone();
        let id = self.next_designer_id();
        let mut design = Design::new_table(db);
        design.table.schema = schema;
        design.table.name = name;
        self.designers.push(TableDesigner { id, info, load: Load::Loading(rx), design });
    }

    pub(super) fn pump_designers(&mut self) {
        for d in &mut self.designers {
            d.pump();
        }
    }

    pub(super) fn designer_windows(&mut self, ctx: &egui::Context) {
        let mut scripts = Vec::new();
        self.designers.retain_mut(|d| {
            let mut open = true;
            let mut outcome = None;
            egui::Window::new(d.title())
                .id(egui::Id::unique(("table_designer", d.id)))
                .open(&mut open)
                .resizable(true)
                .collapsible(false)
                .default_width(1100.0)
                .show(ctx, |ui| outcome = d.ui(ui));
            match outcome {
                Some(Outcome::Script(text)) => {
                    scripts.push((d.info.with_database(&d.design.db), text));
                    false
                }
                Some(Outcome::Close) => false,
                None => open,
            }
        });
        for (info, text) in scripts {
            self.new_query(Some(info), text);
        }
    }
}
