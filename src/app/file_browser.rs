//! A file browser for the SQL Server machine's filesystem, listed through T-SQL
//! (`sys.dm_os_enumerate_filesystem`, falling back to `xp_dirtree`), with an editable path bar
//! (Enter jumps, Tab completes), breadcrumbs, quick links, sortable columns, a filter,
//! multi-select for striped media sets and a RESTORE HEADERONLY / FILELISTONLY preview.

use eframe::egui::{self, Key, Modifiers};

use super::runner::Pending;
use crate::backup::{self as b, BackupSet, FsEntry, LogicalFile, PathStyle, ServerFolders};
use crate::db::Session;
use crate::maintenance::is_permission_error;

#[derive(Clone, Copy, PartialEq)]
pub(super) enum BrowseMode {
    /// Pick one or more existing backup files (several = one striped media set).
    Open,
    /// Pick a file name to write, which may not exist yet.
    Save,
}

#[derive(Clone, Copy, PartialEq)]
enum SortBy {
    Name,
    Size,
    Modified,
}

pub(super) enum Browse {
    /// The chosen paths, and the folder they are in (for the recent list).
    Picked { dir: String, paths: Vec<String> },
    Cancelled,
}

#[derive(Default)]
struct Preview {
    key: Vec<String>,
    header: Pending,
    filelist: Pending,
    sets: Vec<BackupSet>,
    files: Vec<LogicalFile>,
    error: Option<String>,
}

pub(super) struct FileBrowser {
    title: String,
    mode: BrowseMode,
    style: PathStyle,
    dir: String,
    path_text: String,
    entries: Vec<FsEntry>,
    list: Pending,
    via_dirtree: bool,
    error: Option<String>,
    note: Option<String>,
    filter: String,
    all_files: bool,
    sort: (SortBy, bool),
    selected: Vec<String>,
    cursor: Option<usize>,
    anchor: Option<usize>,
    file_name: String,
    quick: Vec<(String, String)>,
    roots: Vec<String>,
    roots_rx: Vec<Pending>,
    pending_select: Option<String>,
    pending_complete: Option<String>,
    focus_path: bool,
    move_cursor_to_end: bool,
    scroll_to: Option<usize>,
    preview: Preview,
}

impl FileBrowser {
    /// `start` may be a folder or, in Save mode, a full file path.
    pub(super) fn new(title: &str, mode: BrowseMode, start: &str, folders: &ServerFolders, recent: &[String], meta: &Session) -> Self {
        let style = folders.style;
        let mut quick = vec![
            ("📦 Default backup folder".to_string(), folders.backup.clone()),
            ("🛢 Data folder".to_string(), folders.data.clone()),
        ];
        if folders.log != folders.data {
            quick.push(("📜 Log folder".into(), folders.log.clone()));
        }
        quick.extend(recent.iter().map(|r| (format!("🕘 {r}"), r.clone())));
        let roots_rx = match style {
            PathStyle::Windows => vec![Pending::new(meta.fetch(b::FILE_DRIVES_QUERY)), Pending::new(meta.fetch(b::FIXED_DRIVES_QUERY))],
            PathStyle::Posix => Vec::new(),
        };
        let mut roots: Vec<String> = match style {
            PathStyle::Posix => vec!["/".into()],
            PathStyle::Windows => Vec::new(),
        };
        for (_, p) in &quick {
            if let Some(r) = style.breadcrumbs(p).first().map(|c| c.1.clone()).filter(|r| style.is_root(r))
                && !roots.contains(&r)
            {
                roots.push(r);
            }
        }
        let (dir, file_name) = match mode {
            BrowseMode::Save if !start.is_empty() && style.parent(start).is_some() && !start.ends_with(style.sep()) => {
                (style.parent(start).unwrap_or_default(), style.file_name(start))
            }
            _ => (if start.is_empty() { folders.backup.clone() } else { start.to_string() }, String::new()),
        };
        let mut fb = Self {
            title: title.to_string(),
            mode,
            style,
            dir: String::new(),
            path_text: String::new(),
            entries: Vec::new(),
            list: Pending::default(),
            via_dirtree: false,
            error: None,
            note: None,
            filter: b::DEFAULT_FILTER.to_string(),
            all_files: mode == BrowseMode::Save,
            sort: (SortBy::Name, true),
            selected: Vec::new(),
            cursor: None,
            anchor: None,
            file_name,
            quick,
            roots,
            roots_rx,
            pending_select: None,
            pending_complete: None,
            focus_path: false,
            move_cursor_to_end: false,
            scroll_to: None,
            preview: Preview::default(),
        };
        fb.navigate(&dir, meta);
        fb
    }

