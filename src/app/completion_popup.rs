//! The completion popup over the query editor, and the per-(connection, database) metadata
//! cache behind it. What to offer is decided in [`crate::completion`].
//!
//! The popup never takes focus: while it shows, its keys (arrows, page keys, Enter, Tab, Esc)
//! are taken out of the input before the editor's `TextEdit` runs, and the TextEdit keeps
//! focus through clicks on the popup.

use std::collections::HashMap;
use std::ops::Range;
use std::time::Duration;

use eframe::egui::{
    self, Align2, Color32, EventFilter, FontId, Id, Key, KeyboardShortcut, LayerId, Modifiers, Order, Sense,
    SurrenderFocusOn, text::CCursor, text::CCursorRange, text_edit::TextEditOutput, text_edit::TextEditState,
};
use tokio::sync::oneshot::{self, error::TryRecvError};

use super::query_tab::QueryTab;
use super::{ConnState, SqlKraftApp, keys};
use crate::completion::{self, Catalog, Item, Kind, Metadata};
use crate::db::{Cell, ConnectionInfo, FetchResult, Session};

const REFRESH: KeyboardShortcut = KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::R);
/// Rows shown before the list scrolls; also what PageUp / PageDown move by.
const ROWS: usize = 10;
const WIDTH: f32 = 460.0;

// ---------------------------------------------------------------------------------------------
// Metadata cache
// ---------------------------------------------------------------------------------------------

enum Entry {
    Loading { rx: [Option<oneshot::Receiver<FetchResult>>; 3], rows: [Option<Vec<Vec<Cell>>>; 3] },
    Loaded(Catalog),
    Failed,
}

/// Catalogs per (connection, database), shared by every tab on the same connection.
#[derive(Default)]
pub(super) struct MetaCache {
    entries: HashMap<(String, String), Entry>,
}

/// Same server and login: same metadata.
fn conn_key(info: &ConnectionInfo) -> String {
    format!("{}|{:?}|{}", info.server.to_lowercase(), info.auth, info.user.to_lowercase())
}

impl MetaCache {
    /// Starts loading `db` through `session` unless it is cached (or failed) already.
    fn ensure(&mut self, key: &str, session: &Session, db: &str) {
        self.entries.entry((key.to_string(), db.to_lowercase())).or_insert_with(|| Entry::Loading {
            rx: Catalog::queries(db).map(|q| Some(session.fetch(q))),
            rows: Default::default(),
        });
    }

    fn forget_connection(&mut self, key: &str) {
        self.entries.retain(|(k, _), _| k != key);
    }

    fn get(&self, key: &str, db: &str) -> Option<&Catalog> {
        match self.entries.get(&(key.to_string(), db.to_lowercase())) {
            Some(Entry::Loaded(c)) => Some(c),
            _ => None,
        }
    }

    fn loading(&self) -> bool {
        self.entries.values().any(|e| matches!(e, Entry::Loading { .. }))
    }

    /// Collects finished catalog queries; called every frame.
    pub(super) fn poll(&mut self) {
        for entry in self.entries.values_mut() {
            let Entry::Loading { rx, rows } = entry else { continue };
            let mut failed = false;
            for (slot, out) in rx.iter_mut().zip(rows.iter_mut()) {
                let Some(r) = slot else { continue };
                match r.try_recv() {
                    Ok(Ok((_, data))) => {
                        *out = Some(data);
                        *slot = None;
                    }
                    Ok(Err(_)) | Err(TryRecvError::Closed) => failed = true,
                    Err(TryRecvError::Empty) => {}
                }
            }
            if failed {
                *entry = Entry::Failed;
            } else if let [Some(_), Some(_), Some(_)] = rows {
                let [Some(s), Some(o), Some(c)] = std::mem::take(rows) else { continue };
                *entry = Entry::Loaded(Catalog::from_rows(s, o, c));
            }
        }
    }

    /// Keeps the tab's database loaded, and reloads after `USE` (a database change) and after a
    /// script with DDL finished. Called for every tab after its events.
    pub(super) fn track(&mut self, tab: &mut QueryTab, finished: bool) {
        let Some(session) = &tab.session else { return };
        if tab.conn != ConnState::Connected {
            return;
        }
        let key = conn_key(&session.info);
        if finished && std::mem::take(&mut tab.completion.ran_ddl) {
            self.forget_connection(&key);
        }
        if tab.current_db.is_empty() {
            return;
        }
        if !tab.completion.last_db.eq_ignore_ascii_case(&tab.current_db) {
            if !tab.completion.last_db.is_empty() {
                self.entries.remove(&(key.clone(), tab.current_db.to_lowercase()));
            }
            tab.completion.last_db = tab.current_db.clone();
        }
        self.ensure(&key, session, &tab.current_db);
    }
}

// ---------------------------------------------------------------------------------------------
// Per-tab popup state
// ---------------------------------------------------------------------------------------------

