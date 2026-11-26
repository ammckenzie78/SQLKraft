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
    self, EventFilter, Id, Key, KeyboardShortcut, LayerId, Modifiers, Order, SurrenderFocusOn, text::CCursor,
    text::CCursorRange, text_edit::TextEditOutput, text_edit::TextEditState,
};
use tokio::sync::oneshot::{self, error::TryRecvError};

use super::query_tab::QueryTab;
use super::{ConnState, SqlKraftApp, keys};
use crate::completion::values::{self as vals, Resolved};
use crate::completion::{self, Catalog, Item, Metadata};
use crate::db::{Cell, ConnectionInfo, FetchResult, Repaint, Session};

mod list;
mod values;
use list::popup;
use values::{ValueCache, ValueOpts};

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

/// Catalogs per (connection, database), shared by every tab on the same connection, and the
/// value suggestions' cache.
pub(super) struct MetaCache {
    entries: HashMap<(String, String), Entry>,
    values: ValueCache,
}

/// What the completion settings say.
pub(super) struct CompletionOpts {
    pub auto_popup: bool,
    pub values: ValueOpts,
}

impl CompletionOpts {
    pub(super) fn from(s: &crate::settings::Settings) -> Self {
        let values = ValueOpts {
            on: s.value_suggestions,
            scan_rows: s.value_scan_rows,
            auto_max_table_rows: s.value_auto_max_table_rows,
        };
        Self { auto_popup: s.completion_auto_popup, values }
    }
}

/// Same server and login: same metadata.
fn conn_key(info: &ConnectionInfo) -> String {
    format!("{}|{:?}|{}", info.server.to_lowercase(), info.auth, info.user.to_lowercase())
}

impl MetaCache {
    pub(super) fn new(rt: tokio::runtime::Handle, repaint: Repaint) -> Self {
        Self { entries: HashMap::new(), values: ValueCache::new(rt, repaint) }
    }

    /// Starts loading `db` through `session` unless it is cached (or failed) already.
    fn ensure(&mut self, key: &str, session: &Session, db: &str) {
        self.entries.entry((key.to_string(), db.to_lowercase())).or_insert_with(|| Entry::Loading {
            rx: Catalog::queries(db).map(|q| Some(session.fetch(q))),
            rows: Default::default(),
        });
    }

    fn forget_connection(&mut self, key: &str) {
        self.entries.retain(|(k, _), _| k != key);
        self.values.forget_connection(key);
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
        self.values.poll();
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
    /// Ctrl+Space was pressed for this word: value suggestions ignore the table size threshold.
    explicit: bool,
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
    opts: &CompletionOpts,
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
    let at = char_to_byte(text, caret);
    // Without a connection there are still keywords and functions.
    let key = tab.session.as_ref().map(|s| conn_key(&s.info));
    let mut missing: Vec<String> = Vec::new();
    // Experimental: where a column's value goes, that column (`None` until its catalog loads).
    let value = match (opts.values.on, &key) {
        (true, Some(k)) => vals::spot(text, at).and_then(|s| {
            let lookup = |db: &str| cache.get(k, db);
            let meta = Metadata { current_db: &tab.current_db, databases: &tab.databases, catalog: &lookup };
            match vals::resolve(&s, &meta) {
                Resolved::Column(t) => Some((s, Some(t))),
                Resolved::Missing(dbs) => {
                    missing = dbs;
                    Some((s, None))
                }
                Resolved::No => None,
            }
        }),
        _ => None,
    };
    let cx = completion::context(text, at);
    let (range, prefix) = match (&value, &cx) {
        (Some((s, _)), _) => (s.replace.clone(), s.typed.clone()),
        (None, Some(cx)) => (cx.word.clone(), cx.prefix.clone()),
        (None, None) => {
            close(&mut tab.completion);
            return;
        }
    };
    // In a string only values are offered, even with nothing typed yet.
    let quoted = value.as_ref().is_some_and(|(s, _)| s.quoted);
    let word = byte_to_char(text, range.start)..byte_to_char(text, range.end);
    let st = &mut tab.completion;
    let typed = ctx.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Text(_))));
    let qualified = cx.as_ref().is_some_and(|c| !c.qualifier.is_empty());
    let after_dot = prefix.is_empty() && qualified;
    if keys.invoke {
        st.active = true;
        st.dismissed_at = None;
        st.explicit = true;
    } else if response.changed() {
        if st.active && !quoted && prefix.is_empty() && !qualified {
            st.active = false;
        } else if !st.active && opts.auto_popup && typed && st.dismissed_at != Some(word.start) {
            st.active = quoted || after_dot || prefix.chars().filter(|c| c.is_alphanumeric() || *c == '_').count() >= 2;
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

    let mut items = Vec::new();
    let mut busy = false;
    if let (Some((s, Some(t))), Some(session)) = (&value, &tab.session) {
        let offer = cache.values.offer(&session.info, t, &s.typed, st.explicit, keys.invoke, &opts.values);
        busy = offer.busy;
        items = vals::items(&offer.values, t, s, offer.approximate);
    }
    if let (false, Some(cx)) = (quoted, &cx) {
        let found = {
            let lookup = |db: &str| key.as_deref().and_then(|k| cache.get(k, db));
            let meta = Metadata { current_db: &tab.current_db, databases: &tab.databases, catalog: &lookup };
            completion::suggest(cx, &meta)
        };
        missing.extend(found.missing);
        items.extend(found.items);
    }
    if let (Some(session), Some(key)) = (&tab.session, &key) {
        for db in missing.iter().filter(|db| !db.is_empty()) {
            cache.ensure(key, session, db);
        }
    }
    if cache.loading() || busy {
        ctx.request_repaint_after(Duration::from_millis(if busy { 50 } else { 100 }));
    }
    if word.start != st.word.start || prefix != st.prefix {
        st.selected = 0;
    }
    st.items = items;
    st.word = word;
    st.prefix = prefix;
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
    let chosen = if keys.accept && n > 0 {
        Some(st.selected.min(n - 1))
    } else {
        st.visible = n > 0;
        // Keep focus on Esc from the very next frame (the TextEdit only updates it as it runs).
        let filter = EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: st.visible };
        ctx.memory_mut(|m| m.set_focus_lock_filter(editor, filter));
        if st.visible { popup(&ctx, editor, output, st) } else { None }
    };
    if let Some(i) = chosen {
        let item = st.items[i].clone();
        // An item may replace more than the word (`= '` becomes `IS NULL`).
        let word = match &item.replace {
            Some(r) => byte_to_char(&tab.text, r.start)..byte_to_char(&tab.text, r.end),
            None => st.word.clone(),
        };
        close(st);
        accept(&ctx, editor, &mut tab.text, word, &item.insert);
    }
}

fn close(st: &mut CompletionState) {
    st.active = false;
    st.visible = false;
    st.items.clear();
    st.selected = 0;
    st.explicit = false;
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
        if ui
            .checkbox(&mut self.settings.value_suggestions, "Suggest column values (experimental)")
            .on_hover_text(
                "In WHERE c.City = '…, offer the column's most common values. Reads table data on a separate \
                 connection, after a typing pause, at most a set number of rows; large tables only on Ctrl+Space.",
            )
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