    fn navigate(&mut self, dir: &str, meta: &Session) {
        self.dir = self.style.normalize_dir(dir);
        self.path_text = self.dir.clone();
        self.entries.clear();
        self.selected.clear();
        self.cursor = None;
        self.anchor = None;
        self.error = None;
        self.note = None;
        self.via_dirtree = false;
        self.list = Pending::new(meta.fetch(b::enumerate_query(&self.dir)));
    }

    /// Enter in the path bar: a folder, or a file whose folder is opened with it selected.
    fn jump(&mut self, meta: &Session) {
        let text = self.path_text.trim().to_string();
        let name = self.style.file_name(&text);
        let looks_like_file = name.contains('.') && !name.starts_with('.') && self.style.parent(&text).is_some();
        if looks_like_file {
            let parent = self.style.parent(&text).unwrap_or_default();
            self.navigate(&parent, meta);
            self.pending_select = Some(name);
        } else {
            self.navigate(&text, meta);
        }
    }

    /// Tab in the path bar: complete the last component from the server's listing.
    fn complete(&mut self, meta: &Session) {
        let text = self.path_text.clone();
        let (parent, prefix) = match text.rfind(|c| c == self.style.sep() || (self.style == PathStyle::Windows && c == '/')) {
            Some(i) => (text[..=i].to_string(), text[i + 1..].to_string()),
            None => return,
        };
        if self.style.normalize_dir(&parent) != self.dir {
            self.navigate(&parent, meta);
            self.path_text = text;
            self.pending_complete = Some(prefix);
            return;
        }
        self.complete_from_listing(&prefix);
    }

    fn complete_from_listing(&mut self, prefix: &str) {
        let lower = prefix.to_lowercase();
        let hits: Vec<&FsEntry> = self.entries.iter().filter(|e| e.name.to_lowercase().starts_with(&lower)).collect();
        let Some(first) = hits.first() else { return };
        let mut common: String = first.name.clone();
        for h in &hits[1..] {
            let n = common.chars().zip(h.name.chars()).take_while(|(a, b)| a.eq_ignore_ascii_case(b)).count();
            common = common.chars().take(n).collect();
        }
        let mut path = self.style.join(&self.dir, &common);
        if hits.len() == 1 && first.is_dir {
            path.push(self.style.sep());
        }
        self.path_text = path;
        self.move_cursor_to_end = true;
    }

