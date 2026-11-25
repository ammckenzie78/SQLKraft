//! The MCP server against a live server, driven through JSON-RPC like Claude Code would.

mod common;

use common::Fixture;
use serde_json::{Value, json};
use sqlkraft::mcp::{self, Backend, headless::{Connection, Headless}};

const CONN: &str = "test";

fn server(f: &Fixture, writes_allowed: bool) -> Headless {
    Headless::new(vec![Connection { name: CONN.into(), info: f.info.clone(), writes_allowed }])
}

fn rpc(b: &mut Headless, method: &str, params: Value) -> Value {
    let reply = mcp::handle(b, json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params })).expect("a reply");
    assert!(reply.get("error").is_none(), "{method}: {reply}");
    reply["result"].clone()
}

/// Calls a tool; returns (text, is_error).
fn tool(b: &mut Headless, name: &str, args: Value) -> (String, bool) {
    let r = rpc(b, "tools/call", json!({ "name": name, "arguments": args }));
    (r["content"][0]["text"].as_str().unwrap_or_default().to_string(), r["isError"].as_bool().unwrap_or(true))
}

fn ok_json(b: &mut Headless, name: &str, args: Value) -> Value {
    let (text, err) = tool(b, name, args);
    assert!(!err, "{name} failed: {text}");
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{name} returned non-JSON ({e}): {text}"))
}

#[test]
fn handshake_and_schema_tools() {
    let f = Fixture::new(live_or_skip!());
    let db = f.db.as_str();
    let mut b = server(&f, false);

    let init = rpc(&mut b, "initialize", json!({ "protocolVersion": "2025-03-26", "capabilities": {}, "clientInfo": { "name": "test", "version": "0" } }));
    assert_eq!(init["protocolVersion"], "2025-03-26", "older clients get their version back");
    assert_eq!(init["serverInfo"]["name"], "sqlkraft");
    assert!(mcp::handle(&mut b, json!({ "jsonrpc": "2.0", "method": "notifications/initialized" })).is_none());
    assert_eq!(rpc(&mut b, "tools/list", json!({}))["tools"].as_array().unwrap().len(), 8);

    let conns = ok_json(&mut b, "list_connections", json!({}));
    assert_eq!(conns[0]["name"], CONN);

    let dbs = ok_json(&mut b, "list_databases", json!({ "connection": CONN }));
    assert!(dbs.as_array().unwrap().iter().any(|d| d["name"] == db), "{dbs}");

    let objs = ok_json(&mut b, "list_objects", json!({ "connection": CONN, "database": db }));
    let names: Vec<String> = objs.as_array().unwrap().iter().map(|o| format!("{}.{}:{}", o["schema"].as_str().unwrap(), o["name"].as_str().unwrap(), o["type"].as_str().unwrap())).collect();
    assert_eq!(
        names,
        [
            "dbo.ActiveCustomers:view", "dbo.Customers:table", "dbo.GetCustomer:procedure", "dbo.Twice:function",
            "sales.LinesFor:function", "sales.Odd]Name Proc:procedure", "sales.Order Lines:table"
        ]
    );
    let procs = ok_json(&mut b, "list_objects", json!({ "connection": CONN, "database": db, "type": "procedure", "schema": "dbo" }));
    assert_eq!(procs.as_array().unwrap().len(), 1);

    let t = ok_json(&mut b, "describe_object", json!({ "connection": CONN, "database": db, "object": "Customers" }));
    assert_eq!(t["columns"][0]["name"], "Id");
    assert_eq!(t["columns"][0]["identity"], true);
    assert_eq!(t["columns"][3]["type"], "decimal(18,2)");
    assert_eq!(t["columns"][6]["computed"], "(upper([Name]))");
    assert_eq!(t["approximate_rows"], "2");
    let pk = t["indexes"].as_array().unwrap().iter().find(|i| i["primary_key"] == true).unwrap();
    assert_eq!(pk["keys"], json!(["Id"]));

    let lines = ok_json(&mut b, "describe_object", json!({ "connection": CONN, "database": db, "object": "[sales].[Order Lines]" }));
    let pk = lines["indexes"].as_array().unwrap().iter().find(|i| i["primary_key"] == true).unwrap();
    assert_eq!(pk["keys"], json!(["OrderId", "Line]No DESC"]));

    let p = ok_json(&mut b, "describe_object", json!({ "connection": CONN, "database": db, "object": "dbo.GetCustomer" }));
    assert_eq!(p["parameters"][0]["name"], "@Id");
    assert_eq!(p["parameters"][0]["type"], "int");

    let (def, err) = tool(&mut b, "get_definition", json!({ "connection": CONN, "database": db, "object": "dbo.GetCustomer", "mode": "alter" }));
    assert!(!err && def.contains("CREATE OR ALTER PROCEDURE dbo.GetCustomer"), "{def}");
    let (def, err) = tool(&mut b, "get_definition", json!({ "connection": CONN, "database": db, "object": "[sales].[Order Lines]" }));
    assert!(!err && def.contains("CREATE TABLE [sales].[Order Lines]"), "{def}");

    let (msg, err) = tool(&mut b, "describe_object", json!({ "connection": CONN, "database": db, "object": "dbo.Nope" }));
    assert!(err && msg.contains("No object"), "{msg}");
    let (msg, err) = tool(&mut b, "get_active_query", json!({}));
    assert!(err && msg.contains("SQLKraft window"), "{msg}");
}

