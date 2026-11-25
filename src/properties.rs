//! The Properties pane (F4): read-only facts about the node selected in Object Explorer.
//!
//! Each node kind maps to one single-row catalog query. Every value is cast to `nvarchar` on
//! the server, so `sql_variant` (SERVERPROPERTY, LOGINPROPERTY) and dates come back as text,
//! and the query only uses catalog views that exist on SQL Server 2008 and later and that any
//! user who can see the object can read.

use crate::db::{Cell, ConnectionInfo, ServerFacts};
use crate::explorer::{NodeKind, type_spec};
use crate::sql::{quote_ident, quote_literal};

/// How a raw value is shown.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Fmt {
    Text,
    /// `1` / `0` shown as True / False.
    Bool,
    /// `yyyy-mm-dd hh:mi:ss`.
    Date,
    /// An integer with thousands separators.
    Int,
    /// A count of 8 KB pages shown in MB.
    Pages,
    /// `name|max_length|precision|scale` shown as `nvarchar(50)` etc.
    Type,
}

#[derive(Debug, Clone)]
struct Field {
    category: &'static str,
    label: &'static str,
    expr: String,
    fmt: Fmt,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Property {
    pub category: &'static str,
    pub label: &'static str,
    pub value: String,
}

/// A query plus the labels and formats of its columns.
#[derive(Debug, Clone)]
pub struct PropQuery {
    pub sql: String,
    fields: Vec<Field>,
}

impl PropQuery {
    fn new(fields: Vec<Field>, from: &str) -> Self {
        let cols: Vec<String> = fields
            .iter()
            .map(|f| match f.fmt {
                Fmt::Date => format!("CONVERT(nvarchar(30), CAST({} AS datetime2(0)), 120)", f.expr),
                _ => format!("CAST({} AS nvarchar(4000))", f.expr),
            })
            .collect();
        // The XML .value() used to join column and role names needs QUOTED_IDENTIFIER ON, and
        // a connection's default can be OFF. The SET returns no result set.
        let sql = format!("SET QUOTED_IDENTIFIER ON;\nSELECT {}\n{from}", cols.join(",\n       "));
        Self { sql: sql.trim_end().to_string(), fields }
    }

