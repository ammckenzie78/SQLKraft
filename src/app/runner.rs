//! Shared plumbing for the maintenance dialogs: one-shot metadata fetches, and long-running
//! commands (shrink, backup, restore) on their own connection with progress and Cancel.

use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText};
use tokio::sync::oneshot;

use crate::backup::{self as b, ServerFolders};
use crate::db::{Cell, ColumnMeta, ConnectionInfo, Event, FetchResult, Message, MsgKind, Repaint, Session};
use crate::maintenance;
use crate::sql;

pub(super) type Rows = (Vec<ColumnMeta>, Vec<Vec<Cell>>);

/// A metadata query in flight (or none).
#[derive(Default)]
pub(super) struct Pending(Option<oneshot::Receiver<FetchResult>>);

impl Pending {
    pub(super) fn new(rx: oneshot::Receiver<FetchResult>) -> Self {
        Self(Some(rx))
    }

    pub(super) fn busy(&self) -> bool {
        self.0.is_some()
    }

    /// The result once it has arrived; errors are formatted for display.
    pub(super) fn poll(&mut self) -> Option<Result<Rows, String>> {
        let rx = self.0.as_mut()?;
        let r = match rx.try_recv() {
            Ok(r) => r.map_err(|e| format!("{e:#}")),
            Err(oneshot::error::TryRecvError::Empty) => return None,
            Err(oneshot::error::TryRecvError::Closed) => Err("Connection closed".into()),
        };
        self.0 = None;
        Some(r)
    }
}

/// Drops a metadata session's events, returning a connection failure if there was one.
pub(super) fn drain_meta(session: &Session) -> Option<String> {
    let mut err = None;
    while let Ok(ev) = session.events.try_recv() {
        if let Event::ConnectFailed(e) = ev {
            err = Some(e);
        }
    }
    err
}

#[derive(Debug, Clone, Copy)]
pub(super) struct RunEnd {
    pub(super) cancelled: bool,
    pub(super) had_errors: bool,
    pub(super) elapsed: Duration,
}

/// A script running on its own connection.
pub(super) struct Runner {
    session: Session,
    spid: Option<i32>,
    pub(super) messages: Vec<Message>,
    /// 0-100, from `STATS` messages or `sys.dm_exec_requests.percent_complete`.
    pub(super) percent: Option<f32>,
    pub(super) end: Option<RunEnd>,
    started: Instant,
    progress: Pending,
    last_poll: Instant,
}

impl Runner {
    pub(super) fn start(rt: &tokio::runtime::Handle, info: ConnectionInfo, repaint: Repaint, script: &str) -> Self {
        let mut session = Session::spawn(rt, info, repaint);
        session.execute(sql::split_batches(script, 0));
        Self {
            session,
            spid: None,
            messages: Vec::new(),
            percent: None,
            end: None,
            started: Instant::now(),
            progress: Pending::default(),
            last_poll: Instant::now(),
        }
    }

    pub(super) fn running(&self) -> bool {
        self.end.is_none()
    }

    pub(super) fn cancel(&mut self) {
        self.session.cancel();
    }

    /// Reads the connection's events; `meta` polls `percent_complete` when the command does
    /// not report progress itself.
    pub(super) fn pump(&mut self, meta: &Session, ctx: &egui::Context) {
        while let Ok(ev) = self.session.events.try_recv() {
            match ev {
                Event::Connected(f) => self.spid = Some(f.spid),
                Event::ConnectFailed(e) => {
                    self.messages.push(Message::error(e));
                    self.end = Some(RunEnd { cancelled: false, had_errors: true, elapsed: self.started.elapsed() });
                }
                Event::Message(m) => {
                    if m.kind == MsgKind::Info
                        && let Some(p) = stats_percent(&m.text)
                    {
                        self.percent = Some(p);
                    }
                    self.messages.push(m);
                }
                Event::ExecFinished { elapsed, cancelled, had_errors } => {
                    self.end = Some(RunEnd { cancelled, had_errors, elapsed });
                }
                Event::Disconnected(e) => self.messages.push(Message::error(e)),
                _ => {}
            }
        }
        if !self.running() {
            return;
        }
        if let Some(Ok((_, rows))) = self.progress.poll() {
            let p = rows.first().and_then(|r| r.first().cloned().flatten()).and_then(|v| v.parse::<f32>().ok());
            if let Some(p) = p.filter(|p| *p > 0.0) {
                self.percent = Some(p);
            }
        }
        if let Some(spid) = self.spid
            && !self.progress.busy()
            && self.last_poll.elapsed() > Duration::from_millis(1000)
        {
            self.progress = Pending::new(meta.fetch(maintenance::progress_query(spid)));
            self.last_poll = Instant::now();
        }
        ctx.request_repaint_after(Duration::from_millis(500));
    }