#[derive(Default)]
pub(super) struct CompletionState {
    /// Wanted open; shown while it has items.
    active: bool,
    visible: bool,
    selected: usize,
    items: Vec<Item>,
    /// Char range of the word the items replace.
    word: Range<usize>,
    prefix: String,
    /// Word start (char index) where Esc closed the popup: typing there doesn't reopen it.
    dismissed_at: Option<usize>,
    scroll_to_selected: bool,
    /// The database the cache was last told about, to notice `USE`.
    last_db: String,
    /// The running script may change the schema.
    pub(super) ran_ddl: bool,
}

/// Keys taken from the input for the popup this frame.
#[derive(Default)]
pub(super) struct PopupKeys {
    moves: isize,
    accept: bool,
    dismiss: bool,
    invoke: bool,
    surrender: Option<SurrenderFocusOn>,
}

fn area_id(editor: Id) -> Id {
    editor.with("completion")
}

/// Presses of `key` without modifiers, removed from the input.
fn take_unmodified(input: &mut egui::InputState, key: Key) -> usize {
    let mut n = 0;
    input.events.retain(|e| {
        let hit = matches!(e, egui::Event::Key { key: k, pressed: true, modifiers, .. } if *k == key && modifiers.is_none());
        n += usize::from(hit);
        !hit
    });
    n
}

/// Before the editor's TextEdit: takes Ctrl+Space and, while the popup shows, its keys; keeps
/// the editor focused when the pointer is on the popup.
pub(super) fn before_edit(ctx: &egui::Context, editor: Id, st: &CompletionState) -> PopupKeys {
    let mut keys = PopupKeys::default();
    if !ctx.memory(|m| m.has_focus(editor)) {
        return keys;
    }
    ctx.input_mut(|i| {
        if i.consume_key(Modifiers::CTRL, Key::Space) {
            keys.invoke = true;
            // Some platforms also send the space as text.
            i.events.retain(|e| !matches!(e, egui::Event::Text(t) if t == " "));
        }
        if st.visible {
            let page = ROWS as isize;
            keys.moves = take_unmodified(i, Key::ArrowDown) as isize - take_unmodified(i, Key::ArrowUp) as isize
                + page * (take_unmodified(i, Key::PageDown) as isize - take_unmodified(i, Key::PageUp) as isize);
            // Enter on a word that is already typed out in full is a new line, not an accept.
            let complete = st.items.get(st.selected).is_some_and(|it| it.insert.eq_ignore_ascii_case(&st.prefix));
            let enter = if complete { 0 } else { take_unmodified(i, Key::Enter) };
            keys.accept = enter + take_unmodified(i, Key::Tab) > 0;
            keys.dismiss = take_unmodified(i, Key::Escape) > 0;
        }
    });
    let on_popup = st.visible
        && ctx.pointer_interact_pos().and_then(|p| ctx.layer_id_at(p))
            == Some(LayerId::new(Order::Foreground, area_id(editor)));
    if on_popup {
        keys.surrender = Some(ctx.options(|o| o.input_options.surrender_focus_on));
        ctx.options_mut(|o| o.input_options.surrender_focus_on = SurrenderFocusOn::Never);
    }
    keys
}

fn char_to_byte(s: &str, c: usize) -> usize {
    s.char_indices().nth(c).map_or(s.len(), |(b, _)| b)
}

fn byte_to_char(s: &str, b: usize) -> usize {
    s[..b.min(s.len())].chars().count()
}

/// Replaces the char range `word` of `text` with `insert` and puts the caret after it.
fn accept(ctx: &egui::Context, editor: Id, text: &mut String, word: Range<usize>, insert: &str) {
    let (a, b) = (char_to_byte(text, word.start), char_to_byte(text, word.end));
    text.replace_range(a..b, insert);
    let caret = word.start + insert.chars().count();
    let mut state = TextEditState::load(ctx, editor).unwrap_or_default();
    state.cursor.set_char_range(Some(CCursorRange::one(CCursor::new(caret))));
    state.store(ctx, editor);
    ctx.request_repaint();
}

