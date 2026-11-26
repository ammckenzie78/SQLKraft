//! The SQL editor widget and its syntax highlighting.

use eframe::egui::{self, Color32, FontId, RichText, TextFormat, text::LayoutJob};

use super::completion_popup::{self, CompletionOpts, MetaCache};
use super::query_tab::QueryTab;
use crate::sql::{self, Tok};

pub(super) fn editor(ui: &mut egui::Ui, tab: &mut QueryTab, font_size: f32, dark: bool, meta: &mut MetaCache, opts: &CompletionOpts) {
    let font = FontId::monospace(font_size);
    let id = egui::Id::unique(("editor_text", tab.id));
    // Before the TextEdit, so the popup's keys never reach it.
    let keys = completion_popup::before_edit(ui.ctx(), id, &tab.completion);
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

    let output = egui::ScrollArea::both()
        .id_salt(("editor", tab.id))
        .auto_shrink(false)
        .show(ui, |ui| {
            ui.horizontal_top(|ui| {
                ui.add_sized(
                    [gutter_width, 0.0],
                    egui::Label::new(RichText::new(gutter).font(font.clone()).color(Color32::from_gray(120)))
                        .selectable(false),
                );
                let output = egui::TextEdit::multiline(&mut tab.text)
                    .id(id)
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
                output
            })
            .inner
        })
        .inner;
    completion_popup::after_edit(ui, id, tab, &output, keys, meta, opts);
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
