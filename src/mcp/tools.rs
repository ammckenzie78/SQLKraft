//! The MCP tools: their JSON schemas and implementations.

use anyhow::{Context as _, anyhow, bail};
use serde_json::{Value, json};

use super::{Backend, guard};
use crate::db::Cell;
use crate::explorer::{self, NodeKind};
use crate::scripting::{self, ScriptMode, Syntax};
use crate::sql::{quote_ident, quote_literal, split_batches};

pub const INSTRUCTIONS: &str = "\
SQLKraft gives you access to the SQL Server connections the user has open in SQLKraft. \
Start with list_connections, then list_databases / list_objects / describe_object to learn the \
schema before writing queries. run_query is read-only unless the user allowed writes for that \
connection; results are capped at max_rows. Prefer open_query_tab to hand the user a script \
to review and run themselves, especially for anything that changes data or schema. \
get_active_query returns what the user is editing in SQLKraft right now.";

const DEFAULT_ROWS: usize = 200;
const MAX_ROWS: usize = 10_000;
/// Longer cell values are cut, so one wide column can't flood the context.
const MAX_CELL_CHARS: usize = 4_000;

pub enum CallError {
    UnknownTool,
    Failed(anyhow::Error),
}

impl From<anyhow::Error> for CallError {
    fn from(e: anyhow::Error) -> Self {
        CallError::Failed(e)
    }
}

pub fn definitions() -> Value {
    let conn = json!({ "type": "string", "description": "Connection name from list_connections." });
    let db = json!({ "type": "string", "description": "Database name." });
    let object = json!({
        "type": "string",
        "description": "Object name: `schema.name` or `name` (default schema). Bracket names with spaces or dots, e.g. `[sales].[Order Lines]`."
    });
    json!([
        {
            "name": "list_connections",
            "title": "List connections",
            "description": "SQL Server connections available to you, with server version and whether writes are allowed.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "list_databases",
            "title": "List databases",
            "description": "Databases on a connection, with state, recovery model, compatibility level and size.",
            "inputSchema": { "type": "object", "properties": { "connection": conn }, "required": ["connection"], "additionalProperties": false },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "list_objects",
            "title": "List objects",
            "description": "Tables, views, stored procedures and functions in a database.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "connection": conn, "database": db,
                    "type": { "type": "string", "enum": ["table", "view", "procedure", "function"], "description": "Only this kind of object." },
                    "schema": { "type": "string", "description": "Only this schema." },
                    "name_like": { "type": "string", "description": "SQL LIKE pattern on the object name, e.g. `%order%`." }
                },
                "required": ["connection", "database"], "additionalProperties": false
            },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "describe_object",
            "title": "Describe object",
            "description": "Columns (type, nullability, identity, computed, default), keys, indexes, foreign keys and row count of a table or view; parameters of a procedure or function.",
            "inputSchema": {
                "type": "object",
                "properties": { "connection": conn, "database": db, "object": object },
                "required": ["connection", "database", "object"], "additionalProperties": false
            },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "get_definition",
            "title": "Get definition",
            "description": "The T-SQL script for an object, like Script as CREATE / ALTER in SQLKraft, in the best syntax for the server's version.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "connection": conn, "database": db, "object": object,
                    "mode": { "type": "string", "enum": ["create", "alter"], "default": "create" }
                },
                "required": ["connection", "database", "object"], "additionalProperties": false
            },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "run_query",
            "title": "Run query",
            "description": "Runs T-SQL and returns the result sets and messages. Read-only unless the user allowed writes for the connection: statements that change data, schema or server state are refused, and everything runs in a transaction that is rolled back. `GO` separates batches.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "connection": conn,
                    "database": { "type": "string", "description": "Database to run in. Defaults to the login's default database." },
                    "sql": { "type": "string" },
                    "max_rows": { "type": "integer", "minimum": 1, "maximum": MAX_ROWS, "default": DEFAULT_ROWS, "description": "Rows kept per result set; the query is stopped after that." }
                },
                "required": ["connection", "sql"], "additionalProperties": false
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": true, "openWorldHint": false }
        },
        {
            "name": "get_active_query",
            "title": "Get active query",
            "description": "The query tab the user is looking at in the SQLKraft window: its text, selection, connection and database.",
            "inputSchema": { "type": "object", "properties": {}, "additionalProperties": false },
            "annotations": { "readOnlyHint": true }
        },
        {
            "name": "open_query_tab",
            "title": "Open query tab",
            "description": "Opens a new query tab in the SQLKraft window with this script, for the user to review and run. Nothing is executed.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "connection": { "type": "string", "description": "Connection for the tab (optional)." },
                    "database": { "type": "string" },
                    "sql": { "type": "string" }
                },
                "required": ["sql"], "additionalProperties": false
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false }
        }
    ])
}

