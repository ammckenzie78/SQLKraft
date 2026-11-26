//! Small square buttons with painted icons. Icon glyphs from egui's emoji font (✖ ✕ ⟳) sit
//! off-centre in buttons and some come out in the emoji's own colour, so these are drawn with
//! lines at the button's exact centre, in the theme's text colour.

use std::f32::consts::TAU;

use eframe::egui::{self, Color32, Pos2, Sense, Stroke, Vec2};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Icon {
    /// ✕: close or remove.
    Close,
    /// ⟳: refresh.
    Refresh,
}

/// A square icon button as tall as other controls. `danger` turns the icon red on hover
/// (for remove/delete).
pub(super) fn icon_button(ui: &mut egui::Ui, icon: Icon, enabled: bool, danger: bool) -> egui::Response {
    let size = ui.spacing().interact_size.y;
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), sense);
    if ui.is_rect_visible(rect) {
        let v = if enabled { ui.style().interact(&resp) } else { &ui.visuals().widgets.noninteractive };
        ui.painter().rect(rect, v.corner_radius, v.weak_bg_fill, v.bg_stroke, egui::StrokeKind::Inside);
        let color = if !enabled {
            ui.visuals().weak_text_color()
        } else if danger && resp.hovered() {
            Color32::from_rgb(220, 80, 80)
        } else {
            v.fg_stroke.color
        };
        paint(ui.painter(), icon, rect.center(), size * 0.2, Stroke::new(1.6, color));
    }
    resp
}

/// Draws `icon` centred on `c`; `r` is half its width.
pub(super) fn paint(p: &egui::Painter, icon: Icon, c: Pos2, r: f32, stroke: Stroke) {
    match icon {
        Icon::Close => {
            p.line_segment([c + Vec2::new(-r, -r), c + Vec2::new(r, r)], stroke);
            p.line_segment([c + Vec2::new(-r, r), c + Vec2::new(r, -r)], stroke);
        }
        Icon::Refresh => {
            // An open circle (gap at the top right) with an arrowhead on its end.
            let r = r * 1.15;
            let (start, end) = (-0.20 * TAU, 0.62 * TAU);
            let at = |a: f32| c + Vec2::new(a.cos(), a.sin()) * r;
            let points: Vec<Pos2> = (0..=24).map(|i| at(start + (end - start) * i as f32 / 24.0)).collect();
            p.add(egui::Shape::line(points, stroke));
            // Arrowhead at `end`, pointing along the direction of travel (increasing angle).
            let tip = at(end);
            let along = Vec2::new(-end.sin(), end.cos());
            let out = Vec2::new(end.cos(), end.sin());
            let head = r * 0.75;
            p.add(egui::Shape::convex_polygon(
                vec![tip + along * head * 0.6, tip - out * head * 0.55, tip + out * head * 0.55],
                stroke.color,
                Stroke::NONE,
            ));
        }
    }
}
