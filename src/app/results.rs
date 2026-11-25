//! Result grids, the Messages tab, copying and CSV export.

use eframe::egui::{self, Align, Color32, Layout, RichText};
use egui_extras::{Column, TableBuilder};

use super::SqlKraftApp;
use super::query_tab::{ExecStatus, QueryTab};
use crate::db::{Cell, ColumnMeta, MsgKind};

pub(super) struct ResultGrid {
    pub(super) columns: Vec<ColumnMeta>,
    pub(super) rows: Vec<Vec<Cell>>,
    /// Column widths measured from the header and the first [`SIZE_SAMPLE`] rows.
    pub(super) widths: Vec<f32>,
    pub(super) truncated: bool,
    /// How many rows have been measured into `widths`; `None` until the header has been.
    sized_rows: Option<usize>,
    /// `widths` grew since the table last used them, so the table must drop its stored widths.
    widths_changed: bool,
}

/// Rows measured to size columns. Later rows don't change widths (or a user's resizing).
const SIZE_SAMPLE: usize = 500;
/// Auto-sized columns stay within this range; drag a column edge to go wider.
const MIN_COL: f32 = 40.0;
const MAX_COL: f32 = 400.0;
/// Text past this many characters doesn't count towards a column's width.
const MEASURE_CHARS: usize = 120;

impl ResultGrid {
    pub(super) fn new(columns: Vec<ColumnMeta>) -> Self {
        let widths = vec![MIN_COL; columns.len()];
        Self { columns, rows: Vec::new(), widths, truncated: false, sized_rows: None, widths_changed: true }
    }

    /// Measures the header and any not-yet-measured sample rows with the grid's fonts, so each
    /// column fits its data (within [`MIN_COL`]..=[`MAX_COL`]).
    fn size_columns(&mut self, ui: &egui::Ui) {
        let target = self.rows.len().min(SIZE_SAMPLE);
        if self.sized_rows == Some(target) {
            return;
        }
        let body = egui::TextStyle::Body.resolve(ui.style());
        let color = ui.visuals().text_color();
        let pad = ui.spacing().item_spacing.x * 2.0 + 8.0;
        let measure = |text: &str| -> f32 {
            let shown: String = text.chars().take(MEASURE_CHARS).map(|c| if c.is_control() { ' ' } else { c }).collect();
            ui.painter().layout_no_wrap(shown, body.clone(), color).size().x + pad
        };
        let start = match self.sized_rows {
            Some(n) => n,
            None => {
                for (w, c) in self.widths.iter_mut().zip(&self.columns) {
                    let name = if c.name.is_empty() { "(No column name)" } else { &c.name };
                    // Headers are bold; allow a little extra.
                    *w = (measure(name) * 1.08).clamp(MIN_COL, MAX_COL);
                }
                self.widths_changed = true;
                0
            }
        };
        for row in &self.rows[start..target] {
            for (w, cell) in self.widths.iter_mut().zip(row) {
                let need = measure(cell.as_deref().unwrap_or("NULL")).clamp(MIN_COL, MAX_COL);
                if need > *w + 0.5 {
                    *w = need;
                    self.widths_changed = true;
                }
            }
        }
        self.sized_rows = Some(target);
    }
}

#[derive(Clone, Copy, PartialEq)]
pub(super) enum ResultView {
    Results,
    Messages,
}

impl SqlKraftApp {
    pub(super) fn export_csv(&mut self, idx: usize) {
        let Some(tab) = self.tabs.get(idx) else { return };
        let grid_idx = tab.selected_cell.map(|c| c.0).unwrap_or(0);
        let Some(grid) = tab.results.get(grid_idx) else { return };
        let Some(path) = self.file_dialogs.save_csv("results.csv") else {
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
}

/// Returns true when the user asked to export to CSV.
pub(super) fn results_pane(ui: &mut egui::Ui, tab: &mut QueryTab) -> bool {
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
        for (gi, grid) in tab.results.iter_mut().enumerate() {
            grid.size_columns(ui);
            let reset_widths = std::mem::take(&mut grid.widths_changed);
            let grid = &*grid;
            ui.push_id(("grid", gi), |ui| {
                ui.set_height(per_grid - 6.0);
                if grid.truncated {
                    ui.colored_label(
                        Color32::from_rgb(220, 160, 60),
                        format!("Showing first {} rows (limit set in View menu).", grid.rows.len()),
                    );
                }
                // Tables don't scroll sideways on their own; wide result sets need this. egui's
                // default scroll bars float and only show on hover, which hides the fact that a
                // grid has more columns, so the grid's bars are solid and drawn when needed.
                ui.scope(|ui| {
                    ui.spacing_mut().scroll = egui::style::ScrollStyle { bar_width: 10.0, ..egui::style::ScrollStyle::solid() };
                    egui::ScrollArea::horizontal().id_salt(("grid-h", gi)).auto_shrink(false).show(ui, |ui| {
                        result_grid(ui, gi, grid, reset_widths, selected, &mut clicked, &mut copy, per_grid - 28.0);
                    });
                });
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
    reset_widths: bool,
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
    // The table keeps its column widths between frames (and runs). Drop them whenever the
    // measured widths changed, i.e. a new result set or more sample rows arrived.
    if reset_widths {
        tb.reset();
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
