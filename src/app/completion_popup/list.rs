//! Drawing the completion list.

use eframe::egui::{self, Align2, Color32, FontId, Id, Order, Sense, text::CCursor, text_edit::TextEditOutput};

use super::{CompletionState, ROWS, WIDTH, area_id};
use crate::completion::Kind;

fn kind_color(kind: Kind, dark: bool) -> Color32 {
    let (d, l) = match kind {
        Kind::Keyword => ((86, 156, 214), (0, 0, 200)),
        Kind::Function => ((220, 140, 220), (170, 0, 170)),
        Kind::Table | Kind::View => ((78, 201, 176), (0, 120, 110)),
        Kind::Column => ((156, 220, 254), (0, 100, 150)),
        Kind::Procedure => ((220, 180, 100), (150, 90, 0)),
        Kind::Schema | Kind::Database => ((200, 200, 120), (110, 110, 0)),
        Kind::Value => ((214, 157, 133), (163, 21, 21)),
    };
    let (r, g, b) = if dark { d } else { l };
    Color32::from_rgb(r, g, b)
}

fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_string() } else { format!("{}…", s.chars().take(max - 1).collect::<String>()) }
}

/// Draws the list under (or above) the word at the caret; returns a clicked row.
pub(super) fn popup(ctx: &egui::Context, editor: Id, output: &TextEditOutput, st: &mut CompletionState) -> Option<usize> {
    let at = output.galley.pos_from_cursor(CCursor::new(st.word.start)).translate(output.galley_pos.to_vec2());
    if !output.text_clip_rect.intersects(at) {
        return None;
    }
    let row_h = 20.0;
    let height = row_h * st.items.len().min(ROWS) as f32 + 8.0;
    let (pos, pivot) = if at.bottom() + height > ctx.content_rect().bottom() {
        (at.left_top(), Align2::LEFT_BOTTOM)
    } else {
        (at.left_bottom(), Align2::LEFT_TOP)
    };
    let dark = ctx.global_style().visuals.dark_mode;
    let mut clicked = None;
    egui::Area::new(area_id(editor)).order(Order::Foreground).fixed_pos(pos).pivot(pivot).constrain(true).show(
        ctx,
        |ui| {
            egui::Frame::popup(ui.style()).inner_margin(4.0).show(ui, |ui| {
                ui.set_width(WIDTH);
                egui::ScrollArea::vertical().max_height(row_h * ROWS as f32).auto_shrink([false, true]).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 0.0;
                    let visuals = ui.visuals().clone();
                    for (i, item) in st.items.iter().enumerate() {
                        let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), row_h), Sense::CLICK);
                        let selected = i == st.selected;
                        let painter = ui.painter_at(rect);
                        if selected {
                            painter.rect_filled(rect, 3.0, visuals.selection.bg_fill);
                        } else if resp.hovered() {
                            painter.rect_filled(rect, 3.0, visuals.widgets.hovered.weak_bg_fill);
                        }
                        let fg = if selected { visuals.selection.stroke.color } else { visuals.text_color() };
                        let y = rect.center().y;
                        painter.text(
                            egui::pos2(rect.left() + 6.0, y),
                            Align2::LEFT_CENTER,
                            item.kind.label(),
                            FontId::proportional(11.0),
                            kind_color(item.kind, dark),
                        );
                        painter.text(
                            egui::pos2(rect.left() + 76.0, y),
                            Align2::LEFT_CENTER,
                            ellipsize(&item.name, 34),
                            FontId::monospace(13.0),
                            fg,
                        );
                        painter.text(
                            egui::pos2(rect.right() - 6.0, y),
                            Align2::RIGHT_CENTER,
                            ellipsize(&item.detail, 30),
                            FontId::proportional(11.0),
                            visuals.weak_text_color(),
                        );
                        let label = format!("{} {} {}", item.name, item.kind.label(), item.detail);
                        resp.widget_info(|| {
                            egui::WidgetInfo::selected(egui::Role::ListBoxOption, true, selected, label.trim_end())
                        });
                        if selected && st.scroll_to_selected {
                            resp.scroll_to_me(None);
                        }
                        if resp.clicked() {
                            clicked = Some(i);
                        }
                    }
                });
            });
        },
    );
    st.scroll_to_selected = false;
    clicked
}
