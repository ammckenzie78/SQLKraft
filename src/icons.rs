//! Painted Object Explorer icons: server and database glyphs with SSMS-style corner badges.
//!
//! Font glyphs can't carry an overlay, so these are drawn with the painter on a 16×16 grid
//! and scaled to the row height.

use eframe::egui::{self, Color32, Pos2, Rect, Response, Sense, Shape, Stroke, Ui, Vec2, pos2, vec2};

use crate::explorer::{Badge, NodeKind};

const OFFLINE: Color32 = Color32::from_rgb(0xE2, 0x4B, 0x4A);
const RESTORING: Color32 = Color32::from_rgb(0x37, 0x8A, 0xDD);
const WARNING: Color32 = Color32::from_rgb(0xD9, 0x8E, 0x1A);
const CONTAINED: Color32 = Color32::from_rgb(0x1D, 0x9E, 0x75);

/// Whether `kind` gets a painted icon (the rest keep their text glyph).
pub fn is_painted(kind: &NodeKind) -> bool {
    matches!(kind, NodeKind::Server | NodeKind::Database { .. })
}

/// Hover text explaining a badge on this kind of node.
pub fn badge_tooltip(kind: &NodeKind, badge: Badge) -> Option<&'static str> {
    Some(match (kind, badge) {
        (_, Badge::None) => return None,
        (NodeKind::Server, Badge::Contained) => "Contained database authentication is enabled",
        (_, Badge::Contained) => "Partially contained database",
        (_, Badge::Offline) => "Offline",
        (_, Badge::Restoring) => "Restoring",
        (_, Badge::Warning) => "Not available (suspect, emergency or recovering)",
    })
}

/// Allocates and paints a row icon. Click-sensing so it toggles/opens menus like the label.
pub fn show(ui: &mut Ui, kind: &NodeKind, badge: Badge) -> Response {
    let side = ui.text_style_height(&egui::TextStyle::Body).max(14.0) + 2.0;
    let (rect, response) = ui.allocate_exact_size(Vec2::splat(side), Sense::click());
    if ui.is_rect_visible(rect) {
        paint(ui, rect, kind, badge);
    }
    match badge_tooltip(kind, badge) {
        Some(tip) => response.on_hover_text(tip),
        None => response,
    }
}

fn paint(ui: &Ui, rect: Rect, kind: &NodeKind, badge: Badge) {
    let painter = ui.painter_at(rect.expand(1.0));
    let u = rect.width() / 16.0;
    let p = |x: f32, y: f32| rect.min + vec2(x * u, y * u);
    let ink = ui.visuals().text_color();
    let stroke = Stroke::new((1.2 * u).max(1.0), ink);

    match kind {
        NodeKind::Server => {
            let body = Rect::from_min_max(p(3.0, 1.5), p(13.0, 14.5));
            painter.rect_stroke(body, 1.5 * u, stroke, egui::StrokeKind::Middle);
            painter.line_segment([p(5.0, 5.0), p(11.0, 5.0)], stroke);
            painter.line_segment([p(5.0, 8.0), p(11.0, 8.0)], stroke);
            painter.circle_filled(p(10.5, 11.5), 0.9 * u, ink);
        }
        _ => {
            // Cylinder: top ellipse, sides, bottom rim, and a faint middle band.
            let (cx, rx, ry) = (8.0, 5.0, 2.0);
            painter.add(Shape::closed_line(ellipse(p(cx, 4.0), rx * u, ry * u, 0.0, 360.0), stroke));
            painter.line_segment([p(3.0, 4.0), p(3.0, 12.0)], stroke);
            painter.line_segment([p(13.0, 4.0), p(13.0, 12.0)], stroke);
            painter.add(Shape::line(ellipse(p(cx, 12.0), rx * u, ry * u, 0.0, 180.0), stroke));
            let faint = Stroke::new(0.9 * u, ink.gamma_multiply(0.55));
            painter.add(Shape::line(ellipse(p(cx, 8.0), rx * u, ry * u, 0.0, 180.0), faint));
        }
    }

    if badge != Badge::None {
        paint_badge(&painter, ui.visuals().panel_fill, u, p(12.0, 12.0), badge);
    }
}

fn paint_badge(painter: &egui::Painter, bg: Color32, u: f32, c: Pos2, badge: Badge) {
    let fill = match badge {
        Badge::None => return,
        Badge::Offline => OFFLINE,
        Badge::Restoring => RESTORING,
        Badge::Warning => WARNING,
        Badge::Contained => CONTAINED,
    };
    // A ring of background colour separates the badge from the glyph underneath.
    painter.circle_filled(c, 4.8 * u, bg);
    painter.circle_filled(c, 4.0 * u, fill);
    let w = Stroke::new((1.1 * u).max(1.0), Color32::WHITE);
    let at = |x: f32, y: f32| c + vec2(x * u, y * u);
    match badge {
        Badge::Offline => {
            // Down arrow.
            painter.line_segment([at(0.0, -2.4), at(0.0, 1.9)], w);
            painter.add(Shape::line(vec![at(-1.7, 0.2), at(0.0, 1.9), at(1.7, 0.2)], w));
        }
        Badge::Restoring => {
            // Clockwise circular arrow, open at the top, head at the upper left pointing up-right.
            painter.add(Shape::line(ellipse(c, 2.0 * u, 2.0 * u, -45.0, 225.0), w));
            let tip = at(-1.414, -1.414);
            painter.add(Shape::line(vec![tip + vec2(-1.3 * u, 0.0), tip, tip + vec2(0.0, 1.3 * u)], w));
        }
        Badge::Warning => {
            painter.line_segment([at(0.0, -2.3), at(0.0, 0.6)], w);
            painter.circle_filled(at(0.0, 2.0), 0.7 * u, Color32::WHITE);
        }
        Badge::Contained => {
            // A box: the database carries its own users and settings.
            let r = Rect::from_center_size(c, Vec2::splat(3.6 * u));
            painter.rect_stroke(r, 0.6 * u, w, egui::StrokeKind::Middle);
        }
        Badge::None => {}
    }
}

/// Points along an ellipse from `from` to `to` degrees (0 = east, 90 = south in screen space).
fn ellipse(c: Pos2, rx: f32, ry: f32, from: f32, to: f32) -> Vec<Pos2> {
    let steps = 24;
    (0..=steps)
        .map(|i| {
            let a = (from + (to - from) * i as f32 / steps as f32).to_radians();
            pos2(c.x + rx * a.cos(), c.y + ry * a.sin())
        })
        .collect()
}