fn arg<'a>(args: &'a Value, name: &str) -> anyhow::Result<&'a str> {
    args.get(name).and_then(Value::as_str).filter(|s| !s.trim().is_empty()).ok_or_else(|| anyhow!("Missing argument `{name}`"))
}

fn opt<'a>(args: &'a Value, name: &str) -> Option<&'a str> {
    args.get(name).and_then(Value::as_str).filter(|s| !s.trim().is_empty())
}

pub fn call(backend: &mut dyn Backend, name: &str, args: &Value) -> Result<String, CallError> {
    let summary = match name {
        "run_query" => format!("{}: {}", opt(args, "connection").unwrap_or("?"), one_line(opt(args, "sql").unwrap_or(""))),
        "list_connections" | "get_active_query" => String::new(),
        _ => [opt(args, "connection"), opt(args, "database"), opt(args, "object")].into_iter().flatten().collect::<Vec<_>>().join(" / "),
    };
    let out = match name {
        "list_connections" => Ok(to_json(&backend.connections())),
        "list_databases" => list_databases(backend, args),
        "list_objects" => list_objects(backend, args),
        "describe_object" => describe_object(backend, args),
        "get_definition" => get_definition(backend, args),
        "run_query" => run_query(backend, args),
        "get_active_query" => backend.active_query().map(|q| match q {
            Some(q) => to_json(&q),
            None => "No query tab is open.".to_string(),
        }),
        "open_query_tab" => (|| {
            let sql = arg(args, "sql")?;
            backend.open_query_tab(opt(args, "connection"), opt(args, "database"), sql)?;
            Ok("Opened a new query tab in SQLKraft. Nothing was run.".to_string())
        })(),
        _ => return Err(CallError::UnknownTool),
    };
    backend.log(name, &match &out {
        Ok(_) => summary,
        Err(e) => format!("{summary} (failed: {e})"),
    });
    Ok(out?)
}

fn one_line(s: &str) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > 120 { format!("{}…", flat.chars().take(120).collect::<String>()) } else { flat }
}

fn to_json(v: &impl serde::Serialize) -> String {
    serde_json::to_string_pretty(v).unwrap_or_default()
}

fn s(r: &[Cell], i: usize) -> Option<String> {
    r.get(i).cloned().flatten()
}

/// Rows as objects keyed by column name, which reads better than bare arrays.
fn objects(cols: &[&str], rows: &[Vec<Cell>]) -> Value {
    Value::Array(
        rows.iter()
            .map(|r| {
                let mut m = serde_json::Map::new();
                for (i, c) in cols.iter().enumerate() {
                    m.insert((*c).to_string(), s(r, i).map(Value::String).unwrap_or(Value::Null));
                }
                Value::Object(m)
            })
            .collect(),
    )
}

