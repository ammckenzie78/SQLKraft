//! The Object Explorer panel: the server tree and its context menus.

use eframe::egui::{self, Align, Color32, Layout};

use super::dialogs::DialogPurpose;
use super::{Action, ConnState, SqlKraftApp};
use crate::db::Session;
use crate::explorer::{self, Children, Node, NodeKind};
use crate::icons;
use crate::properties;
use crate::scripting::ScriptMode;

impl SqlKraftApp {
    pub(super) fn object_explorer(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.strong("Object Explorer");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.small_button("Connect ▾").clicked() {
                    let info = self.settings.recent.first().cloned().unwrap_or_default();
                    self.open_dialog(DialogPurpose::ObjectExplorer, info);
                }
            });
        });
        ui.separator();
        let mut actions = Vec::new();
        egui::ScrollArea::both().auto_shrink(false).show(ui, |ui| {
            if self.servers.is_empty() {
                ui.weak("Not connected. Click Connect to add a server.");
            }
            for (si, server) in self.servers.iter_mut().enumerate() {
                let label = match (&server.state, &server.facts) {
                    (ConnState::Connected, Some(f)) => {
                        format!("{} (SQL Server {} - {})", server.info.server, f.product_version, f.login)
                    }
                    (ConnState::Connecting, _) => format!("{} (connecting…)", server.info.server),
                    (ConnState::Failed(_), _) => format!("{} (failed)", server.info.server),
                    _ => format!("{} (disconnected)", server.info.server),
                };
                server.root.label = label;
                server.root.badge = match &server.facts {
                    Some(f) if f.contained_auth => explorer::Badge::Contained,
                    _ => explorer::Badge::None,
                };
                if let ConnState::Failed(e) = &server.state {
                    ui.colored_label(Color32::from_rgb(220, 80, 80), e);
                }
                let selected = self.selection.as_ref().filter(|s| s.server == si).map(|s| s.node);
                let ctx = NodeCtx {
                    session: &server.session,
                    server: si,
                    selected,
                    server_name: &server.info.server,
                    claude_writes: server.claude_writes,
                };
                show_node(ui, &mut server.root, &ctx, &mut actions);
            }
        });
        for a in actions {
            if let Action::CopyText(t) = &a {
                ui.ctx().copy_text(t.clone());
            }
            self.handle_action(a);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Widgets
// ---------------------------------------------------------------------------------------------

/// What `show_node` needs from the server it is drawing.
struct NodeCtx<'a> {
    session: &'a Session,
    server: usize,
    server_name: &'a str,
    /// The selected node's id, if it is in this server's tree.
    selected: Option<u64>,
    /// The user let Claude change data on this server.
    claude_writes: bool,
}

/// The name the Properties pane shows: the object's own name, without the extras in its label.
fn node_name(node: &Node, server_name: &str) -> String {
    match &node.kind {
        NodeKind::Server => server_name.to_string(),
        NodeKind::Column { name, .. } | NodeKind::Index { name, .. } | NodeKind::Login { name } => name.clone(),
        _ => node.label.clone(),
    }
}

fn show_node(ui: &mut egui::Ui, node: &mut Node, ctx: &NodeCtx, actions: &mut Vec<Action>) {
    let icon = node.kind.icon();
    let is_selected = ctx.selected == Some(node.id);
    let (node_id, node_kind, name) = (node.id, node.kind.clone(), node_name(node, ctx.server_name));
    let select = |show: bool| Action::Select {
        server: ctx.server,
        node: node_id,
        kind: node_kind.clone(),
        name: name.clone(),
        show,
    };
    if node.is_leaf() {
        ui.horizontal(|ui| {
            ui.add_space(18.0);
            let r = ui.selectable_label(is_selected, format!("{icon} {}", node.label));
            if r.clicked() || r.secondary_clicked() {
                actions.push(select(false));
            }
            r.context_menu(|ui| {
                if ui.button("Copy").clicked() {
                    let name = node.label.split(" (").next().unwrap_or(&node.label).to_string();
                    actions.push(Action::CopyText(name));
                    ui.close();
                }
                if properties::query(&node.kind).is_some() && ui.button("Properties").clicked() {
                    actions.push(select(true));
                    ui.close();
                }
            });
        });
        return;
    }
    let id = ui.make_persistent_id(("node", node.id));
    let state = egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, false);
    if state.is_open() {
        node.ensure_loading(ctx.session);
    }
    let kind = node.kind.clone();
    let badge = node.badge;
    let painted = icons::is_painted(&kind);
    let label = if painted { node.label.clone() } else { format!("{icon} {}", node.label) };
    let select_action = select(false);
    let show_action = select(true);
    let header = state.show_header(ui, |ui| {
        let icon_r = painted.then(|| icons::show(ui, &kind, badge));
        let r = ui.selectable_label(is_selected, label);
        let r = match icon_r {
            Some(i) => r.union(i),
            None => r,
        };
        if r.double_clicked() {
            match &kind {
                NodeKind::Table { .. } | NodeKind::View { .. } => {
                    actions.push(Action::SelectTop { server: ctx.server, kind: kind.clone(), run: true })
                }
                _ => {}
            }
        }
        r
    });
    let refresh = std::cell::Cell::new(false);
    let (_, header_inner, _) = header.body(|ui| match &mut node.children {
        Children::NotLoaded | Children::Loading(_) => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.weak("Loading…");
            });
        }
        Children::Failed(e) => {
            ui.colored_label(Color32::from_rgb(220, 80, 80), e.as_str());
        }
        Children::Loaded(children) => {
            if children.is_empty() {
                ui.weak("(empty)");
            }
            for c in children {
                show_node(ui, c, ctx, actions);
            }
        }
    });
    let header_label = header_inner.inner;
    if header_label.clicked() || header_label.secondary_clicked() {
        actions.push(select_action);
    }
    header_label.context_menu(|ui| {
        node_menu(ui, &kind, ctx.server, ctx.claude_writes, actions, &refresh);
        if properties::query(&kind).is_some() {
            ui.separator();
            if ui.button("Properties").clicked() {
                actions.push(show_action);
                ui.close();
            }
        }
    });
    if header_label.clicked() {
        // Click on the label toggles, like a tree view.
        let mut s = egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), id, false);
        s.toggle(ui);
        s.store(ui.ctx());
    }
    if refresh.get() {
        node.refresh();
    }
}