    /// Turns the first result row into labelled, formatted properties.
    pub fn parse(&self, rows: &[Vec<Cell>]) -> Result<Vec<Property>, String> {
        let Some(row) = rows.first() else {
            return Err("The object was not found. It may have been dropped or renamed: refresh Object Explorer.".into());
        };
        Ok(self
            .fields
            .iter()
            .enumerate()
            .map(|(i, f)| Property {
                category: f.category,
                label: f.label,
                value: format_value(row.get(i).cloned().flatten().as_deref(), f.fmt),
            })
            .collect())
    }
}

fn f(category: &'static str, label: &'static str, expr: impl Into<String>, fmt: Fmt) -> Field {
    Field { category, label, expr: expr.into(), fmt }
}

pub fn format_value(v: Option<&str>, fmt: Fmt) -> String {
    let Some(v) = v else { return String::new() };
    match fmt {
        Fmt::Text | Fmt::Date => v.to_string(),
        Fmt::Bool => match v {
            "1" => "True".into(),
            "0" => "False".into(),
            _ => v.to_string(),
        },
        Fmt::Int => v.parse::<i64>().map(group_thousands).unwrap_or_else(|_| v.to_string()),
        Fmt::Pages => match v.parse::<f64>() {
            Ok(p) => format!("{:.2}", p * 8.0 / 1024.0),
            Err(_) => v.to_string(),
        },
        Fmt::Type => {
            let p: Vec<&str> = v.split('|').collect();
            match p.as_slice() {
                [ty, len, prec, scale] => type_spec(ty, len, prec, scale),
                _ => v.to_string(),
            }
        }
    }
}

fn group_thousands(n: i64) -> String {
    let digits = n.unsigned_abs().to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

/// What the pane calls this kind of node, e.g. "Table".
pub fn type_name(kind: &NodeKind) -> &'static str {
    match kind {
        NodeKind::Server => "Server",
        NodeKind::Database { .. } => "Database",
        NodeKind::Table { .. } => "Table",
        NodeKind::View { .. } => "View",
        NodeKind::Procedure { .. } => "Stored Procedure",
        NodeKind::Function { .. } => "Function",
        NodeKind::Column { .. } => "Column",
        NodeKind::Index { .. } => "Index",
        NodeKind::Login { .. } => "Login",
        NodeKind::Leaf => "Item",
        _ => "Folder",
    }
}

/// Properties that come from the connection rather than the server.
pub fn connection_properties(info: &ConnectionInfo, facts: Option<&ServerFacts>) -> Vec<Property> {
    let c = |label, value: String| Property { category: "Connection", label, value };
    let mut v = vec![
        c("Server name", info.server.clone()),
        c("Authentication", info.auth.label().to_string()),
        c("User name", info.display_user()),
        c("Encryption", info.encrypt.label().to_string()),
        c("Trust server certificate", if info.trust_server_certificate { "True" } else { "False" }.into()),
    ];
    if let Some(f) = facts {
        v.push(c("Login", f.login.clone()));
        v.push(c("SPID", f.spid.to_string()));
    }
    v
}

/// `(SELECT MS_Description ...)` for an object (minor 0) or one of its columns.
fn description(d: &str, major: &str, minor: &str) -> String {
    format!(
        "(SELECT TOP (1) CAST(ep.value AS nvarchar(4000)) FROM {d}.sys.extended_properties ep \
         WHERE ep.class = 1 AND ep.major_id = {major} AND ep.minor_id = {minor} AND ep.name = N'MS_Description')"
    )
}

/// `SUM(used_pages)` of a table's or index's allocation units.
fn used_pages(d: &str, object: &str, index_filter: &str) -> String {
    format!(
        "(SELECT SUM(a.used_pages) FROM {d}.sys.partitions p \
         JOIN {d}.sys.allocation_units a ON a.container_id = p.partition_id \
         WHERE p.object_id = {object} AND {index_filter})"
    )
}

fn rows(d: &str, object: &str, index_filter: &str) -> String {
    format!("(SELECT SUM(p.rows) FROM {d}.sys.partitions p WHERE p.object_id = {object} AND {index_filter})")
}

/// `N'[db].[schema].[name]'` wrapped in OBJECT_ID.
fn object_id(db: &str, schema: &str, name: &str) -> String {
    format!("OBJECT_ID({})", quote_literal(&format!("{}.{}.{}", quote_ident(db), quote_ident(schema), quote_ident(name))))
}

/// Comma-separated index columns, in key order or (for included columns) column order.
fn index_columns(d: &str, included: bool) -> String {
    let (filter, order) = if included { ("1", "ic.index_column_id") } else { ("0", "ic.key_ordinal") };
    format!(
        "STUFF((SELECT N', ' + QUOTENAME(c.name) + CASE WHEN ic.is_descending_key = 1 THEN N' DESC' ELSE N'' END \
         FROM {d}.sys.index_columns ic JOIN {d}.sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
         WHERE ic.object_id = i.object_id AND ic.index_id = i.index_id AND ic.is_included_column = {filter} \
         ORDER BY {order} FOR XML PATH(''), TYPE).value('.', 'nvarchar(max)'), 1, 2, N'')"
    )
}

