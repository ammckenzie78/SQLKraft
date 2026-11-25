//! Lets `sqlkraft --mcp` use the running SQLKraft window.
//!
//! The window listens on `127.0.0.1` (random port) and writes the port and a random token to
//! `mcp-bridge.json` in the config directory, which only the user can read. `sqlkraft --mcp`
//! reads that file, connects, sends the token, then forwards MCP traffic between Claude Code
//! (stdio) and the window. The window answers tools with the connections it has open, so
//! Claude sees exactly what the user sees, without SQLKraft ever storing a password.

use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::headless::{Connection, Headless};
use super::{ActiveQuery, Backend, ConnectionSummary, ExecOutput};
use crate::db::{Cell, ConnectionInfo, Repaint};
use crate::sql::Batch;

#[derive(Serialize, Deserialize)]
struct BridgeFile {
    port: u16,
    token: String,
    pid: u32,
}

fn bridge_path() -> Option<PathBuf> {
    crate::settings::config_dir().map(|d| d.join("mcp-bridge.json"))
}

/// A connection the window lends to Claude.
#[derive(Clone)]
pub struct GuiConnection {
    pub name: String,
    pub info: ConnectionInfo,
    pub writes_allowed: bool,
}

/// Requests from bridge threads to the UI thread.
pub enum GuiCall {
    Connections(Sender<Vec<GuiConnection>>),
    ActiveQuery(Sender<Option<ActiveQuery>>),
    OpenTab { connection: Option<String>, database: Option<String>, sql: String, reply: Sender<anyhow::Result<()>> },
    Log { tool: String, summary: String },
    ClientConnected,
    ClientDisconnected,
}

/// The listening side, owned by the window. Dropping it stops listening and removes the file.
pub struct Bridge {
    stop: Arc<AtomicBool>,
    token: String,
    pub port: u16,
}

impl Bridge {
    pub fn start(repaint: Repaint) -> std::io::Result<(Bridge, Receiver<GuiCall>)> {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let token = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
        write_private(&BridgeFile { port, token: token.clone(), pid: std::process::id() })?;

        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = channel();
        let (stop2, token2) = (stop.clone(), token.clone());
        std::thread::Builder::new().name("sqlkraft-mcp-bridge".into()).spawn(move || {
            while !stop2.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let (tx, token, repaint, stop) = (tx.clone(), token2.clone(), repaint.clone(), stop2.clone());
                        let _ = std::thread::Builder::new()
                            .name("sqlkraft-mcp-client".into())
                            .spawn(move || serve_client(stream, &token, tx, repaint, stop));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(100)),
                    Err(_) => std::thread::sleep(Duration::from_millis(500)),
                }
            }
        })?;
        Ok((Bridge { stop, token, port }, rx))
    }
}

impl Drop for Bridge {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Only remove the file if another SQLKraft window hasn't replaced it since.
        if let Some(p) = bridge_path() {
            let ours = std::fs::read_to_string(&p)
                .ok()
                .and_then(|s| serde_json::from_str::<BridgeFile>(&s).ok())
                .is_some_and(|f| f.token == self.token);
            if ours {
                let _ = std::fs::remove_file(p);
            }
        }
    }
}

fn write_private(file: &BridgeFile) -> std::io::Result<()> {
    let path = bridge_path().ok_or_else(|| std::io::Error::other("no config directory"))?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let json = serde_json::to_string(file).map_err(std::io::Error::other)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&path)?;
        f.write_all(json.as_bytes())?;
    }
    #[cfg(not(unix))]
    std::fs::write(&path, json)?;
    Ok(())
}