    fn pump(&mut self, meta: &Session) {
        if let Some(r) = self.list.poll() {
            match r {
                Ok((_, rows)) => {
                    self.entries = if self.via_dirtree { b::parse_dirtree(&rows) } else { b::parse_enumerate(&rows) };
                    if self.entries.is_empty() {
                        self.note = Some(if self.via_dirtree {
                            "xp_dirtree returned nothing. It only lists folders for sysadmin logins; you can still type a full path.".into()
                        } else {
                            "This folder is empty, or it doesn't exist on the server.".into()
                        });
                    }
                    if let Some(name) = self.pending_select.take() {
                        self.selected = vec![name.clone()];
                        self.file_name = name;
                    }
                    if let Some(prefix) = self.pending_complete.take() {
                        self.complete_from_listing(&prefix);
                    }
                }
                Err(e) if !self.via_dirtree && b::is_missing_object(&e) => {
                    self.via_dirtree = true;
                    self.list = Pending::new(meta.fetch(b::dirtree_query(&self.dir)));
                }
                Err(e) => {
                    let why = if is_permission_error(&e) {
                        "The login can't list folders on the server. sys.dm_os_enumerate_filesystem (SQL Server 2017+) \
                         needs VIEW SERVER STATE on some versions; xp_dirtree needs sysadmin. You can still type a full path."
                    } else {
                        "Could not list this folder."
                    };
                    self.error = Some(format!("{why}\n{e}"));
                }
            }
        }
        for rx in &mut self.roots_rx {
            if let Some(Ok((_, rows))) = rx.poll() {
                for r in rows {
                    let Some(v) = r.into_iter().next().flatten() else { continue };
                    let root = self.style.normalize_dir(if v.len() == 1 { format!("{v}:") } else { v }.as_str());
                    if !self.roots.iter().any(|x| x.eq_ignore_ascii_case(&root)) {
                        self.roots.push(root);
                    }
                }
                self.roots.sort();
            }
        }
        self.pump_preview(meta);
    }

    fn selected_paths(&self) -> Vec<String> {
        self.selected.iter().map(|n| self.style.join(&self.dir, n)).collect()
    }

    fn pump_preview(&mut self, meta: &Session) {
        let p = &mut self.preview;
        if let Some(r) = p.header.poll() {
            match r {
                Ok((cols, rows)) => {
                    p.sets = b::parse_headers(&cols, &rows, &p.key);
                    if let Some(first) = p.sets.first() {
                        p.filelist = Pending::new(meta.fetch(b::filelistonly_query(&p.key, first.position)));
                    }
                }
                Err(e) => p.error = Some(e),
            }
        }
        if let Some(r) = p.filelist.poll() {
            match r {
                Ok((cols, rows)) => p.files = b::parse_filelist(&cols, &rows),
                Err(e) => p.error = Some(e),
            }
        }
        if self.mode != BrowseMode::Open || p.header.busy() || p.filelist.busy() {
            return;
        }
        let files: Vec<String> = self
            .selected
            .iter()
            .filter(|n| self.entries.iter().any(|e| &e.name == *n && !e.is_dir))
            .map(|n| self.style.join(&self.dir, n))
            .collect();
        if files != p.key {
            *p = Preview { key: files.clone(), ..Default::default() };
            if !files.is_empty() {
                p.header = Pending::new(meta.fetch(b::headeronly_query(&files)));
            }
        }
    }

    /// Indices into `entries`: folders first, then files matching the filter, sorted.
    fn visible(&self) -> Vec<usize> {
        let mut v: Vec<usize> = (0..self.entries.len())
            .filter(|&i| {
                let e = &self.entries[i];
                e.is_dir || self.all_files || b::matches_filter(&e.name, &self.filter)
            })
            .collect();
        let (by, asc) = self.sort;
        v.sort_by(|&a, &bb| {
            let (x, y) = (&self.entries[a], &self.entries[bb]);
            let ord = match by {
                SortBy::Name => x.name.to_lowercase().cmp(&y.name.to_lowercase()),
                SortBy::Size => x.size.cmp(&y.size),
                SortBy::Modified => x.modified.cmp(&y.modified),
            };
            y.is_dir.cmp(&x.is_dir).then(if asc { ord } else { ord.reverse() })
        });
        v
    }