/// The properties query for a node, or None for folders.
pub fn query(kind: &NodeKind) -> Option<PropQuery> {
    use Fmt::*;
    Some(match kind {
        NodeKind::Server => {
            let cfg = |name: &str| format!("(SELECT value_in_use FROM sys.configurations WHERE name = N'{name}')");
            PropQuery::new(
                vec![
                    f("General", "Name", "SERVERPROPERTY('ServerName')", Text),
                    f("General", "Machine name", "SERVERPROPERTY('MachineName')", Text),
                    f("General", "Instance", "ISNULL(CAST(SERVERPROPERTY('InstanceName') AS nvarchar(128)), N'(default)')", Text),
                    f("General", "Edition", "SERVERPROPERTY('Edition')", Text),
                    f(
                        "General",
                        "Engine edition",
                        "CASE CAST(SERVERPROPERTY('EngineEdition') AS int) WHEN 1 THEN N'Personal or Desktop' \
                         WHEN 2 THEN N'Standard' WHEN 3 THEN N'Enterprise' WHEN 4 THEN N'Express' \
                         WHEN 5 THEN N'Azure SQL Database' WHEN 6 THEN N'Azure Synapse Analytics' \
                         WHEN 8 THEN N'Azure SQL Managed Instance' WHEN 9 THEN N'Azure SQL Edge' \
                         ELSE CAST(SERVERPROPERTY('EngineEdition') AS nvarchar(10)) END",
                        Text,
                    ),
                    f("General", "Version", "SERVERPROPERTY('ProductVersion')", Text),
                    f("General", "Product level", "SERVERPROPERTY('ProductLevel')", Text),
                    f("General", "Update level", "SERVERPROPERTY('ProductUpdateLevel')", Text),
                    f("General", "Collation", "SERVERPROPERTY('Collation')", Text),
                    f("General", "Clustered", "SERVERPROPERTY('IsClustered')", Bool),
                    f("General", "Always On availability groups", "SERVERPROPERTY('IsHadrEnabled')", Bool),
                    f("General", "Started", "(SELECT create_date FROM sys.databases WHERE name = N'tempdb')", Date),
                    f(
                        "Security",
                        "Server authentication",
                        "CASE CAST(SERVERPROPERTY('IsIntegratedSecurityOnly') AS int) WHEN 1 THEN N'Windows Authentication' \
                         ELSE N'SQL Server and Windows Authentication' END",
                        Text,
                    ),
                    f("Configuration", "Min server memory (MB)", cfg("min server memory (MB)"), Int),
                    f("Configuration", "Max server memory (MB)", cfg("max server memory (MB)"), Int),
                    f("Configuration", "Max degree of parallelism", cfg("max degree of parallelism"), Int),
                    f("Configuration", "Cost threshold for parallelism", cfg("cost threshold for parallelism"), Int),
                ],
                "",
            )
        }
        NodeKind::Database { db } => {
            let files = |filter: &str| {
                format!(
                    "(SELECT SUM(CAST(mf.size AS bigint)) FROM sys.master_files mf WHERE mf.database_id = d.database_id{filter})"
                )
            };
            PropQuery::new(
                vec![
                    f("General", "Name", "d.name", Text),
                    f("General", "Status", "d.state_desc", Text),
                    f("General", "Owner", "SUSER_SNAME(d.owner_sid)", Text),
                    f("General", "Date created", "d.create_date", Date),
                    f("General", "Database ID", "d.database_id", Text),
                    f("Size", "Size (MB)", files(""), Pages),
                    f("Size", "Data files (MB)", files(" AND mf.type = 0"), Pages),
                    f("Size", "Log files (MB)", files(" AND mf.type = 1"), Pages),
                    f("Options", "Recovery model", "d.recovery_model_desc", Text),
                    f("Options", "Compatibility level", "d.compatibility_level", Text),
                    f("Options", "Collation", "d.collation_name", Text),
                    f("Options", "Page verify", "d.page_verify_option_desc", Text),
                    f("Options", "Restrict access", "d.user_access_desc", Text),
                    f("Options", "Read-only", "d.is_read_only", Bool),
                    f("Options", "Auto close", "d.is_auto_close_on", Bool),
                    f("Options", "Auto shrink", "d.is_auto_shrink_on", Bool),
                    f("Options", "Auto create statistics", "d.is_auto_create_stats_on", Bool),
                    f("Options", "Auto update statistics", "d.is_auto_update_stats_on", Bool),
                    f("Options", "Auto update statistics asynchronously", "d.is_auto_update_stats_async_on", Bool),
                    f("Advanced", "Snapshot isolation", "d.snapshot_isolation_state_desc", Text),
                    f("Advanced", "Read committed snapshot", "d.is_read_committed_snapshot_on", Bool),
                    f("Advanced", "Service Broker enabled", "d.is_broker_enabled", Bool),
                    f("Advanced", "Trustworthy", "d.is_trustworthy_on", Bool),
                    f("Advanced", "Encrypted", "d.is_encrypted", Bool),
                ],
                &format!("FROM sys.databases d WHERE d.name = {}", quote_literal(db)),
            )
        }
        NodeKind::Table { db, schema, name } => {
            let d = &quote_ident(db);
            PropQuery::new(
                vec![
                    f("General", "Name", "t.name", Text),
                    f("General", "Schema", "s.name", Text),
                    f("General", "Created", "t.create_date", Date),
                    f("General", "Last modified", "t.modify_date", Date),
                    f("General", "Object ID", "t.object_id", Text),
                    f("General", "System object", "t.is_ms_shipped", Bool),
                    f("General", "Description", description(d, "t.object_id", "0"), Text),
                    f("Storage", "Row count", rows(d, "t.object_id", "p.index_id IN (0, 1)"), Int),
                    f("Storage", "Data space (MB)", used_pages(d, "t.object_id", "p.index_id IN (0, 1)"), Pages),
                    f("Storage", "Index space (MB)", used_pages(d, "t.object_id", "p.index_id > 1"), Pages),
                    f(
                        "Storage",
                        "Partitions",
                        format!("(SELECT COUNT(*) FROM {d}.sys.partitions p WHERE p.object_id = t.object_id AND p.index_id IN (0, 1))"),
                        Int,
                    ),
                    f(
                        "Storage",
                        "Filegroup / partition scheme",
                        format!(
                            "(SELECT ds.name FROM {d}.sys.indexes i JOIN {d}.sys.data_spaces ds ON ds.data_space_id = i.data_space_id \
                             WHERE i.object_id = t.object_id AND i.index_id IN (0, 1))"
                        ),
                        Text,
                    ),
                    f(
                        "Storage",
                        "Text filegroup",
                        format!("(SELECT ds.name FROM {d}.sys.data_spaces ds WHERE ds.data_space_id = t.lob_data_space_id)"),
                        Text,
                    ),
                    f(
                        "Storage",
                        "Heap",
                        format!(
                            "CASE WHEN EXISTS (SELECT 1 FROM {d}.sys.indexes i WHERE i.object_id = t.object_id AND i.index_id = 0) \
                             THEN 1 ELSE 0 END"
                        ),
                        Bool,
                    ),
                    f(
                        "Options",
                        "Columns",
                        format!("(SELECT COUNT(*) FROM {d}.sys.columns c WHERE c.object_id = t.object_id)"),
                        Int,
                    ),
                    f("Options", "Lock escalation", "t.lock_escalation_desc", Text),
                    f("Options", "ANSI NULLs", "t.uses_ansi_nulls", Bool),
                    f("Options", "Replicated", "t.is_replicated", Bool),
                ],
                &format!(
                    "FROM {d}.sys.tables t JOIN {d}.sys.schemas s ON s.schema_id = t.schema_id WHERE t.object_id = {}",
                    object_id(db, schema, name)
                ),
            )
        }
        NodeKind::View { db, schema, name } => {
            let d = &quote_ident(db);
            PropQuery::new(
                vec![
                    f("General", "Name", "v.name", Text),
                    f("General", "Schema", "s.name", Text),
                    f("General", "Created", "v.create_date", Date),
                    f("General", "Last modified", "v.modify_date", Date),
                    f("General", "Object ID", "v.object_id", Text),
                    f("General", "System object", "v.is_ms_shipped", Bool),
                    f("General", "Description", description(d, "v.object_id", "0"), Text),
                    f(
                        "Options",
                        "Columns",
                        format!("(SELECT COUNT(*) FROM {d}.sys.columns c WHERE c.object_id = v.object_id)"),
                        Int,
                    ),
                    f("Options", "Schema bound", "m.is_schema_bound", Bool),
                    f("Options", "Check option", "v.with_check_option", Bool),
                    f(
                        "Options",
                        "Indexed",
                        format!(
                            "CASE WHEN EXISTS (SELECT 1 FROM {d}.sys.indexes i WHERE i.object_id = v.object_id AND i.index_id > 0) \
                             THEN 1 ELSE 0 END"
                        ),
                        Bool,
                    ),
                    f("Options", "ANSI NULLs", "m.uses_ansi_nulls", Bool),
                    f("Options", "Quoted identifier", "m.uses_quoted_identifier", Bool),
                ],
                &format!(
                    "FROM {d}.sys.views v JOIN {d}.sys.schemas s ON s.schema_id = v.schema_id \
                     LEFT JOIN {d}.sys.sql_modules m ON m.object_id = v.object_id WHERE v.object_id = {}",
                    object_id(db, schema, name)
                ),
            )
        }
        NodeKind::Procedure { db, schema, name } => {
            let d = &quote_ident(db);
            PropQuery::new(
                vec![
                    f("General", "Name", "o.name", Text),
                    f("General", "Schema", "s.name", Text),
                    f("General", "Created", "o.create_date", Date),
                    f("General", "Last modified", "o.modify_date", Date),
                    f("General", "Object ID", "o.object_id", Text),
                    f("General", "Type", "o.type_desc", Text),
                    f("General", "System object", "o.is_ms_shipped", Bool),
                    f("General", "Description", description(d, "o.object_id", "0"), Text),
                    f(
                        "Options",
                        "Parameters",
                        format!("(SELECT COUNT(*) FROM {d}.sys.parameters pa WHERE pa.object_id = o.object_id)"),
                        Int,
                    ),
                    f("Options", "Execute as", execute_as(d), Text),
                    f("Options", "Recompile", "m.is_recompiled", Bool),
                    f("Options", "Runs at startup", "o.is_auto_executed", Bool),
                    f("Options", "ANSI NULLs", "m.uses_ansi_nulls", Bool),
                    f("Options", "Quoted identifier", "m.uses_quoted_identifier", Bool),
                ],
                &format!(
                    "FROM {d}.sys.procedures o JOIN {d}.sys.schemas s ON s.schema_id = o.schema_id \
                     LEFT JOIN {d}.sys.sql_modules m ON m.object_id = o.object_id WHERE o.object_id = {}",
                    object_id(db, schema, name)
                ),
            )
        }
        NodeKind::Function { db, schema, name, .. } => {
            let d = &quote_ident(db);
            PropQuery::new(
                vec![
                    f("General", "Name", "o.name", Text),
                    f("General", "Schema", "s.name", Text),
                    f("General", "Created", "o.create_date", Date),
                    f("General", "Last modified", "o.modify_date", Date),
                    f("General", "Object ID", "o.object_id", Text),
                    f(
                        "General",
                        "Type",
                        "CASE o.type WHEN 'FN' THEN N'Scalar' WHEN 'IF' THEN N'Inline table-valued' \
                         WHEN 'TF' THEN N'Multi-statement table-valued' WHEN 'FS' THEN N'CLR scalar' \
                         WHEN 'FT' THEN N'CLR table-valued' WHEN 'AF' THEN N'CLR aggregate' ELSE o.type_desc END",
                        Text,
                    ),
                    f("General", "System object", "o.is_ms_shipped", Bool),
                    f("General", "Description", description(d, "o.object_id", "0"), Text),
                    f(
                        "Options",
                        "Returns",
                        format!(
                            "CASE WHEN o.type IN ('IF', 'TF', 'FT') THEN N'table' ELSE \
                             (SELECT ty.name + N'|' + CAST(pa.max_length AS nvarchar(10)) + N'|' + CAST(pa.precision AS nvarchar(10)) \
                                     + N'|' + CAST(pa.scale AS nvarchar(10)) \
                              FROM {d}.sys.parameters pa JOIN {d}.sys.types ty ON ty.user_type_id = pa.user_type_id \
                              WHERE pa.object_id = o.object_id AND pa.parameter_id = 0) END"
                        ),
                        Type,
                    ),
                    f(
                        "Options",
                        "Parameters",
                        format!(
                            "(SELECT COUNT(*) FROM {d}.sys.parameters pa WHERE pa.object_id = o.object_id AND pa.parameter_id > 0)"
                        ),
                        Int,
                    ),
                    f("Options", "Schema bound", "m.is_schema_bound", Bool),
                    f("Options", "Execute as", execute_as(d), Text),
                    f("Options", "ANSI NULLs", "m.uses_ansi_nulls", Bool),
                    f("Options", "Quoted identifier", "m.uses_quoted_identifier", Bool),
                ],
                &format!(
                    "FROM {d}.sys.objects o JOIN {d}.sys.schemas s ON s.schema_id = o.schema_id \
                     LEFT JOIN {d}.sys.sql_modules m ON m.object_id = o.object_id WHERE o.object_id = {}",
                    object_id(db, schema, name)
                ),
            )
        }
        NodeKind::Column { db, object_id, name } => {
            let d = &quote_ident(db);
            PropQuery::new(
                vec![
                    f("General", "Name", "c.name", Text),
                    f("General", "Position", "c.column_id", Int),
                    f(
                        "General",
                        "Data type",
                        "ty.name + N'|' + CAST(c.max_length AS nvarchar(10)) + N'|' + CAST(c.precision AS nvarchar(10)) \
                         + N'|' + CAST(c.scale AS nvarchar(10))",
                        Type,
                    ),
                    f("General", "Allow nulls", "c.is_nullable", Bool),
                    f("General", "Collation", "c.collation_name", Text),
                    f("General", "Description", description(d, "c.object_id", "c.column_id"), Text),
                    f(
                        "Keys",
                        "Primary key",
                        format!(
                            "CASE WHEN EXISTS (SELECT 1 FROM {d}.sys.index_columns ic JOIN {d}.sys.indexes i \
                             ON i.object_id = ic.object_id AND i.index_id = ic.index_id \
                             WHERE i.is_primary_key = 1 AND ic.object_id = c.object_id AND ic.column_id = c.column_id) \
                             THEN 1 ELSE 0 END"
                        ),
                        Bool,
                    ),
                    f("Keys", "Default", "dc.definition", Text),
                    f("Keys", "Default constraint", "dc.name", Text),
                    f("Identity", "Identity", "c.is_identity", Bool),
                    f("Identity", "Identity seed", "idc.seed_value", Text),
                    f("Identity", "Identity increment", "idc.increment_value", Text),
                    f("Identity", "RowGuid", "c.is_rowguidcol", Bool),
                    f("Computed", "Computed", "c.is_computed", Bool),
                    f("Computed", "Formula", "cc.definition", Text),
                    f("Computed", "Persisted", "cc.is_persisted", Bool),
                    f("Computed", "Sparse", "c.is_sparse", Bool),
                ],
                &format!(
                    "FROM {d}.sys.columns c JOIN {d}.sys.types ty ON ty.user_type_id = c.user_type_id \
                     LEFT JOIN {d}.sys.computed_columns cc ON cc.object_id = c.object_id AND cc.column_id = c.column_id \
                     LEFT JOIN {d}.sys.default_constraints dc ON dc.parent_object_id = c.object_id AND dc.parent_column_id = c.column_id \
                     LEFT JOIN {d}.sys.identity_columns idc ON idc.object_id = c.object_id AND idc.column_id = c.column_id \
                     WHERE c.object_id = {object_id} AND c.name = {}",
                    quote_literal(name)
                ),
            )
        }
        NodeKind::Index { db, object_id, name } => {
            let d = &quote_ident(db);
            let this = "p.index_id = i.index_id";
            PropQuery::new(
                vec![
                    f("General", "Name", "i.name", Text),
                    f("General", "Index ID", "i.index_id", Text),
                    f("General", "Type", "i.type_desc", Text),
                    f("General", "Unique", "i.is_unique", Bool),
                    f("General", "Primary key", "i.is_primary_key", Bool),
                    f("General", "Unique constraint", "i.is_unique_constraint", Bool),
                    f("General", "Disabled", "i.is_disabled", Bool),
                    f("Columns", "Key columns", index_columns(d, false), Text),
                    f("Columns", "Included columns", index_columns(d, true), Text),
                    f("Columns", "Filter", "i.filter_definition", Text),
                    f("Options", "Fill factor", "i.fill_factor", Int),
                    f("Options", "Pad index", "i.is_padded", Bool),
                    f("Options", "Ignore duplicate keys", "i.ignore_dup_key", Bool),
                    f("Options", "Allow row locks", "i.allow_row_locks", Bool),
                    f("Options", "Allow page locks", "i.allow_page_locks", Bool),
                    f("Storage", "Filegroup / partition scheme", "ds.name", Text),
                    f("Storage", "Rows", rows(d, "i.object_id", this), Int),
                    f("Storage", "Space used (MB)", used_pages(d, "i.object_id", this), Pages),
                ],
                &format!(
                    "FROM {d}.sys.indexes i LEFT JOIN {d}.sys.data_spaces ds ON ds.data_space_id = i.data_space_id \
                     WHERE i.object_id = {object_id} AND i.name = {}",
                    quote_literal(name)
                ),
            )
        }
        NodeKind::Login { name } => PropQuery::new(
            vec![
                f("General", "Name", "p.name", Text),
                f("General", "Type", "p.type_desc", Text),
                f("General", "Created", "p.create_date", Date),
                f("General", "Last modified", "p.modify_date", Date),
                f("General", "Disabled", "p.is_disabled", Bool),
                f("General", "Default database", "p.default_database_name", Text),
                f("General", "Default language", "p.default_language_name", Text),
                f("General", "SID", "CONVERT(nvarchar(200), p.sid, 1)", Text),
                f(
                    "General",
                    "Server roles",
                    "STUFF((SELECT N', ' + r.name FROM sys.server_role_members rm \
                     JOIN sys.server_principals r ON r.principal_id = rm.role_principal_id \
                     WHERE rm.member_principal_id = p.principal_id ORDER BY r.name \
                     FOR XML PATH(''), TYPE).value('.', 'nvarchar(max)'), 1, 2, N'')",
                    Text,
                ),
                f("Password policy", "Enforce password policy", "l.is_policy_checked", Bool),
                f("Password policy", "Enforce password expiration", "l.is_expiration_checked", Bool),
                f("Password policy", "Locked out", "LOGINPROPERTY(p.name, 'IsLocked')", Bool),
                f("Password policy", "Password last set", "LOGINPROPERTY(p.name, 'PasswordLastSetTime')", Date),
            ],
            &format!(
                "FROM sys.server_principals p LEFT JOIN sys.sql_logins l ON l.principal_id = p.principal_id WHERE p.name = {}",
                quote_literal(name)
            ),
        ),
        _ => return None,
    })
}

