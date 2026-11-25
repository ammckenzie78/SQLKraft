//! A Model Context Protocol server, so Claude Code (or any MCP client) can work with SQLKraft.
//!
//! `sqlkraft --mcp` speaks MCP over stdio (newline-delimited JSON-RPC 2.0). The user adds it to
//! their own Claude Code or Claude Desktop, so the Claude subscription is only ever used inside
//! Anthropic's apps; SQLKraft only provides tools.
//!
//! The tools run against a [`Backend`]:
//! - [`headless::Headless`] opens its own connections (used when the SQLKraft window isn't
//!   running, and by the tests).
//!
//! Safety: queries are read-only unless the user allowed writes for that connection
//! ([`guard`]), result sizes are capped, and credentials never pass through MCP; tools refer to
//! connections by name.

pub mod bridge;
pub mod guard;
pub mod headless;
pub mod tools;

use std::io::{BufRead, Write};

use serde_json::{Value, json};

use crate::db::{Cell, ColumnMeta};

/// The newest protocol revision this server implements. Older clients get their own version
/// echoed back, as the spec allows.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// A connection the tools can use.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ConnectionSummary {
    /// What tools take as `connection`.
    pub name: String,
    pub server: String,
    pub login: String,
    /// `SERVERPROPERTY('ProductVersion')`, when connected.
    pub version: String,
    pub writes_allowed: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ResultSet {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Cell>>,
    /// More rows existed than `max_rows`; the query was stopped there.
    pub truncated: bool,
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ExecOutput {
    pub result_sets: Vec<ResultSet>,
    /// PRINT output, row counts and errors, as in the Messages tab.
    pub messages: Vec<String>,
    pub had_errors: bool,
}

impl ExecOutput {
    pub(crate) fn push_columns(&mut self, cols: &[ColumnMeta]) {
        self.result_sets.push(ResultSet { columns: cols.iter().map(|c| c.name.clone()).collect(), rows: Vec::new(), truncated: false });
    }
}

/// The query editor tab the user is looking at in the SQLKraft window.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ActiveQuery {
    pub title: String,
    pub connection: Option<String>,
    pub database: Option<String>,
    pub text: String,
    /// The selected text, when there is a selection.
    pub selection: Option<String>,
}

/// Where the tools get their data. All calls block; the server handles one request at a time.
pub trait Backend {
    fn connections(&mut self) -> Vec<ConnectionSummary>;

    /// First result set of `sql`, run through the Object Explorer path of `connection`.
    fn fetch(&mut self, connection: &str, sql: &str) -> anyhow::Result<Vec<Vec<Cell>>>;

    /// Runs batches like a query tab would, in `database` if given. Stops collecting a result
    /// set after `max_rows` rows.
    fn execute(
        &mut self,
        connection: &str,
        database: Option<&str>,
        batches: Vec<crate::sql::Batch>,
        max_rows: usize,
    ) -> anyhow::Result<ExecOutput>;

    fn writes_allowed(&self, connection: &str) -> bool;

    /// Server version and engine edition, for version-aware scripts.
    fn server_version(&mut self, connection: &str) -> (String, i32);

    fn active_query(&mut self) -> anyhow::Result<Option<ActiveQuery>> {
        anyhow::bail!("Needs the SQLKraft window: open SQLKraft and enable Claude Code access.")
    }

    fn open_query_tab(&mut self, _connection: Option<&str>, _database: Option<&str>, _sql: &str) -> anyhow::Result<()> {
        anyhow::bail!("Needs the SQLKraft window: open SQLKraft and enable Claude Code access.")
    }

    /// Called for every tool call, so the window can show what Claude did.
    fn log(&mut self, _tool: &str, _summary: &str) {}
}

/// Serves MCP on `input`/`output` until `input` closes.
pub fn serve(backend: &mut dyn Backend, input: impl BufRead, mut output: impl Write) -> std::io::Result<()> {
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(Value::Array(batch)) => {
                let replies: Vec<Value> = batch.into_iter().filter_map(|m| handle(backend, m)).collect();
                (!replies.is_empty()).then_some(Value::Array(replies))
            }
            Ok(msg) => handle(backend, msg),
            Err(e) => Some(error(Value::Null, -32700, &format!("Parse error: {e}"))),
        };
        if let Some(reply) = reply {
            serde_json::to_writer(&mut output, &reply)?;
            output.write_all(b"\n")?;
            output.flush()?;
        }
    }
    Ok(())
}

/// Handles one JSON-RPC message. Notifications get no reply.
pub fn handle(backend: &mut dyn Backend, msg: Value) -> Option<Value> {
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(Value::as_str).unwrap_or_default();
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let id = id?; // notifications (initialized, cancelled, ...) need no answer
    let result = match method {
        "initialize" => {
            let requested = params.get("protocolVersion").and_then(Value::as_str).unwrap_or(PROTOCOL_VERSION);
            let version = if requested < PROTOCOL_VERSION { requested } else { PROTOCOL_VERSION };
            json!({
                "protocolVersion": version,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": "sqlkraft", "title": "SQLKraft", "version": env!("CARGO_PKG_VERSION") },
                "instructions": tools::INSTRUCTIONS,
            })
        }
        "ping" => json!({}),
        "tools/list" => json!({ "tools": tools::definitions() }),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or_default();
            let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            match tools::call(backend, name, &args) {
                Ok(text) => json!({ "content": [{ "type": "text", "text": text }], "isError": false }),
                Err(tools::CallError::UnknownTool) => return Some(error(id, -32602, &format!("Unknown tool: {name}"))),
                Err(tools::CallError::Failed(e)) => {
                    json!({ "content": [{ "type": "text", "text": format!("{e:#}") }], "isError": true })
                }
            }
        }
        _ => return Some(error(id, -32601, &format!("Method not found: {method}"))),
    };
    Some(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}
