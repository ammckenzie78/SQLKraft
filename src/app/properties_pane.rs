//! The Properties pane (F4): details of the node selected in Object Explorer.

use eframe::egui::{self, Align, Color32, Layout, RichText};
use tokio::sync::oneshot;

use super::{ConnState, SqlKraftApp};
use crate::db::FetchResult;
use crate::explorer::NodeKind;
use crate::properties::{self, PropQuery, Property};

/// The node selected in Object Explorer.
#[derive(Clone, PartialEq)]
pub(super) struct Selection {
    pub(super) server: usize,
    pub(super) node: u64,
    pub(super) kind: NodeKind,
    pub(super) name: String,
}

pub(super) enum PropsState {
    Loading(oneshot::Receiver<FetchResult>, PropQuery),
    Loaded(Vec<Property>),
    Failed(String),
}

/// What the Properties pane is showing, and for which selection.
pub(super) struct PropsView {
    pub(super) for_selection: Selection,
    pub(super) connection: Vec<Property>,
    pub(super) state: Option<PropsState>,
}

impl SqlKraftApp {
    pub(super) fn toggle_properties(&mut self) {
        self.settings.show_properties = !self.settings.show_properties;
        self.settings.save();
    }

    /// Starts loading properties when the pane is open and the selection has changed.
    pub(super) fn sync_properties(&mut self) {
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

    pub(super) fn pump_properties(&mut self) {
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

    pub(super) fn properties_pane(&mut self, ui: &mut egui::Ui) {
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