#[test]
fn run_query_is_read_only_by_default() {
    let f = Fixture::new(live_or_skip!());
    let db = f.db.as_str();
    let mut b = server(&f, false);

    let out = ok_json(&mut b, "run_query", json!({ "connection": CONN, "database": db, "sql": "SELECT Name FROM dbo.Customers ORDER BY Id; PRINT 'hi'" }));
    assert_eq!(out["result_sets"][0]["rows"], json!([["Ada"], ["Brian"]]));
    assert!(out["messages"].as_array().unwrap().iter().any(|m| m == "hi"), "{out}");

    let (msg, err) = tool(&mut b, "run_query", json!({ "connection": CONN, "database": db, "sql": "DELETE FROM dbo.Customers" }));
    assert!(err && msg.contains("`DELETE`"), "{msg}");

    // A row cap stops the query, and the session and its transaction state stay clean.
    let out = ok_json(&mut b, "run_query", json!({
        "connection": CONN, "database": db, "max_rows": 5,
        "sql": "SELECT a.object_id FROM sys.all_objects a CROSS JOIN sys.all_objects b"
    }));
    assert_eq!(out["result_sets"][0]["rows"].as_array().unwrap().len(), 5);
    assert_eq!(out["result_sets"][0]["truncated"], true);
    assert_eq!(out["had_errors"], false, "{out}");
    // Outside the read-only wrapper, the session has no transaction left open.
    let rows = Backend::fetch(&mut b, CONN, "SELECT @@TRANCOUNT").unwrap();
    assert_eq!(rows[0][0].as_deref(), Some("0"));

    // Errors come back as messages, not a broken connection.
    let out = ok_json(&mut b, "run_query", json!({ "connection": CONN, "database": db, "sql": "SELECT * FROM dbo.Missing" }));
    assert_eq!(out["had_errors"], true);
    assert!(out["messages"][0].as_str().unwrap().contains("Invalid object name"), "{out}");
}

#[test]
fn run_query_can_write_when_allowed() {
    let f = Fixture::new(live_or_skip!());
    let db = f.db.as_str();
    let mut b = server(&f, true);
    let out = ok_json(&mut b, "run_query", json!({ "connection": CONN, "database": db, "sql": "INSERT dbo.Customers (Name, Email) VALUES (N'Cy', 'cy@example.com')" }));
    assert_eq!(out["had_errors"], false, "{out}");
    let rows = f.fetch(&format!("SELECT COUNT(*) FROM [{db}].dbo.Customers"));
    assert_eq!(rows[0][0].as_deref(), Some("3"));
}