fn node_menu(
    ui: &mut egui::Ui,
    kind: &NodeKind,
    server: usize,
    claude_writes: bool,
    actions: &mut Vec<Action>,
    refresh: &std::cell::Cell<bool>,
) {
    let mut item = |ui: &mut egui::Ui, label: &str, action: Action| {
        if ui.button(label).clicked() {
            actions.push(action);
            ui.close();
        }
    };
    match kind {
        NodeKind::Server => {
            item(ui, "New Query", Action::NewQuery { server, db: None });
            item(ui, "Reconnect", Action::Reconnect(server));
            item(ui, "Disconnect", Action::Disconnect(server));
            ui.separator();
            let label = if claude_writes { "✔ Allow Claude to modify data" } else { "Allow Claude to modify data" };
            item(ui, label, Action::ToggleClaudeWrites(server));
        }
        NodeKind::DatabasesFolder => {
            item(ui, "Restore Database…", Action::Restore { server, db: None });
        }
        NodeKind::Database { db } => {
            item(ui, "New Query", Action::NewQuery { server, db: Some(db.clone()) });
            item(ui, "Copy name", Action::CopyText(db.clone()));
            ui.menu_button("Tasks", |ui| {
                item(ui, "Back Up…", Action::BackUp { server, db: db.clone() });
                item(ui, "Restore Database…", Action::Restore { server, db: Some(db.clone()) });
                ui.separator();
                ui.menu_button("Shrink", |ui| {
                    item(ui, "Database…", Action::Shrink { server, db: db.clone(), database: true });
                    item(ui, "Files…", Action::Shrink { server, db: db.clone(), database: false });
                });
            });
        }
        NodeKind::TablesFolder { db } => {
            item(ui, "New Table…", Action::NewTable { server, db: db.clone() });
        }
        NodeKind::Table { .. } | NodeKind::View { .. } | NodeKind::Procedure { .. } | NodeKind::Function { .. } => {
            let is_table = matches!(kind, NodeKind::Table { .. });
            if is_table {
                item(ui, "Design", Action::Design { server, kind: kind.clone() });
            }
            if matches!(kind, NodeKind::Table { .. } | NodeKind::View { .. }) {
                item(ui, "Select Top 1000 Rows", Action::SelectTop { server, kind: kind.clone(), run: true });
                item(ui, "Script as SELECT", Action::SelectTop { server, kind: kind.clone(), run: false });
            }
            if matches!(kind, NodeKind::Procedure { .. }) {
                item(ui, "Execute Stored Procedure…", Action::ExecuteProc { server, kind: kind.clone() });
            }
            let mut modes = vec![ScriptMode::Create];
            if !is_table {
                modes.push(ScriptMode::Alter);
            }
            modes.extend([ScriptMode::Drop, ScriptMode::DropAndCreate]);
            for mode in modes {
                item(ui, mode.label(), Action::Script { server, kind: kind.clone(), mode });
            }
            if let Some(n) = kind.object_name() {
                item(ui, "Copy name", Action::CopyText(n));
            }
        }
        _ => {}
    }
    if ui.button("Refresh").clicked() {
        refresh.set(true);
        ui.close();
    }
}
