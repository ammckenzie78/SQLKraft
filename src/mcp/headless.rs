//! A [`Backend`] that opens its own connections, for `sqlkraft --mcp` without the window and
//! for tests.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow, bail};

use super::{Backend, ConnectionSummary, ExecOutput};
use crate::db::{Cell, ConnectionInfo, Event, MsgKind, Session};
use crate::sql::{Batch, quote_ident};

/// How long a query may run before it is cancelled.
const QUERY_TIMEOUT: Duration = Duration::from_secs(120);

pub struct Connection {
    pub name: String,
    pub info: ConnectionInfo,
    pub writes_allowed: bool,
}

struct Open {
    session: Session,
    version: String,
    edition: i32,
    login: String,
}

pub struct Headless {
    rt: tokio::runtime::Runtime,
    conns: Vec<Connection>,
    open: HashMap<String, Open>,
}

impl Headless {
    pub fn new(conns: Vec<Connection>) -> Self {
        crate::capture::install();
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("sqlkraft-mcp")
            .build()
            .expect("failed to start tokio runtime");
        Self { rt, conns, open: HashMap::new() }
    }

    /// Recent connections from the settings file that can connect without a stored password:
    /// integrated authentication, or any login when `SQLKRAFT_MCP_PASSWORD` is set.
    pub fn from_settings(settings: &crate::settings::Settings) -> Self {
        let password = std::env::var("SQLKRAFT_MCP_PASSWORD").ok();
        let conns = settings
            .recent
            .iter()
            .filter(|c| !c.auth.needs_password() || password.is_some())
            .map(|c| {
                let mut info = c.clone();
                if let Some(p) = &password {
                    info.password = p.clone();
                }
                Connection { name: connection_name(&info), info, writes_allowed: false }
            })
            .collect();
        Self::new(conns)
    }

    /// Replaces the connection list, closing sessions whose settings changed or went away.
    pub fn set_connections(&mut self, conns: Vec<Connection>) {
        self.open.retain(|name, _| {
            let old = self.conns.iter().find(|c| &c.name == name).map(|c| &c.info);
            let new = conns.iter().find(|c| &c.name == name).map(|c| &c.info);
            old.is_some() && old == new
        });
        self.conns = conns;
    }

    fn session(&mut self, name: &str) -> anyhow::Result<&mut Open> {
        if !self.open.contains_key(name) {
            let conn = self
                .conns
                .iter()
                .find(|c| c.name == name)
                .ok_or_else(|| anyhow!("No connection named `{name}`. Use list_connections."))?;
            let session = Session::spawn(self.rt.handle(), conn.info.clone(), std::sync::Arc::new(|| {}));
            let start = Instant::now();
            let facts = loop {
                match session.events.recv_timeout(Duration::from_millis(200)) {
                    Ok(Event::Connected(f)) => break f,
                    Ok(Event::ConnectFailed(e)) => bail!("Could not connect to `{name}`: {e}"),
                    Ok(_) => {}
                    Err(_) if start.elapsed() > Duration::from_secs(conn.info.connect_timeout_secs + 5) => {
                        bail!("Timed out connecting to `{name}`")
                    }
                    Err(_) => {}
                }
            };
            self.open.insert(
                name.to_string(),
                Open { session, version: facts.product_version, edition: facts.engine_edition, login: facts.login },
            );
        }
        Ok(self.open.get_mut(name).unwrap())
    }
}

fn is_deadlock(e: &anyhow::Error) -> bool {
    let text = format!("{e:#}");
    text.contains("Msg 1205,") || text.contains("code: 1205") || text.contains("deadlock victim")
}

/// `server (login)`, the name tools use for a connection.
pub fn connection_name(info: &ConnectionInfo) -> String {
    format!("{} ({})", info.server, info.display_user())
}