fn list_databases(b: &mut dyn Backend, args: &Value) -> anyhow::Result<String> {
    let conn = arg(args, "connection")?;
    let rows = b.fetch(
        conn,
        "SELECT d.name, d.state_desc, d.recovery_model_desc, CAST(d.compatibility_level AS varchar(10)), \
                CASE WHEN d.database_id <= 4 THEN 'yes' ELSE 'no' END, \
                CAST(CAST(SUM(CAST(f.size AS bigint)) * 8 / 1024.0 AS decimal(18,1)) AS varchar(30)) \
         FROM sys.databases d LEFT JOIN sys.master_files f ON f.database_id = d.database_id \
         GROUP BY d.name, d.state_desc, d.recovery_model_desc, d.compatibility_level, d.database_id \
         ORDER BY d.name",
    )?;
    Ok(to_json(&objects(&["name", "state", "recovery_model", "compatibility_level", "system", "size_mb"], &rows)))
}

const TYPE_FILTERS: &[(&str, &str)] = &[
    ("table", "'U'"),
    ("view", "'V'"),
    ("procedure", "'P','PC'"),
    ("function", "'FN','IF','TF','FS','FT'"),
];

fn list_objects(b: &mut dyn Backend, args: &Value) -> anyhow::Result<String> {
    let conn = arg(args, "connection")?;
    let d = quote_ident(arg(args, "database")?);
    let types = match opt(args, "type") {
        Some(t) => TYPE_FILTERS.iter().find(|(k, _)| *k == t).map(|(_, v)| v.to_string()).ok_or_else(|| anyhow!("Unknown type `{t}`"))?,
        None => TYPE_FILTERS.iter().map(|(_, v)| *v).collect::<Vec<_>>().join(","),
    };
    let mut filter = String::new();
    if let Some(schema) = opt(args, "schema") {
        filter.push_str(&format!(" AND s.name = {}", quote_literal(schema)));
    }
    if let Some(like) = opt(args, "name_like") {
        filter.push_str(&format!(" AND o.name LIKE {}", quote_literal(like)));
    }
    let rows = b.fetch(
        conn,
        &format!(
            "SELECT TOP (2000) s.name, o.name, \
                    CASE o.type WHEN 'U' THEN 'table' WHEN 'V' THEN 'view' WHEN 'P' THEN 'procedure' WHEN 'PC' THEN 'procedure' ELSE 'function' END, \
                    CONVERT(varchar(19), o.modify_date, 126) \
             FROM {d}.sys.objects o JOIN {d}.sys.schemas s ON s.schema_id = o.schema_id \
             WHERE o.type IN ({types}) AND o.is_ms_shipped = 0{filter} ORDER BY s.name, o.name"
        ),
    )?;
    Ok(to_json(&objects(&["schema", "name", "type", "modified"], &rows)))
}

/// A resolved object: (schema, name, sys.objects.type).
fn resolve(b: &mut dyn Backend, conn: &str, db: &str, object: &str) -> anyhow::Result<(String, String, String)> {
    let d = quote_ident(db);
    let parts = name_parts(object);
    let full = match parts.len() {
        1 => format!("{d}..{object}"),
        2 => format!("{d}.{object}"),
        _ => bail!("`{object}` should be `schema.name` or `name`"),
    };
    let rows = b.fetch(
        conn,
        &format!(
            "SELECT s.name, o.name, RTRIM(o.type) FROM {d}.sys.objects o JOIN {d}.sys.schemas s ON s.schema_id = o.schema_id \
             WHERE o.object_id = OBJECT_ID({})",
            quote_literal(&full)
        ),
    )?;
    let r = rows.first().ok_or_else(|| anyhow!("No object `{object}` in database `{db}`"))?;
    Ok((s(r, 0).unwrap_or_default(), s(r, 1).unwrap_or_default(), s(r, 2).unwrap_or_default()))
}

