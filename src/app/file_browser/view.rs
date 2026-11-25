//! Drawing the server file browser: path bar and breadcrumbs, quick links, the sortable
//! listing and the backup preview pane.

use eframe::egui::{self, Align, Color32, Key, Modifiers, RichText};
use egui_extras::{Column, TableBuilder};

use super::{Browse, BrowseMode, FileBrowser, SortBy};
use crate::db::Session;

impl FileBrowser {
    /// Draws the browser window; returns once the user picks or cancels.
    pub(in crate::app) fn show(&mut self, ctx: &egui::Context, meta: &Session) -> Option<Browse> {
        self.pump(meta);
        let mut out = None;
        let mut open = true;
        egui::Window::new(self.title.clone())
            .id(egui::Id::unique(("server_file_browser", self.title.as_str())))
            .open(&mut open)
            .resizable(true)
            .collapsible(false)
            .default_size([860.0, 560.0])
            .show(ctx, |ui| out = self.contents(ui, meta));
        if !open || ctx.input(|i| i.key_pressed(Key::Escape)) {
            return Some(Browse::Cancelled);
        }
        out
    }

    fn contents(&mut self, ui: &mut egui::Ui, meta: &Session) -> Option<Browse> {
        let mut out = None;
        let mut go: Option<String> = None;
        // Breadcrumbs, then the editable path.
        ui.horizontal_wrapped(|ui| {
            if ui.add_enabled(self.style.parent(&self.dir).is_some(), egui::Button::new("⬆")).on_hover_text("Parent folder (Backspace)").clicked() {
                go = self.style.parent(&self.dir);
            }
            for (i, (label, path)) in self.style.breadcrumbs(&self.dir).into_iter().enumerate() {
                if i > 0 {
                    ui.weak("›");
                }
                if ui.link(label).clicked() {
                    go = Some(path);
                }
            }
            if self.list.busy() {
                ui.spinner();
            }
        });
        let path_id = ui.make_persistent_id("browser_path");
        let r = ui.add(
            egui::TextEdit::singleline(&mut self.path_text)
                .id(path_id)
                .lock_focus(true)
                .desired_width(f32::INFINITY)
                .font(egui::TextStyle::Monospace)
                .hint_text("Type or paste a server path; Enter to go, Tab to complete"),
        );
        if self.focus_path {
            r.request_focus();
            self.focus_path = false;
        }
        if r.has_focus() && ui.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Tab)) {
            self.complete(meta);
        }
        if self.move_cursor_to_end {
            self.move_cursor_to_end = false;
            if let Some(mut st) = egui::TextEdit::load_state(ui.ctx(), path_id) {
                let end = egui::text::CCursor::new(self.path_text.chars().count());
                st.cursor.set_char_range(Some(egui::text::CCursorRange::one(end)));
                st.store(ui.ctx(), path_id);
            }
        }
        if r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter)) {
            self.jump(meta);
        }
        if ui.input(|i| i.modifiers.command && i.key_pressed(Key::L)) {
            self.focus_path = true;
        }
        if let Some(e) = &self.error {
            ui.colored_label(Color32::from_rgb(220, 80, 80), e);
        }

        let vis = self.visible();
        let preview_h = if self.mode == BrowseMode::Open { 150.0 } else { 0.0 };
        let list_h = (ui.available_height() - preview_h - 70.0).max(120.0);
        ui.horizontal_top(|ui| {
            ui.vertical(|ui| {
                ui.set_width(190.0);
                egui::ScrollArea::vertical().id_salt("quick").max_height(list_h).show(ui, |ui| {
                    for (label, path) in &self.quick {
                        if ui.selectable_label(self.dir == *path, label.as_str()).on_hover_text(path).clicked() {
                            go = Some(path.clone());
                        }
                    }
                    ui.separator();
                    for root in &self.roots {
                        if ui.selectable_label(self.dir == *root, format!("💽 {root}")).clicked() {
                            go = Some(root.clone());
                        }
                    }
                });
            });
            ui.separator();
            ui.vertical(|ui| {
                out = self.table(ui, &vis, list_h, meta);
            });
        });
        if let Some(d) = go {
            self.navigate(&d, meta);
        }
        if out.is_none() {
            out = self.keys(ui.ctx(), &vis, meta);
        }
        if self.mode == BrowseMode::Open {
            ui.separator();
            self.preview_ui(ui, preview_h);
        }
        ui.separator();
        ui.horizontal(|ui| {
            ui.label("Filter:");
            ui.add_enabled(!self.all_files, egui::TextEdit::singleline(&mut self.filter).desired_width(160.0));
            ui.checkbox(&mut self.all_files, "All files");
            if self.mode == BrowseMode::Save {
                ui.label("File name:");
                ui.add(egui::TextEdit::singleline(&mut self.file_name).desired_width(220.0));
            } else {
                ui.weak(format!("{} selected", self.selected.len()));
            }
            ui.with_layout(egui::Layout::right_to_left(Align::Center), |ui| {
                if ui.button("Cancel").clicked() {
                    out = Some(Browse::Cancelled);
                }
                let ok = self.ok();
                if ui.add_enabled(ok.is_some(), egui::Button::new("OK")).clicked() {
                    out = ok;
                }
            });
        });
        out
    }

    fn table(&mut self, ui: &mut egui::Ui, vis: &[usize], height: f32, meta: &Session) -> Option<Browse> {
        if let Some(n) = &self.note {
            ui.weak(n);
        }
        let mut clicked = None;
        let mut activated = None;
        let mut sort = None;
        let mut tb = TableBuilder::new(ui)
            .striped(true)
            .sense(egui::Sense::click())
            .min_scrolled_height(height)
            .max_scroll_height(height)
            .column(Column::remainder().at_least(220.0).clip(true))
            .column(Column::auto().at_least(90.0))
            .column(Column::auto().at_least(140.0));
        if let Some(r) = self.scroll_to.take() {
            tb = tb.scroll_to_row(r, None);
        }
        let (by, asc) = self.sort;
        let arrow = |s: SortBy| if by == s { if asc { " ▲" } else { " ▼" } } else { "" };
        tb.header(20.0, |mut h| {
            for (s, label) in [(SortBy::Name, "Name"), (SortBy::Size, "Size"), (SortBy::Modified, "Modified")] {
                h.col(|ui| {
                    if ui.selectable_label(by == s, RichText::new(format!("{label}{}", arrow(s))).strong()).clicked() {
                        sort = Some(s);
                    }
                });
            }
        })
        .body(|body| {
            body.rows(18.0, vis.len(), |mut row| {
                let pos = row.index();
                let e = &self.entries[vis[pos]];
                row.set_selected(self.selected.contains(&e.name));
                row.col(|ui| {
                    ui.label(format!("{} {}", if e.is_dir { "📁" } else { "📄" }, e.name));
                });
                row.col(|ui| {
                    ui.monospace(e.size.filter(|_| !e.is_dir).map(human_size).unwrap_or_default());
                });
                row.col(|ui| {
                    ui.monospace(e.modified.clone().unwrap_or_default());
                });
                let r = row.response();
                if r.double_clicked() {
                    activated = Some(pos);
                } else if r.clicked() {
                    clicked = Some((pos, r.ctx.input(|i| i.modifiers)));
                }
            });
        });
        if let Some(s) = sort {
            self.sort = (s, if by == s { !asc } else { true });
        }
        if let Some((pos, mods)) = clicked {
            self.click(vis, pos, mods);
        }
        activated.and_then(|pos| self.activate(vis, pos, meta))
    }

    fn preview_ui(&self, ui: &mut egui::Ui, height: f32) {
        let p = &self.preview;
        egui::ScrollArea::vertical().id_salt("preview").max_height(height).show(ui, |ui| {
            if p.key.is_empty() {
                ui.weak("Select a backup file to see what's in it. Ctrl/Shift+click selects the files of a striped backup.");
                return;
            }
            if let Some(e) = &p.error {
                ui.colored_label(Color32::from_rgb(220, 80, 80), e);
            }
            if p.header.busy() || p.filelist.busy() {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.weak("Reading the backup header…");
                });
            }
            for s in &p.sets {
                ui.label(
                    RichText::new(format!(
                        "#{}  {}  {}{}  ·  {} → {}  ·  {}",
                        s.position,
                        s.kind().label(),
                        s.database,
                        if s.copy_only { " (copy-only)" } else { "" },
                        s.start,
                        s.finish,
                        human_size(s.size)
                    ))
                    .strong(),
                );
                ui.weak(format!(
                    "Server {} · recovery {} · LSN {} – {} · checkpoint {} · base {}{}",
                    s.server,
                    s.recovery_model,
                    s.first_lsn,
                    s.last_lsn,
                    s.checkpoint_lsn,
                    s.database_backup_lsn,
                    if s.compressed { " · compressed" } else { "" }
                ));
            }
            if !p.files.is_empty() {
                ui.label(RichText::new("Files in the first backup set").small().strong());
                for f in &p.files {
                    ui.monospace(format!("{} [{}] {} {}  {}", f.logical, f.kind, f.filegroup, human_size(f.size), f.physical));
                }
            }
        });
    }
}

pub(in crate::app) fn human_size(bytes: i64) -> String {
    let b = bytes as f64;
    match bytes {
        i64::MIN..1024 => format!("{bytes} B"),
        1024..1_048_576 => format!("{:.1} KB", b / 1024.0),
        1_048_576..1_073_741_824 => format!("{:.1} MB", b / 1_048_576.0),
        _ => format!("{:.2} GB", b / 1_073_741_824.0),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn sizes() {
        assert_eq!(super::human_size(512), "512 B");
        assert_eq!(super::human_size(1536), "1.5 KB");
        assert_eq!(super::human_size(7_560_392_704), "7.04 GB");
    }
}