fn serve_client(stream: TcpStream, token: &str, tx: Sender<GuiCall>, repaint: Repaint, stop: Arc<AtomicBool>) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let Ok(read) = stream.try_clone() else { return };
    let mut reader = BufReader::new(read);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() || !constant_time_eq(line.trim().as_bytes(), token.as_bytes()) {
        return;
    }
    let mut writer = stream;
    if writer.write_all(b"ok\n").is_err() {
        return;
    }
    let _ = reader.get_ref().set_read_timeout(None);
    let send = |c: GuiCall| {
        let _ = tx.send(c);
        repaint();
    };
    send(GuiCall::ClientConnected);
    let mut backend = GuiBackend { tx: tx.clone(), repaint: repaint.clone(), inner: Headless::new(Vec::new()) };
    for line in reader.lines() {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str(&line) {
            Ok(msg) => super::handle(&mut backend, msg),
            Err(e) => Some(serde_json::json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": format!("Parse error: {e}") } })),
        };
        if let Some(reply) = reply {
            if serde_json::to_writer(&mut writer, &reply).is_err() || writer.write_all(b"\n").is_err() {
                break;
            }
        }
    }
    send(GuiCall::ClientDisconnected);
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Answers tools for one bridge client: connection lists and the editor come from the window;
/// queries run on this backend's own sessions, opened with the window's connection settings.
struct GuiBackend {
    tx: Sender<GuiCall>,
    repaint: Repaint,
    inner: Headless,
}

impl GuiBackend {
    fn ask<T>(&self, make: impl FnOnce(Sender<T>) -> GuiCall) -> anyhow::Result<T> {
        let (reply, rx) = channel();
        self.tx.send(make(reply)).map_err(|_| anyhow::anyhow!("The SQLKraft window has closed"))?;
        (self.repaint)();
        rx.recv_timeout(Duration::from_secs(10)).map_err(|_| anyhow::anyhow!("The SQLKraft window did not answer"))
    }

    fn sync(&mut self) {
        if let Ok(list) = self.ask(GuiCall::Connections) {
            self.inner.set_connections(
                list.into_iter().map(|c| Connection { name: c.name, info: c.info, writes_allowed: c.writes_allowed }).collect(),
            );
        }
    }
}

impl Backend for GuiBackend {
    fn connections(&mut self) -> Vec<ConnectionSummary> {
        self.sync();
        self.inner.connections()
    }

    fn fetch(&mut self, connection: &str, sql: &str) -> anyhow::Result<Vec<Vec<Cell>>> {
        self.sync();
        self.inner.fetch(connection, sql)
    }

    fn execute(&mut self, connection: &str, database: Option<&str>, batches: Vec<Batch>, max_rows: usize) -> anyhow::Result<ExecOutput> {
        self.sync();
        self.inner.execute(connection, database, batches, max_rows)
    }

    fn writes_allowed(&self, connection: &str) -> bool {
        self.inner.writes_allowed(connection)
    }

    fn server_version(&mut self, connection: &str) -> (String, i32) {
        self.inner.server_version(connection)
    }

    fn active_query(&mut self) -> anyhow::Result<Option<ActiveQuery>> {
        self.ask(GuiCall::ActiveQuery)
    }

    fn open_query_tab(&mut self, connection: Option<&str>, database: Option<&str>, sql: &str) -> anyhow::Result<()> {
        let (connection, database, sql) = (connection.map(String::from), database.map(String::from), sql.to_string());
        self.ask(|reply| GuiCall::OpenTab { connection, database, sql, reply })?
    }

    fn log(&mut self, tool: &str, summary: &str) {
        let _ = self.tx.send(GuiCall::Log { tool: tool.into(), summary: summary.into() });
        (self.repaint)();
    }
}

/// The `sqlkraft --mcp` side: forwards stdio to a running window. Returns false, having done
/// nothing, when no window is listening.
pub fn proxy_to_window() -> bool {
    let Some(file) = bridge_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<BridgeFile>(&s).ok())
    else {
        return false;
    };
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, file.port));
    let Ok(stream) = TcpStream::connect_timeout(&addr, Duration::from_secs(1)) else { return false };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut writer = match stream.try_clone() {
        Ok(w) => w,
        Err(_) => return false,
    };
    if writer.write_all(format!("{}\n", file.token).as_bytes()).is_err() {
        return false;
    }
    let mut reader = BufReader::new(stream);
    let mut ok = String::new();
    if reader.read_line(&mut ok).is_err() || ok.trim() != "ok" {
        return false;
    }
    let _ = reader.get_ref().set_read_timeout(None);

    // stdin -> window
    std::thread::spawn(move || {
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            let Ok(line) = line else { break };
            if writer.write_all(line.as_bytes()).is_err() || writer.write_all(b"\n").is_err() {
                break;
            }
        }
        let _ = writer.shutdown(std::net::Shutdown::Write);
    });
    // window -> stdout
    let mut stdout = std::io::stdout().lock();
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if stdout.write_all(line.as_bytes()).is_err() || stdout.write_all(b"\n").is_err() || stdout.flush().is_err() {
            break;
        }
    }
    true
}