/// The window side of `sqlkraft --mcp`: token check, then tools answered with the window's
/// connections and editor.
#[test]
fn window_bridge_serves_tools() {
    use sqlkraft::mcp::ActiveQuery;
    use sqlkraft::mcp::bridge::{Bridge, GuiCall, GuiConnection};
    use std::io::{BufRead, BufReader, Write};

    let f = Fixture::new(live_or_skip!());
    let dir = std::env::temp_dir().join(format!("sqlkraft-bridge-test-{}", std::process::id()));
    // SAFETY: no other test in this binary reads the environment concurrently with this write.
    unsafe { std::env::set_var("SQLKRAFT_CONFIG_DIR", &dir) };
    let (bridge, calls) = Bridge::start(std::sync::Arc::new(|| {})).unwrap();

    // A stand-in for the UI thread.
    let info = f.info.clone();
    let db = f.db.clone();
    let ui = std::thread::spawn(move || {
        let mut logged = Vec::new();
        while let Ok(call) = calls.recv() {
            match call {
                GuiCall::Connections(reply) => {
                    let _ = reply.send(vec![GuiConnection { name: "gui".into(), info: info.clone(), writes_allowed: false }]);
                }
                GuiCall::ActiveQuery(reply) => {
                    let _ = reply.send(Some(ActiveQuery {
                        title: "SQLQuery1.sql".into(), connection: Some("gui".into()), database: Some(db.clone()),
                        text: "SELECT 1".into(), selection: None,
                    }));
                }
                GuiCall::Log { tool, .. } => logged.push(tool),
                GuiCall::ClientDisconnected => break,
                _ => {}
            }
        }
        logged
    });

    let file: Value = serde_json::from_str(&std::fs::read_to_string(dir.join("mcp-bridge.json")).unwrap()).unwrap();
    let port = file["port"].as_u64().unwrap() as u16;
    let token = file["token"].as_str().unwrap().to_string();

    // A wrong token gets no "ok".
    let bad = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    (&bad).write_all(b"nope\n").unwrap();
    let mut line = String::new();
    BufReader::new(&bad).read_line(&mut line).unwrap();
    assert_eq!(line, "", "a bad token must be refused");

    let stream = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    let mut w = stream.try_clone().unwrap();
    let mut r = BufReader::new(stream);
    w.write_all(format!("{token}\n").as_bytes()).unwrap();
    line.clear();
    r.read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "ok");

    let mut call = |id: u64, name: &str, args: Value| -> Value {
        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call", "params": { "name": name, "arguments": args } });
        w.write_all(format!("{msg}\n").as_bytes()).unwrap();
        let mut line = String::new();
        r.read_line(&mut line).unwrap();
        let reply: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(reply["result"]["isError"], false, "{name}: {reply}");
        serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
    };
    assert_eq!(call(1, "list_connections", json!({}))[0]["name"], "gui");
    let out = call(2, "run_query", json!({ "connection": "gui", "database": f.db, "sql": "SELECT COUNT(*) FROM dbo.Customers" }));
    assert_eq!(out["result_sets"][0]["rows"], json!([["2"]]));
    assert_eq!(call(3, "get_active_query", json!({}))["text"], "SELECT 1");

    drop(w);
    drop(r);
    let logged = ui.join().unwrap();
    assert_eq!(logged, ["list_connections", "run_query", "get_active_query"]);
    drop(bridge);
    assert!(!dir.join("mcp-bridge.json").exists(), "bridge file removed on drop");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn stdio_transport_round_trip() {
    let f = Fixture::new(live_or_skip!());
    let mut b = server(&f, false);
    let input = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#, "\n",
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#, "\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"run_query","arguments":{"connection":"test","sql":"SELECT 40 + 2"}}}"#, "\n",
        "not json\n",
        r#"{"jsonrpc":"2.0","id":3,"method":"nope"}"#, "\n",
    );
    let mut output = Vec::new();
    mcp::serve(&mut b, input.as_bytes(), &mut output).unwrap();
    let replies: Vec<Value> = String::from_utf8(output).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(replies.len(), 4, "{replies:?}");
    assert_eq!(replies[0]["id"], 1);
    assert!(replies[1]["result"]["content"][0]["text"].as_str().unwrap().contains("\"42\""), "{}", replies[1]);
    assert_eq!(replies[2]["error"]["code"], -32700);
    assert_eq!(replies[3]["error"]["code"], -32601);
}