/// Splits `a.b` / `[a.b].[c]` on dots outside brackets and quotes.
fn name_parts(name: &str) -> Vec<String> {
    let mut parts = vec![String::new()];
    let mut chars = name.chars().peekable();
    while let Some(c) = chars.next() {
        let part = parts.last_mut().unwrap();
        match c {
            '[' => {
                // Bracketed: up to the closing `]`, with `]]` meaning a literal `]`.
                while let Some(c) = chars.next() {
                    if c == ']' {
                        if chars.peek() == Some(&']') {
                            chars.next();
                            part.push(']');
                            continue;
                        }
                        break;
                    }
                    part.push(c);
                }
            }
            '"' => {
                for c in chars.by_ref() {
                    if c == '"' {
                        break;
                    }
                    part.push(c);
                }
            }
            '.' => parts.push(String::new()),
            c => part.push(c),
        }
    }
    parts
}

fn node_kind(db: &str, schema: &str, name: &str, ty: &str) -> anyhow::Result<NodeKind> {
    let (db, schema, name) = (db.to_string(), schema.to_string(), name.to_string());
    Ok(match ty {
        "U" => NodeKind::Table { db, schema, name },
        "V" => NodeKind::View { db, schema, name },
        "P" | "PC" => NodeKind::Procedure { db, schema, name },
        "FN" | "IF" | "TF" | "FS" | "FT" => NodeKind::Function { db, schema, name, table_valued: matches!(ty, "IF" | "TF" | "FT") },
        other => bail!("Objects of type `{other}` are not supported"),
    })
}