/// EXECUTE AS for a module joined as `m`.
fn execute_as(d: &str) -> String {
    format!(
        "CASE WHEN m.object_id IS NULL THEN NULL WHEN m.execute_as_principal_id IS NULL THEN N'CALLER' \
         WHEN m.execute_as_principal_id = -2 THEN N'OWNER' \
         ELSE (SELECT dp.name FROM {d}.sys.database_principals dp WHERE dp.principal_id = m.execute_as_principal_id) END"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_kinds() -> Vec<NodeKind> {
        let (db, schema, name) = ("sh]op".to_string(), "dbo".to_string(), "O'Brien".to_string());
        let object_id = object_id(&db, &schema, &name);
        vec![
            NodeKind::Server,
            NodeKind::Database { db: db.clone() },
            NodeKind::Table { db: db.clone(), schema: schema.clone(), name: name.clone() },
            NodeKind::View { db: db.clone(), schema: schema.clone(), name: name.clone() },
            NodeKind::Procedure { db: db.clone(), schema: schema.clone(), name: name.clone() },
            NodeKind::Function { db: db.clone(), schema: schema.clone(), name: name.clone(), table_valued: false },
            NodeKind::Column { db: db.clone(), object_id: object_id.clone(), name: name.clone() },
            NodeKind::Index { db: db.clone(), object_id, name: name.clone() },
            NodeKind::Login { name },
        ]
    }

    #[test]
    fn every_object_kind_has_a_query() {
        for kind in all_kinds() {
            let q = query(&kind).unwrap_or_else(|| panic!("no query for {kind:?}"));
            assert!(q.sql.starts_with("SET QUOTED_IDENTIFIER ON;\nSELECT "));
            // Names are quoted, never pasted in raw.
            assert!(!q.sql.contains("O'Brien"), "{kind:?}");
            assert!(!q.sql.contains("[sh]op]"), "{kind:?}");
        }
        assert!(query(&NodeKind::TablesFolder { db: "x".into() }).is_none());
        assert!(query(&NodeKind::Leaf).is_none());
    }

    #[test]
    fn identifiers_are_escaped() {
        let q = query(&NodeKind::Table { db: "sh]op".into(), schema: "dbo".into(), name: "O'Brien".into() }).unwrap();
        assert!(q.sql.contains("[sh]]op].sys.tables"));
        assert!(q.sql.contains("OBJECT_ID(N'[sh]]op].[dbo].[O''Brien]')"));
        let q = query(&NodeKind::Login { name: "O'Brien".into() }).unwrap();
        assert!(q.sql.contains("WHERE p.name = N'O''Brien'"));
    }

    #[test]
    fn parse_formats_values() {
        let q = query(&NodeKind::Column { db: "d".into(), object_id: "1".into(), name: "c".into() }).unwrap();
        let mut row: Vec<Cell> = vec![None; q.fields.len()];
        row[0] = Some("Note".into());
        row[1] = Some("12345".into());
        row[2] = Some("nvarchar|200|0|0".into());
        row[3] = Some("1".into());
        let props = q.parse(&[row]).unwrap();
        assert_eq!(props.len(), q.fields.len());
        assert_eq!(props[0], Property { category: "General", label: "Name", value: "Note".into() });
        assert_eq!(props[1].value, "12,345");
        assert_eq!(props[2].value, "nvarchar(100)");
        assert_eq!(props[3].value, "True");
        assert_eq!(props[4].value, "");
        assert!(q.parse(&[]).is_err());
    }

    #[test]
    fn formats() {
        assert_eq!(format_value(Some("0"), Fmt::Bool), "False");
        assert_eq!(format_value(Some("1280"), Fmt::Pages), "10.00");
        assert_eq!(format_value(Some("-1234567"), Fmt::Int), "-1,234,567");
        assert_eq!(format_value(Some("999"), Fmt::Int), "999");
        assert_eq!(format_value(Some("table"), Fmt::Type), "table");
        assert_eq!(format_value(None, Fmt::Int), "");
    }
}