    fn click(&mut self, vis: &[usize], pos: usize, mods: Modifiers) {
        let name = self.entries[vis[pos]].name.clone();
        let multi = self.mode == BrowseMode::Open;
        if multi && mods.shift {
            let a = self.anchor.unwrap_or(pos);
            let (lo, hi) = (a.min(pos), a.max(pos));
            self.selected = vis[lo..=hi].iter().map(|&i| self.entries[i].name.clone()).collect();
        } else if multi && mods.command {
            if let Some(i) = self.selected.iter().position(|n| *n == name) {
                self.selected.remove(i);
            } else {
                self.selected.push(name.clone());
            }
            self.anchor = Some(pos);
        } else {
            self.selected = vec![name.clone()];
            self.anchor = Some(pos);
        }
        self.cursor = Some(pos);
        if !self.entries[vis[pos]].is_dir {
            self.file_name = name;
        }
    }

    /// Opens a folder, or picks the file(s).
    fn activate(&mut self, vis: &[usize], pos: usize, meta: &Session) -> Option<Browse> {
        let e = &self.entries[vis[pos]];
        if e.is_dir {
            let d = self.style.join(&self.dir, &e.name.clone());
            self.navigate(&d, meta);
            return None;
        }
        self.ok()
    }

    fn ok(&self) -> Option<Browse> {
        let paths = match self.mode {
            BrowseMode::Save if !self.file_name.trim().is_empty() => vec![self.style.join(&self.dir, self.file_name.trim())],
            BrowseMode::Save => return None,
            BrowseMode::Open => {
                let p: Vec<String> = self
                    .selected_paths()
                    .into_iter()
                    .filter(|p| self.entries.iter().any(|e| !e.is_dir && self.style.join(&self.dir, &e.name) == *p))
                    .collect();
                if p.is_empty() {
                    return None;
                }
                p
            }
        };
        Some(Browse::Picked { dir: self.dir.clone(), paths })
    }

    fn keys(&mut self, ctx: &egui::Context, vis: &[usize], meta: &Session) -> Option<Browse> {
        if ctx.memory(|m| m.focused().is_some()) {
            return None;
        }
        let (up, down, enter, back, space, all, home, end) = ctx.input_mut(|i| {
            (
                i.consume_key(Modifiers::NONE, Key::ArrowUp) || i.consume_key(Modifiers::SHIFT, Key::ArrowUp),
                i.consume_key(Modifiers::NONE, Key::ArrowDown) || i.consume_key(Modifiers::SHIFT, Key::ArrowDown),
                i.consume_key(Modifiers::NONE, Key::Enter),
                i.consume_key(Modifiers::NONE, Key::Backspace),
                i.consume_key(Modifiers::NONE, Key::Space),
                i.consume_key(Modifiers::COMMAND, Key::A),
                i.consume_key(Modifiers::NONE, Key::Home),
                i.consume_key(Modifiers::NONE, Key::End),
            )
        });
        let shift = ctx.input(|i| i.modifiers.shift);
        if vis.is_empty() {
            if back && let Some(p) = self.style.parent(&self.dir) {
                self.navigate(&p, meta);
            }
            return None;
        }
        let last = vis.len() - 1;
        let target = match (up, down, home, end) {
            (true, ..) => Some(self.cursor.map_or(0, |c| c.saturating_sub(1))),
            (_, true, ..) => Some(self.cursor.map_or(0, |c| (c + 1).min(last))),
            (_, _, true, _) => Some(0),
            (.., true) => Some(last),
            _ => None,
        };
        if let Some(t) = target {
            let mods = Modifiers { shift, ..Default::default() };
            self.click(vis, t, mods);
            self.scroll_to = Some(t);
        }
        if space && let Some(c) = self.cursor {
            self.click(vis, c, Modifiers::COMMAND);
        }
        if all && self.mode == BrowseMode::Open {
            self.selected = vis.iter().filter(|&&i| !self.entries[i].is_dir).map(|&i| self.entries[i].name.clone()).collect();
        }
        if back && let Some(p) = self.style.parent(&self.dir) {
            self.navigate(&p, meta);
            return None;
        }
        if enter && let Some(c) = self.cursor {
            return self.activate(vis, c, meta);
        }
        None
    }
}

mod view;

pub(super) use view::human_size;