fn describe_object(b: &mut dyn Backend, args: &Value) -> anyhow::Result<String> {
    let conn = arg(args, "connection")?;
    let db = arg(args, "database")?;
    let (schema, name, ty) = resolve(b, conn, db, arg(args, "object")?)?;
    let d = quote_ident(db);
    let oid = format!("OBJECT_ID({})", quote_literal(&format!("{d}.{}.{}", quote_ident(&schema), quote_ident(&name))));
    let mut out = serde_json::Map::new();
    out.insert("schema".into(), json!(schema));
    out.insert("name".into(), json!(name));
    out.insert("type".into(), json!(ty));

    if matches!(ty.as_str(), "U" | "V") {
        let cols = b.fetch(
            conn,
            &format!(
                "SELECT c.name, ty.name, CAST(c.max_length AS varchar(10)), CAST(c.precision AS varchar(10)), CAST(c.scale AS varchar(10)), \
                        CASE c.is_nullable WHEN 1 THEN 'yes' ELSE 'no' END, CASE c.is_identity WHEN 1 THEN 'yes' ELSE 'no' END, \
                        cc.definition, dc.definition, c.collation_name \
                 FROM {d}.sys.columns c JOIN {d}.sys.types ty ON ty.user_type_id = c.user_type_id \
                 LEFT JOIN {d}.sys.computed_columns cc ON cc.object_id = c.object_id AND cc.column_id = c.column_id \
                 LEFT JOIN {d}.sys.default_constraints dc ON dc.parent_object_id = c.object_id AND dc.parent_column_id = c.column_id \
                 WHERE c.object_id = {oid} ORDER BY c.column_id"
            ),
        )?;
        let columns: Vec<Value> = cols
            .iter()
            .map(|r| {
                let ty = explorer::type_spec(&s(r, 1).unwrap_or_default(), &s(r, 2).unwrap_or_default(), &s(r, 3).unwrap_or_default(), &s(r, 4).unwrap_or_default());
                json!({
                    "name": s(r, 0), "type": ty, "nullable": s(r, 5).as_deref() == Some("yes"),
                    "identity": s(r, 6).as_deref() == Some("yes"), "computed": s(r, 7), "default": s(r, 8), "collation": s(r, 9),
                })
            })
            .collect();
        out.insert("columns".into(), Value::Array(columns));

        let idx = b.fetch(
            conn,
            &format!(
                "SELECT i.name, i.type_desc, CASE i.is_primary_key WHEN 1 THEN 'yes' ELSE 'no' END, \
                        CASE i.is_unique WHEN 1 THEN 'yes' ELSE 'no' END, c.name, \
                        CASE ic.is_included_column WHEN 1 THEN 'yes' ELSE 'no' END, CASE ic.is_descending_key WHEN 1 THEN 'yes' ELSE 'no' END, \
                        i.filter_definition \
                 FROM {d}.sys.indexes i \
                 JOIN {d}.sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
                 JOIN {d}.sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
                 WHERE i.object_id = {oid} AND i.type > 0 ORDER BY i.index_id, ic.is_included_column, ic.key_ordinal, ic.index_column_id"
            ),
        )?;
        let mut indexes: Vec<Value> = Vec::new();
        for r in &idx {
            let iname = s(r, 0).unwrap_or_default();
            if indexes.last().and_then(|v| v["name"].as_str()) != Some(iname.as_str()) {
                indexes.push(json!({
                    "name": iname, "type": s(r, 1), "primary_key": s(r, 2).as_deref() == Some("yes"),
                    "unique": s(r, 3).as_deref() == Some("yes"), "keys": [], "included": [], "filter": s(r, 7),
                }));
            }
            let last = indexes.last_mut().unwrap();
            let col = format!("{}{}", s(r, 4).unwrap_or_default(), if s(r, 6).as_deref() == Some("yes") { " DESC" } else { "" });
            let list = if s(r, 5).as_deref() == Some("yes") { "included" } else { "keys" };
            last[list].as_array_mut().unwrap().push(json!(col));
        }
        out.insert("indexes".into(), Value::Array(indexes));

        if ty == "U" {
            let fks = b.fetch(
                conn,
                &format!(
                    "SELECT fk.name, pc.name, rs.name + '.' + rt.name, rc.name, fk.delete_referential_action_desc \
                     FROM {d}.sys.foreign_keys fk \
                     JOIN {d}.sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id \
                     JOIN {d}.sys.columns pc ON pc.object_id = fkc.parent_object_id AND pc.column_id = fkc.parent_column_id \
                     JOIN {d}.sys.tables rt ON rt.object_id = fkc.referenced_object_id \
                     JOIN {d}.sys.schemas rs ON rs.schema_id = rt.schema_id \
                     JOIN {d}.sys.columns rc ON rc.object_id = fkc.referenced_object_id AND rc.column_id = fkc.referenced_column_id \
                     WHERE fk.parent_object_id = {oid} ORDER BY fk.name, fkc.constraint_column_id"
                ),
            )?;
            out.insert("foreign_keys".into(), objects(&["name", "column", "references_table", "references_column", "on_delete"], &fks));
            let count = b.fetch(
                conn,
                &format!("SELECT CAST(SUM(p.rows) AS varchar(30)) FROM {d}.sys.partitions p WHERE p.object_id = {oid} AND p.index_id IN (0, 1)"),
            )?;
            out.insert("approximate_rows".into(), json!(count.first().and_then(|r| s(r, 0))));
        }
    } else {
        let params = b.fetch(
            conn,
            &format!(
                "SELECT p.name, ty.name, CAST(p.max_length AS varchar(10)), CAST(p.precision AS varchar(10)), CAST(p.scale AS varchar(10)), \
                        CASE p.is_output WHEN 1 THEN 'yes' ELSE 'no' END, CASE p.has_default_value WHEN 1 THEN 'yes' ELSE 'no' END \
                 FROM {d}.sys.parameters p JOIN {d}.sys.types ty ON ty.user_type_id = p.user_type_id \
                 WHERE p.object_id = {oid} ORDER BY p.parameter_id"
            ),
        )?;
        let list: Vec<Value> = params
            .iter()
            .map(|r| {
                let ty = explorer::type_spec(&s(r, 1).unwrap_or_default(), &s(r, 2).unwrap_or_default(), &s(r, 3).unwrap_or_default(), &s(r, 4).unwrap_or_default());
                // parameter_id 0 of a function is its return value and has no name.
                let name = s(r, 0).filter(|n| !n.is_empty()).unwrap_or_else(|| "(returns)".into());
                json!({ "name": name, "type": ty, "output": s(r, 5).as_deref() == Some("yes"), "has_default": s(r, 6).as_deref() == Some("yes") })
            })
            .collect();
        out.insert("parameters".into(), Value::Array(list));
    }
    Ok(to_json(&Value::Object(out)))
}