impl Backend for Headless {
    fn connections(&mut self) -> Vec<ConnectionSummary> {
        self.conns
            .iter()
            .map(|c| {
                let open = self.open.get(&c.name);
                ConnectionSummary {
                    name: c.name.clone(),
                    server: c.info.server.clone(),
                    login: open.map(|o| o.login.clone()).unwrap_or_else(|| c.info.display_user()),
                    version: open.map(|o| o.version.clone()).unwrap_or_default(),
                    writes_allowed: c.writes_allowed,
                }
            })
            .collect()
    }

    fn fetch(&mut self, connection: &str, sql: &str) -> anyhow::Result<Vec<Vec<Cell>>> {
        let rt = self.rt.handle().clone();
        // Catalog reads can lose a deadlock to concurrent CREATE/DROP DATABASE (error 1205);
        // they change nothing, so retrying is safe.
        for attempt in 1.. {
            let open = self.session(connection)?;
            let rx = open.session.fetch(sql);
            let reply = rt
                .block_on(async { tokio::time::timeout(QUERY_TIMEOUT, rx).await })
                .map_err(|_| anyhow!("Query timed out"))?
                .context("Connection closed")?;
            match reply {
                Err(e) if attempt < 4 && is_deadlock(&e) => {
                    std::thread::sleep(Duration::from_millis(100 * attempt));
                }
                r => return Ok(r?.1),
            }
        }
        unreachable!()
    }

    fn execute(&mut self, connection: &str, database: Option<&str>, batches: Vec<Batch>, max_rows: usize) -> anyhow::Result<ExecOutput> {
        let open = self.session(connection)?;
        let session = &mut open.session;
        while session.events.try_recv().is_ok() {}
        let mut all = Vec::new();
        if let Some(db) = database {
            all.push(Batch { text: format!("USE {}", quote_ident(db)), start_line: 0, repeat: 1 });
        }
        all.extend(batches);
        session.execute(all);
        collect(session, max_rows)
    }

    fn writes_allowed(&self, connection: &str) -> bool {
        self.conns.iter().any(|c| c.name == connection && c.writes_allowed)
    }

    fn server_version(&mut self, connection: &str) -> (String, i32) {
        match self.session(connection) {
            Ok(o) => (o.version.clone(), o.edition),
            Err(_) => (String::new(), 0),
        }
    }
}

/// Collects one execution's events into an [`ExecOutput`], cancelling once a result set has
/// more than `max_rows` rows.
pub fn collect(session: &mut Session, max_rows: usize) -> anyhow::Result<ExecOutput> {
    let mut out = ExecOutput::default();
    let start = Instant::now();
    let mut truncated = false;
    loop {
        let ev = match session.events.recv_timeout(Duration::from_millis(200)) {
            Ok(e) => e,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                if start.elapsed() > QUERY_TIMEOUT && !truncated {
                    truncated = true;
                    out.messages.push(format!("Stopped after {}s.", QUERY_TIMEOUT.as_secs()));
                    session.cancel();
                }
                continue;
            }
            Err(_) => bail!("Connection closed"),
        };
        match ev {
            Event::ResultSet(cols) => out.push_columns(&cols),
            Event::Rows(rows) => {
                let Some(set) = out.result_sets.last_mut() else { continue };
                let room = max_rows.saturating_sub(set.rows.len());
                if rows.len() > room {
                    set.rows.extend(rows.into_iter().take(room));
                    if !set.truncated {
                        set.truncated = true;
                        truncated = true;
                        session.cancel();
                    }
                } else {
                    set.rows.extend(rows);
                }
            }
            Event::Message(m) => {
                // Our own cancel after max_rows is not an error worth reporting.
                if truncated && m.text == "Query was cancelled by user." {
                    continue;
                }
                if m.kind == MsgKind::Error {
                    out.had_errors = true;
                }
                let text = m.text.trim();
                // Skip the noise from the `USE` we add for `database`, and the footer.
                if !text.is_empty() && !text.starts_with("Completion time:") && !text.starts_with("Changed database context to") {
                    out.messages.push(text.to_string());
                }
            }
            Event::ExecFinished { .. } => return Ok(out),
            Event::ConnectFailed(e) => bail!("Connection failed: {e}"),
            _ => {}
        }
    }
}
