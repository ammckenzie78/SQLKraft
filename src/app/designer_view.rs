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
    /// The row whose drag handle is being dragged.
    dragging: Option<usize>,
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
        // Each row's vertical extent, for working out where a dragged row would land.
        let mut rows: Vec<egui::Rangef> = Vec::with_capacity(count);
        let mut drop_released = false;
        let grid = egui::Grid::new(("designer_grid", self.id)).striped(true).spacing([6.0, 4.0]).show(ui, |ui| {
            for h in headers {
                ui.strong(h);
            }
            ui.end_row();
            for (i, c) in cols.iter_mut().enumerate() {
                let n = i + 1;
                ui.horizontal(|ui| {
                    let handle = drag_handle(ui, self.dragging == Some(i)).on_hover_text("Drag to move this column");
                    let handle = handle.accessible_name(format!("Drag to move column {n}"));
                    rows.push(handle.rect.y_range());
                    if handle.drag_started() {
                        self.dragging = Some(i);
                    }
                    if handle.drag_stopped() {
                        drop_released = true;
                    }
                    // Keyboard and screen-reader users can still reorder from the handle's menu.
                    handle.context_menu(|ui| {
                        if ui.add_enabled(i > 0, egui::Button::new("Move up")).accessible_name(format!("Move column {n} up")).clicked() {
                            action = Some((i, RowAction::Up));
                            ui.close();
                        }
                        if ui.add_enabled(i + 1 < count, egui::Button::new("Move down")).accessible_name(format!("Move column {n} down")).clicked() {
                            action = Some((i, RowAction::Down));
                            ui.close();
                        }
                    });
                    ui.label(if c.source.is_none() && self.design.original.is_some() { format!("{n}*") } else { n.to_string() })
                        .on_hover_text(if c.source.is_none() { "New column" } else { "Existing column" });
                });
                ui.add(egui::TextEdit::singleline(&mut c.name).desired_width(140.0)).accessible_name(format!("Column name {n}"));
                if let Some(expr) = &c.computed {
                    ui.label(RichText::new(format!("AS {expr}")).italics()).on_hover_text("Computed column: it can be renamed or removed here");
                    for _ in 0..9 {
                        ui.label("");
                    }
                } else {
                    column_cells(ui, c, n);
                }
                if remove_button(ui).on_hover_text("Remove this column").accessible_name(format!("Remove column {n}")).clicked() {
                    action = Some((i, RowAction::Remove));
                }
                ui.end_row();
            }
        });

        // While a row is dragged: show where it would land, and move it on release.
        if let Some(from) = self.dragging {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
            let target = ui.ctx().pointer_latest_pos().map(|p| drop_index(&rows, p.y));
            if let Some(to) = target {
                let y = match rows.get(to) {
                    Some(r) => r.min - 2.0,
                    None => rows.last().map_or(grid.response.rect.bottom(), |r| r.max + 2.0),
                };
                let x = grid.response.rect.x_range();
                ui.painter().hline(x, y, egui::Stroke::new(2.0, ui.visuals().selection.stroke.color));
            }
            if drop_released || !ui.input(|i| i.pointer.any_down()) {
                if let Some(to) = target {
                    action = Some((from, RowAction::MoveTo(to)));
                }
                self.dragging = None;
            }
        }

        match action {
            Some((i, RowAction::Up)) => cols.swap(i, i - 1),
            Some((i, RowAction::Down)) => cols.swap(i, i + 1),
            Some((from, RowAction::MoveTo(to))) => move_to(cols, from, to),
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
    /// Drag and drop: move to this insertion point (0 = before the first row, len = last).
    MoveTo(usize),
    Remove,
}

/// The insertion point for a row dropped at height `y`: before the first row whose middle is
/// below `y`, or after the last row.
fn drop_index(rows: &[egui::Rangef], y: f32) -> usize {
    rows.iter().position(|r| y < r.center()).unwrap_or(rows.len())
}

/// Moves item `from` to insertion point `to`, counted before the item is taken out (so
/// `to == from` or `from + 1` leaves it where it is).
fn move_to<T>(v: &mut Vec<T>, from: usize, to: usize) {
    if from >= v.len() {
        return;
    }
    let dest = if to > from { to - 1 } else { to };
    if dest != from {
        let item = v.remove(from);
        v.insert(dest.min(v.len()), item);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drop_index_uses_row_middles() {
        let rows = [egui::Rangef::new(0.0, 20.0), egui::Rangef::new(24.0, 44.0), egui::Rangef::new(48.0, 68.0)];
        assert_eq!(drop_index(&rows, -5.0), 0);
        assert_eq!(drop_index(&rows, 9.0), 0);
        assert_eq!(drop_index(&rows, 11.0), 1);
        assert_eq!(drop_index(&rows, 40.0), 2);
        assert_eq!(drop_index(&rows, 60.0), 3);
        assert_eq!(drop_index(&rows, 500.0), 3);
    }

    #[test]
    fn move_to_insertion_points() {
        let moved = |from, to| {
            let mut v = vec!['a', 'b', 'c', 'd'];
            move_to(&mut v, from, to);
            v.into_iter().collect::<String>()
        };
        assert_eq!(moved(0, 0), "abcd");
        assert_eq!(moved(0, 1), "abcd", "dropping just below itself is no move");
        assert_eq!(moved(0, 2), "bacd");
        assert_eq!(moved(0, 4), "bcda", "to the end");
        assert_eq!(moved(3, 0), "dabc", "to the top");
        assert_eq!(moved(2, 1), "acbd");
        assert_eq!(moved(9, 0), "abcd", "out of range is ignored");
    }
}

/// A grip of six dots that starts a row drag.
fn drag_handle(ui: &mut egui::Ui, active: bool) -> egui::Response {
    let h = ui.spacing().interact_size.y;
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(12.0, h), egui::Sense::drag());
    if resp.hovered() && !active {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
    }
    let color = if active || resp.hovered() { ui.visuals().strong_text_color() } else { ui.visuals().weak_text_color() };
    let c = rect.center();
    for dx in [-2.5, 2.5] {
        for dy in [-4.5, 0.0, 4.5] {
            ui.painter().circle_filled(egui::pos2(c.x + dx, c.y + dy), 1.4, color);
        }
    }
    resp
}

/// A square button with an X painted at its exact centre (the ✖ glyph sat off-centre and came
/// out in the emoji font's colour).
fn remove_button(ui: &mut egui::Ui) -> egui::Response {
    let size = ui.spacing().interact_size.y;
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let v = ui.style().interact(&resp);
        ui.painter().rect(rect, v.corner_radius, v.weak_bg_fill, v.bg_stroke, egui::StrokeKind::Inside);
        let color = if resp.hovered() { Color32::from_rgb(220, 80, 80) } else { v.fg_stroke.color };
        let r = size * 0.18;
        let c = rect.center();
        let stroke = egui::Stroke::new(1.6, color);
        ui.painter().line_segment([c + egui::vec2(-r, -r), c + egui::vec2(r, r)], stroke);
        ui.painter().line_segment([c + egui::vec2(-r, r), c + egui::vec2(r, -r)], stroke);
    }
    resp
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
        self.designers.push(TableDesigner { id, info, load: Load::Ready, design, dragging: None });
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
        self.designers.push(TableDesigner { id, info, load: Load::Loading(rx), design, dragging: None });
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