fn get_definition(b: &mut dyn Backend, args: &Value) -> anyhow::Result<String> {
    let conn = arg(args, "connection")?;
    let db = arg(args, "database")?;
    let mode = match opt(args, "mode").unwrap_or("create") {
        "create" => ScriptMode::Create,
        "alter" => ScriptMode::Alter,
        other => bail!("Unknown mode `{other}`"),
    };
    let (schema, name, ty) = resolve(b, conn, db, arg(args, "object")?)?;
    let kind = node_kind(db, &schema, &name, &ty)?;
    let q = explorer::script_create_query(&kind).context("can't script this object")?;
    if let Some(kq) = explorer::script_keys_query(&kind) {
        let cols = b.fetch(conn, &q)?;
        let keys = b.fetch(conn, &kq)?;
        return Ok(explorer::build_create_table(&kind, &cols, &keys));
    }
    let rows = b.fetch(conn, &q)?;
    let row = rows.first().ok_or_else(|| anyhow!("No definition for `{schema}.{name}`"))?;
    let def = s(row, 0).ok_or_else(|| anyhow!("The definition of `{schema}.{name}` is encrypted or not visible to this login"))?;
    let (version, edition) = b.server_version(conn);
    let syntax = Syntax::for_server(&version, edition);
    Ok(scripting::module_script(&kind, &def, &s(row, 1).unwrap_or_default(), mode, syntax))
}

fn run_query(b: &mut dyn Backend, args: &Value) -> anyhow::Result<String> {
    let conn = arg(args, "connection")?;
    let sql = arg(args, "sql")?;
    let max_rows = args.get("max_rows").and_then(Value::as_u64).map(|n| n as usize).unwrap_or(DEFAULT_ROWS).clamp(1, MAX_ROWS);
    let read_only = !b.writes_allowed(conn);
    let batches = if read_only {
        if let Some(why) = guard::check_read_only(sql) {
            bail!(why);
        }
        guard::read_only_batches(sql)
    } else {
        split_batches(sql, 0)
    };
    let result = b.execute(conn, opt(args, "database"), batches, max_rows);
    if read_only {
        // Stopping at max_rows cancels the run before its own ROLLBACK batch; make sure.
        b.fetch(conn, "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION")?;
    }
    let mut out = result?;
    for set in &mut out.result_sets {
        for row in &mut set.rows {
            for cell in row.iter_mut().flatten() {
                if cell.chars().count() > MAX_CELL_CHARS {
                    *cell = format!("{}… ({} characters)", cell.chars().take(MAX_CELL_CHARS).collect::<String>(), cell.chars().count());
                }
            }
        }
    }
    Ok(to_json(&out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_parts_respects_brackets() {
        assert_eq!(name_parts("dbo.Customers"), ["dbo", "Customers"]);
        assert_eq!(name_parts("[sales].[Order Lines]"), ["sales", "Order Lines"]);
        assert_eq!(name_parts("[a.b].c"), ["a.b", "c"]);
        assert_eq!(name_parts("[Odd]]Name]"), ["Odd]Name"]);
        assert_eq!(name_parts("Customers"), ["Customers"]);
    }

    #[test]
    fn every_tool_has_a_schema() {
        let defs = definitions();
        let names: Vec<&str> = defs.as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(
            names,
            ["list_connections", "list_databases", "list_objects", "describe_object", "get_definition", "run_query", "get_active_query", "open_query_tab"]
        );
        for t in defs.as_array().unwrap() {
            assert_eq!(t["inputSchema"]["type"], "object", "{}", t["name"]);
        }
    }
}