    /// Progress bar, status line and the server's messages.
    pub(super) fn ui(&self, ui: &mut egui::Ui) {
        let elapsed = self.end.map(|e| e.elapsed).unwrap_or_else(|| self.started.elapsed()).as_secs();
        let clock = format!("{:02}:{:02}:{:02}", elapsed / 3600, (elapsed / 60) % 60, elapsed % 60);
        match self.end {
            None => {
                let pct = self.percent.unwrap_or(0.0);
                let text = match self.percent {
                    Some(p) => format!("{p:.0}%  ·  {clock}"),
                    None => format!("Running…  {clock}"),
                };
                ui.add(egui::ProgressBar::new(pct / 100.0).text(text).animate(self.percent.is_none()));
            }
            Some(e) if e.cancelled => {
                ui.colored_label(Color32::from_rgb(220, 140, 60), format!("Cancelled after {clock}."));
            }
            Some(e) if e.had_errors => {
                ui.colored_label(Color32::from_rgb(220, 80, 80), format!("⚠ Finished with errors after {clock}."));
            }
            Some(_) => {
                ui.colored_label(Color32::from_rgb(80, 180, 80), format!("✔ Completed in {clock}."));
            }
        }
        let shown: Vec<&Message> = self.messages.iter().filter(|m| !m.text.trim().is_empty()).collect();
        if !shown.is_empty() {
            egui::ScrollArea::vertical().max_height(110.0).stick_to_bottom(true).id_salt("run_messages").show(ui, |ui| {
                for m in shown {
                    let text = RichText::new(m.text.trim()).monospace().small();
                    match m.kind {
                        MsgKind::Error => ui.colored_label(Color32::from_rgb(220, 80, 80), text),
                        MsgKind::Info => ui.label(text),
                    };
                }
            });
        }
    }
}

/// Default folders and path style of a server, loaded once per dialog.
pub(super) struct FolderLoader {
    paths: Pending,
    host: Pending,
    row: Option<Rows>,
    platform: Option<Option<String>>,
    pub(super) folders: Option<ServerFolders>,
    pub(super) error: Option<String>,
}

impl FolderLoader {
    pub(super) fn new(meta: &Session) -> Self {
        Self {
            paths: Pending::new(meta.fetch(b::SERVER_PATHS_QUERY)),
            host: Pending::new(meta.fetch(b::HOST_PLATFORM_QUERY)),
            row: None,
            platform: None,
            folders: None,
            error: None,
        }
    }

    /// Returns true when the folders have just become available.
    pub(super) fn poll(&mut self) -> bool {
        match self.paths.poll() {
            Some(Ok(r)) => self.row = Some(r),
            Some(Err(e)) => self.error = Some(e),
            None => {}
        }
        if let Some(r) = self.host.poll() {
            self.platform = Some(r.ok().and_then(|(_, rows)| rows.first().and_then(|r| r.first().cloned().flatten())));
        }
        if self.folders.is_none()
            && self.platform.is_some()
            && let Some((_, rows)) = &self.row
        {
            let row = rows.first().cloned().unwrap_or_default();
            self.folders = Some(b::parse_server_folders(&row, self.platform.clone().flatten().as_deref()));
            return true;
        }
        false
    }
}

/// `10 percent processed.` (BACKUP / RESTORE `WITH STATS`) as a number.
pub(super) fn stats_percent(text: &str) -> Option<f32> {
    let t = text.trim();
    let n = t.strip_suffix("percent processed.")?.trim();
    n.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::stats_percent;

    #[test]
    fn stats_messages() {
        assert_eq!(stats_percent("10 percent processed."), Some(10.0));
        assert_eq!(stats_percent(" 100 percent processed.\n"), Some(100.0));
        assert_eq!(stats_percent("Processed 360 pages for database"), None);
    }
}