/// After the editor's TextEdit: decides whether the popup is open, refreshes its items, acts on
/// the keys taken by [`before_edit`] and draws it at the caret.
pub(super) fn after_edit(
    ui: &egui::Ui,
    editor: Id,
    tab: &mut QueryTab,
    output: &TextEditOutput,
    keys: PopupKeys,
    cache: &mut MetaCache,
    auto_popup: bool,
) {
    let ctx = ui.ctx().clone();
    if let Some(s) = keys.surrender {
        ctx.options_mut(|o| o.input_options.surrender_focus_on = s);
    }
    let response = &output.response.response;
    let focused = response.has_focus();
    let caret = output.cursor_range.filter(|r| r.primary == r.secondary).map(|r| r.primary.index.0);
    let (Some(caret), true) = (caret, focused) else {
        close(&mut tab.completion);
        return;
    };
    let text = &tab.text;
    let Some(cx) = completion::context(text, char_to_byte(text, caret)) else {
        close(&mut tab.completion);
        return;
    };
    let word = byte_to_char(text, cx.word.start)..byte_to_char(text, cx.word.end);
    let st = &mut tab.completion;
    let typed = ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Text(_))));
    let after_dot = cx.prefix.is_empty() && !cx.qualifier.is_empty();
    if keys.invoke {
        st.active = true;
        st.dismissed_at = None;
    } else if response.changed() {
        if st.active && cx.prefix.is_empty() && cx.qualifier.is_empty() {
            st.active = false;
        } else if !st.active && auto_popup && typed && st.dismissed_at != Some(word.start) {
            st.active = after_dot || cx.prefix.chars().filter(|c| c.is_alphanumeric() || *c == '_').count() >= 2;
        }
    } else if st.active && word.start != st.word.start {
        // The caret moved to another word.
        st.active = false;
    }
    if st.dismissed_at.is_some_and(|d| d != word.start) {
        st.dismissed_at = None;
    }
    if !st.active {
        close(st);
        return;
    }

    // Without a connection there are still keywords and functions.
    let key = tab.session.as_ref().map(|s| conn_key(&s.info));
    let found = {
        let lookup = |db: &str| key.as_deref().and_then(|k| cache.get(k, db));
        let meta = Metadata { current_db: &tab.current_db, databases: &tab.databases, catalog: &lookup };
        completion::suggest(&cx, &meta)
    };
    if let (Some(session), Some(key)) = (&tab.session, &key) {
        for db in found.missing.iter().filter(|db| !db.is_empty()) {
            cache.ensure(key, session, db);
        }
    }
    if cache.loading() {
        ctx.request_repaint_after(Duration::from_millis(100));
    }
    if word.start != st.word.start || cx.prefix != st.prefix {
        st.selected = 0;
    }
    st.items = found.items;
    st.word = word;
    st.prefix = cx.prefix;
    let n = st.items.len();
    if n > 0 && keys.moves != 0 {
        st.selected = st.selected.saturating_add_signed(keys.moves).min(n - 1);
        st.scroll_to_selected = true;
    }
    if keys.dismiss {
        st.dismissed_at = Some(st.word.start);
        close(st);
        return;
    }
    if keys.accept && n > 0 {
        let insert = st.items[st.selected.min(n - 1)].insert.clone();
        let word = st.word.clone();
        close(st);
        accept(&ctx, editor, &mut tab.text, word, &insert);
        return;
    }
    st.visible = n > 0;
    // Keep focus on Esc from the very next frame (the TextEdit only updates it as it runs).
    ctx.memory_mut(|m| {
        m.set_focus_lock_filter(
            editor,
            EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: st.visible },
        )
    });
    if st.visible
        && let Some(i) = popup(&ctx, editor, output, st)
    {
        let (insert, word) = (st.items[i].insert.clone(), st.word.clone());
        close(st);
        accept(&ctx, editor, &mut tab.text, word, &insert);
    }
}

fn close(st: &mut CompletionState) {
    st.active = false;
    st.visible = false;
    st.items.clear();
    st.selected = 0;
}

fn kind_color(kind: Kind, dark: bool) -> Color32 {
    let (d, l) = match kind {
        Kind::Keyword => ((86, 156, 214), (0, 0, 200)),
        Kind::Function => ((220, 140, 220), (170, 0, 170)),
        Kind::Table | Kind::View => ((78, 201, 176), (0, 120, 110)),
        Kind::Column => ((156, 220, 254), (0, 100, 150)),
        Kind::Procedure => ((220, 180, 100), (150, 90, 0)),
        Kind::Schema | Kind::Database => ((200, 200, 120), (110, 110, 0)),
    };
    let (r, g, b) = if dark { d } else { l };
    Color32::from_rgb(r, g, b)
}

fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max { s.to_string() } else { format!("{}…", s.chars().take(max - 1).collect::<String>()) }
}

/// Draws the list under (or above) the word at the caret; returns a clicked row.
fn popup(ctx: &egui::Context, editor: Id, output: &TextEditOutput, st: &mut CompletionState) -> Option<usize> {
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

// ---------------------------------------------------------------------------------------------
// Menu and shortcut
// ---------------------------------------------------------------------------------------------

impl SqlKraftApp {
    /// Ctrl+Shift+R; taken before Ctrl+R (results pane), which would also match it.
    pub(super) fn completion_shortcuts(&mut self, ctx: &egui::Context) {
        if ctx.input_mut(|i| i.consume_shortcut(&REFRESH)) {
            self.refresh_completion();
        }
    }

    fn refresh_completion(&mut self) {
        if let Some(s) = self.tabs.get(self.active).and_then(|t| t.session.as_ref()) {
            self.meta.forget_connection(&conn_key(&s.info));
            self.toast("IntelliSense cache refreshed.");
        }
    }

    /// The IntelliSense items of the View menu.
    pub(super) fn completion_menu(&mut self, ui: &mut egui::Ui) {
        ui.separator();
        if ui
            .checkbox(&mut self.settings.completion_auto_popup, "Show completion list while typing")
            .on_hover_text("Ctrl+Space shows it anytime")
            .changed()
        {
            self.settings.save();
        }
        let has_session = self.tabs.get(self.active).is_some_and(|t| t.session.is_some());
        if ui.add_enabled(has_session, egui::Button::new(keys("Refresh IntelliSense Cache\tCtrl+Shift+R"))).clicked() {
            self.refresh_completion();
            ui.close();
        }
    }
}
