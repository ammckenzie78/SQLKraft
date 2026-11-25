//! Object Explorer: a lazily-loaded tree per connected server.

use tokio::sync::oneshot;

use crate::db::{Cell, FetchResult, Session};
use crate::sql::{quote_ident, quote_literal};

#[derive(Debug, Clone, PartialEq)]
pub enum NodeKind {
    Server,
    DatabasesFolder,
    SystemDatabasesFolder,
    Database { db: String },
    TablesFolder { db: String },
    ViewsFolder { db: String },
    ProgrammabilityFolder { db: String },
    ProceduresFolder { db: String },
    FunctionsFolder { db: String },
    Table { db: String, schema: String, name: String },
    View { db: String, schema: String, name: String },
    Procedure { db: String, schema: String, name: String },
    Function { db: String, schema: String, name: String, table_valued: bool },
    ColumnsFolder { db: String, object_id: String },
    IndexesFolder { db: String, object_id: String },
    SecurityFolder,
    LoginsFolder,
    Leaf,
}

impl NodeKind {
    pub fn icon(&self) -> &'static str {
        match self {
            NodeKind::Server => "🖥",
            NodeKind::Database { .. } => "🛢",
            NodeKind::Table { .. } => "▦",
            NodeKind::View { .. } => "👁",
            NodeKind::Procedure { .. } => "⚙",
            NodeKind::Function { .. } => "ƒ",
            NodeKind::Leaf => "•",
            _ => "📁",
        }
    }

    pub fn database(&self) -> Option<&str> {
        match self {
            NodeKind::Database { db }
            | NodeKind::TablesFolder { db }
            | NodeKind::ViewsFolder { db }
            | NodeKind::ProgrammabilityFolder { db }
            | NodeKind::ProceduresFolder { db }
            | NodeKind::FunctionsFolder { db }
            | NodeKind::Table { db, .. }
            | NodeKind::View { db, .. }
            | NodeKind::Procedure { db, .. }
            | NodeKind::Function { db, .. }
            | NodeKind::ColumnsFolder { db, .. }
            | NodeKind::IndexesFolder { db, .. } => Some(db),
            _ => None,
        }
    }

    /// `[schema].[name]` for objects.
    pub fn object_name(&self) -> Option<String> {
        match self {
            NodeKind::Table { schema, name, .. }
            | NodeKind::View { schema, name, .. }
            | NodeKind::Procedure { schema, name, .. }
            | NodeKind::Function { schema, name, .. } => {
                Some(format!("{}.{}", quote_ident(schema), quote_ident(name)))
            }
            _ => None,
        }
    }
}

pub enum Children {
    NotLoaded,
    Loading(oneshot::Receiver<FetchResult>),
    Loaded(Vec<Node>),
    Failed(String),
}

/// Status overlay drawn on a node's icon, like the small badges in SSMS's Object Explorer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Badge {
    #[default]
    None,
    /// Database is OFFLINE (or OFFLINE_SECONDARY).
    Offline,
    /// Database is RESTORING.
    Restoring,
    /// Database is SUSPECT, EMERGENCY, RECOVERY_PENDING or RECOVERING.
    Warning,
    /// Database has partial containment; on a server, contained database authentication is on.
    Contained,
}

impl Badge {
    /// Badge for a `sys.databases` row: state wins over containment.
    pub fn for_database(state_desc: &str, containment: &str) -> Badge {
        match state_desc {
            "" | "ONLINE" => {
                if containment == "1" {
                    Badge::Contained
                } else {
                    Badge::None
                }
            }
            "OFFLINE" | "OFFLINE_SECONDARY" => Badge::Offline,
            "RESTORING" => Badge::Restoring,
            _ => Badge::Warning,
        }
    }
}

pub struct Node {
    pub label: String,
    pub kind: NodeKind,
    pub children: Children,
    pub id: u64,
    pub badge: Badge,
}

impl Node {
    pub fn new(label: impl Into<String>, kind: NodeKind) -> Self {
        let children = if kind == NodeKind::Leaf {
            Children::Loaded(Vec::new())
        } else {
            Children::NotLoaded
        };
        Self { label: label.into(), kind, children, id: next_id(), badge: Badge::None }
    }

    fn folder(label: &str, kind: NodeKind) -> Self {
        Self::new(label, kind)
    }

    pub fn is_leaf(&self) -> bool {
        self.kind == NodeKind::Leaf
    }

    /// Kick off loading the children, if needed.
    pub fn ensure_loading(&mut self, session: &Session) {
        if !matches!(self.children, Children::NotLoaded) {
            return;
        }
        match static_children(&self.kind) {
            Some(children) => self.children = Children::Loaded(children),
            None => match children_query(&self.kind) {
                Some(sql) => self.children = Children::Loading(session.fetch(sql)),
                None => self.children = Children::Loaded(Vec::new()),
            },
        }
    }

    /// Poll in-flight loads anywhere in the subtree.
    pub fn poll(&mut self) {
        if let Children::Loading(rx) = &mut self.children {
            match rx.try_recv() {
                Ok(Ok((_, rows))) => {
                    self.children = Children::Loaded(build_children(&self.kind, rows));
                }
                Ok(Err(e)) => self.children = Children::Failed(format!("{e:#}")),
                Err(oneshot::error::TryRecvError::Empty) => {}
                Err(oneshot::error::TryRecvError::Closed) => {
                    self.children = Children::Failed("Connection closed".into())
                }
            }
        }
        if let Children::Loaded(children) = &mut self.children {
            for c in children {
                c.poll();
            }
        }
    }

    pub fn refresh(&mut self) {
        self.children = Children::NotLoaded;
    }
}

fn next_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn static_children(kind: &NodeKind) -> Option<Vec<Node>> {
    Some(match kind {
        NodeKind::Server => vec![
            Node::folder("Databases", NodeKind::DatabasesFolder),
            Node::folder("Security", NodeKind::SecurityFolder),
        ],
        NodeKind::SecurityFolder => vec![Node::folder("Logins", NodeKind::LoginsFolder)],
        NodeKind::Database { db } => vec![
            Node::folder("Tables", NodeKind::TablesFolder { db: db.clone() }),
            Node::folder("Views", NodeKind::ViewsFolder { db: db.clone() }),
            Node::folder("Programmability", NodeKind::ProgrammabilityFolder { db: db.clone() }),
        ],
        NodeKind::ProgrammabilityFolder { db } => vec![
            Node::folder("Stored Procedures", NodeKind::ProceduresFolder { db: db.clone() }),
            Node::folder("Functions", NodeKind::FunctionsFolder { db: db.clone() }),
        ],
        NodeKind::Table { db, schema, name } | NodeKind::View { db, schema, name } => {
            let object_id = format!(
                "OBJECT_ID({})",
                quote_literal(&format!("{}.{}.{}", quote_ident(db), quote_ident(schema), quote_ident(name)))
            );
            let mut v = vec![Node::folder(
                "Columns",
                NodeKind::ColumnsFolder { db: db.clone(), object_id: object_id.clone() },
            )];
            v.push(Node::folder("Indexes", NodeKind::IndexesFolder { db: db.clone(), object_id }));
            v
        }
        _ => return None,
    })
}

fn children_query(kind: &NodeKind) -> Option<String> {
    Some(match kind {
        NodeKind::DatabasesFolder => {
            "SELECT name, CASE WHEN database_id <= 4 THEN 1 ELSE 0 END, state_desc, containment \
             FROM sys.databases ORDER BY name"
                .into()
        }
        NodeKind::SystemDatabasesFolder => {
            "SELECT name, 1, state_desc, containment FROM sys.databases WHERE database_id <= 4 ORDER BY name".into()
        }
        NodeKind::LoginsFolder => {
            "SELECT name, type_desc FROM sys.server_principals \
             WHERE type IN ('S','U','G','E','X') ORDER BY name"
                .into()
        }
        NodeKind::TablesFolder { db } => format!(
            "SELECT s.name, t.name FROM {d}.sys.tables t JOIN {d}.sys.schemas s ON s.schema_id = t.schema_id \
             WHERE t.is_ms_shipped = 0 ORDER BY s.name, t.name",
            d = quote_ident(db)
        ),
        NodeKind::ViewsFolder { db } => format!(
            "SELECT s.name, v.name FROM {d}.sys.views v JOIN {d}.sys.schemas s ON s.schema_id = v.schema_id \
             WHERE v.is_ms_shipped = 0 ORDER BY s.name, v.name",
            d = quote_ident(db)
        ),
        NodeKind::ProceduresFolder { db } => format!(
            "SELECT s.name, p.name FROM {d}.sys.procedures p JOIN {d}.sys.schemas s ON s.schema_id = p.schema_id \
             WHERE p.is_ms_shipped = 0 ORDER BY s.name, p.name",
            d = quote_ident(db)
        ),
        NodeKind::FunctionsFolder { db } => format!(
            "SELECT s.name, o.name, CASE WHEN o.type IN ('IF','TF','FT') THEN 1 ELSE 0 END \
             FROM {d}.sys.objects o JOIN {d}.sys.schemas s ON s.schema_id = o.schema_id \
             WHERE o.type IN ('FN','IF','TF','FS','FT','AF') AND o.is_ms_shipped = 0 ORDER BY s.name, o.name",
            d = quote_ident(db)
        ),
        NodeKind::ColumnsFolder { db, object_id } => format!(
            "SELECT c.name, ty.name, c.max_length, c.precision, c.scale, c.is_nullable, \
                    CASE WHEN EXISTS (SELECT 1 FROM {d}.sys.index_columns ic JOIN {d}.sys.indexes i \
                        ON i.object_id = ic.object_id AND i.index_id = ic.index_id \
                        WHERE i.is_primary_key = 1 AND ic.object_id = c.object_id AND ic.column_id = c.column_id) \
                    THEN 1 ELSE 0 END, c.is_identity, c.is_computed \
             FROM {d}.sys.columns c JOIN {d}.sys.types ty ON ty.user_type_id = c.user_type_id \
             WHERE c.object_id = {object_id} ORDER BY c.column_id",
            d = quote_ident(db)
        ),
        NodeKind::IndexesFolder { db, object_id } => format!(
            "SELECT i.name, i.type_desc, i.is_unique, i.is_primary_key FROM {d}.sys.indexes i \
             WHERE i.object_id = {object_id} AND i.type > 0 ORDER BY i.name",
            d = quote_ident(db)
        ),
        _ => return None,
    })
}

fn s(row: &[Cell], i: usize) -> String {
    row.get(i).cloned().flatten().unwrap_or_default()
}

fn build_children(kind: &NodeKind, rows: Vec<Vec<Cell>>) -> Vec<Node> {
    match kind {
        NodeKind::DatabasesFolder => {
            let mut v = vec![Node::folder("System Databases", NodeKind::SystemDatabasesFolder)];
            for r in rows.iter().filter(|r| s(r, 1) == "0") {
                v.push(database_node(r));
            }
            v
        }
        NodeKind::SystemDatabasesFolder => rows.iter().map(|r| database_node(r)).collect(),
        NodeKind::LoginsFolder => rows
            .iter()
            .map(|r| Node::new(format!("{} ({})", s(r, 0), s(r, 1).to_lowercase().replace('_', " ")), NodeKind::Leaf))
            .collect(),
        NodeKind::TablesFolder { db } => objects(&rows, |schema, name| NodeKind::Table {
            db: db.clone(),
            schema,
            name,
        }),
        NodeKind::ViewsFolder { db } => objects(&rows, |schema, name| NodeKind::View {
            db: db.clone(),
            schema,
            name,
        }),
        NodeKind::ProceduresFolder { db } => objects(&rows, |schema, name| NodeKind::Procedure {
            db: db.clone(),
            schema,
            name,
        }),
        NodeKind::FunctionsFolder { db } => rows
            .iter()
            .map(|r| {
                Node::new(
                    format!("{}.{}", s(r, 0), s(r, 1)),
                    NodeKind::Function {
                        db: db.clone(),
                        schema: s(r, 0),
                        name: s(r, 1),
                        table_valued: s(r, 2) == "1",
                    },
                )
            })
            .collect(),
        NodeKind::ColumnsFolder { .. } => rows
            .iter()
            .map(|r| {
                let ty = type_spec(&s(r, 1), &s(r, 2), &s(r, 3), &s(r, 4));
                let mut extra = Vec::new();
                if s(r, 6) == "1" {
                    extra.push("PK".to_string());
                }
                if s(r, 7) == "1" {
                    extra.push("identity".to_string());
                }
                if s(r, 8) == "1" {
                    extra.push("computed".to_string());
                }
                extra.push(ty);
                extra.push(if s(r, 5) == "1" { "null".into() } else { "not null".into() });
                Node::new(format!("{} ({})", s(r, 0), extra.join(", ")), NodeKind::Leaf)
            })
            .collect(),
        NodeKind::IndexesFolder { .. } => rows
            .iter()
            .map(|r| {
                let mut d = s(r, 1).to_lowercase().replace('_', " ");
                if s(r, 3) == "1" {
                    d = format!("primary key, {d}");
                } else if s(r, 2) == "1" {
                    d = format!("unique, {d}");
                }
                Node::new(format!("{} ({d})", s(r, 0)), NodeKind::Leaf)
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn database_node(r: &[Cell]) -> Node {
    let name = s(r, 0);
    let state = s(r, 2);
    let label = if state.is_empty() || state == "ONLINE" {
        name.clone()
    } else {
        format!("{name} ({})", state.to_lowercase())
    };
    let mut n = Node::new(label, NodeKind::Database { db: name });
    n.badge = Badge::for_database(&state, &s(r, 3));
    if !(state.is_empty() || state == "ONLINE") {
        n.children = Children::Loaded(Vec::new());
    }
    n
}

fn objects(rows: &[Vec<Cell>], f: impl Fn(String, String) -> NodeKind) -> Vec<Node> {
    rows.iter()
        .map(|r| Node::new(format!("{}.{}", s(r, 0), s(r, 1)), f(s(r, 0), s(r, 1))))
        .collect()
}

/// `nvarchar(50)`, `decimal(18,2)`, `varchar(max)` from sys.columns values.
pub fn type_spec(ty: &str, max_length: &str, precision: &str, scale: &str) -> String {
    let len: i32 = max_length.parse().unwrap_or(0);
    match ty {
        "varchar" | "char" | "varbinary" | "binary" => {
            if len == -1 {
                format!("{ty}(max)")
            } else {
                format!("{ty}({len})")
            }
        }
        "nvarchar" | "nchar" => {
            if len == -1 {
                format!("{ty}(max)")
            } else {
                format!("{ty}({})", len / 2)
            }
        }
        "decimal" | "numeric" => format!("{ty}({precision},{scale})"),
        "datetime2" | "time" | "datetimeoffset" => format!("{ty}({scale})"),
        _ => ty.to_string(),
    }
}

// ---------------------------------------------------------------------------------------------
// Scripting
// ---------------------------------------------------------------------------------------------

/// SQL that returns the CREATE script for a node in a single cell, or None if it can't be
/// scripted.
pub fn script_create_query(kind: &NodeKind) -> Option<String> {
    let db = kind.database()?;
    let full = format!("{}.{}", quote_ident(db), kind.object_name()?);
    match kind {
        // OBJECT_DEFINITION resolves the id in the session's current database (usually master
        // for the explorer connection), so read the target database's sys.sql_modules instead.
        // Returns (definition, sys.objects.type); the type picks the stub for Script as ALTER.
        NodeKind::View { .. } | NodeKind::Procedure { .. } | NodeKind::Function { .. } => Some(format!(
            "SELECT m.definition, o.type FROM {d}.sys.sql_modules m \
             JOIN {d}.sys.objects o ON o.object_id = m.object_id WHERE m.object_id = OBJECT_ID({lit})",
            d = quote_ident(db),
            lit = quote_literal(&full)
        )),
        NodeKind::Table { .. } => Some(format!(
            "DECLARE @oid int = OBJECT_ID({lit});\n\
             SELECT c.name, ty.name, c.max_length, c.precision, c.scale, c.is_nullable, c.is_identity, \
                    CAST(IDENT_SEED({lit}) AS nvarchar(40)), CAST(IDENT_INCR({lit}) AS nvarchar(40)), \
                    cc.definition, dc.definition, c.collation_name \
             FROM {d}.sys.columns c \
             JOIN {d}.sys.types ty ON ty.user_type_id = c.user_type_id \
             LEFT JOIN {d}.sys.computed_columns cc ON cc.object_id = c.object_id AND cc.column_id = c.column_id \
             LEFT JOIN {d}.sys.default_constraints dc ON dc.parent_object_id = c.object_id AND dc.parent_column_id = c.column_id \
             WHERE c.object_id = @oid ORDER BY c.column_id;",
            lit = quote_literal(&full),
            d = quote_ident(db)
        )),
        _ => None,
    }
}

/// SQL returning (index name, is_primary_key, is_unique, type_desc, column, is_descending)
/// for a table's PK/unique constraints, run after `script_create_query` for tables.
pub fn script_keys_query(kind: &NodeKind) -> Option<String> {
    let NodeKind::Table { db, .. } = kind else { return None };
    let full = format!("{}.{}", quote_ident(db), kind.object_name()?);
    Some(format!(
        "SELECT i.name, i.is_primary_key, i.is_unique_constraint, i.type_desc, c.name, ic.is_descending_key \
         FROM {d}.sys.indexes i \
         JOIN {d}.sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
         JOIN {d}.sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
         WHERE i.object_id = OBJECT_ID({lit}) AND (i.is_primary_key = 1 OR i.is_unique_constraint = 1) \
           AND ic.is_included_column = 0 \
         ORDER BY i.index_id, ic.key_ordinal",
        lit = quote_literal(&full),
        d = quote_ident(db)
    ))
}

/// Builds a CREATE TABLE script from the two result sets above.
pub fn build_create_table(kind: &NodeKind, cols: &[Vec<Cell>], keys: &[Vec<Cell>]) -> String {
    let NodeKind::Table { db, .. } = kind else { return String::new() };
    let name = kind.object_name().unwrap_or_default();
    let mut out = format!("USE {};\nGO\n\nCREATE TABLE {name}\n(\n", quote_ident(db));
    let mut lines = Vec::new();
    for r in cols {
        let col = quote_ident(&s(r, 0));
        if let Some(expr) = r.get(9).cloned().flatten() {
            lines.push(format!("    {col} AS {expr}"));
            continue;
        }
        let mut line = format!("    {col} {}", type_spec(&s(r, 1), &s(r, 2), &s(r, 3), &s(r, 4)));
        if s(r, 6) == "1" {
            line.push_str(&format!(" IDENTITY({},{})", s(r, 7), s(r, 8)));
        }
        line.push_str(if s(r, 5) == "1" { " NULL" } else { " NOT NULL" });
        if let Some(def) = r.get(10).cloned().flatten() {
            line.push_str(&format!(" DEFAULT {def}"));
        }
        lines.push(line);
    }
    // Group key columns by constraint name, keeping order.
    let mut constraints: Vec<(String, bool, String, Vec<String>)> = Vec::new();
    for r in keys {
        let cname = s(r, 0);
        let col = format!("{}{}", quote_ident(&s(r, 4)), if s(r, 5) == "1" { " DESC" } else { " ASC" });
        match constraints.iter_mut().find(|c| c.0 == cname) {
            Some(c) => c.3.push(col),
            None => constraints.push((cname, s(r, 1) == "1", s(r, 3), vec![col])),
        }
    }
    for (cname, pk, ty, cols) in constraints {
        lines.push(format!(
            "    CONSTRAINT {} {} {} ({})",
            quote_ident(&cname),
            if pk { "PRIMARY KEY" } else { "UNIQUE" },
            ty,
            cols.join(", ")
        ));
    }
    out.push_str(&lines.join(",\n"));
    out.push_str("\n);\nGO\n");
    out
}

/// Turns a view/proc/function definition into an editable script.
pub fn wrap_definition(kind: &NodeKind, definition: &str) -> String {
    let db = kind.database().unwrap_or("master");
    format!("USE {};\nGO\n\n{}\nGO\n", quote_ident(db), definition.trim())
}

pub fn select_top_script(kind: &NodeKind, cols: &[String]) -> Option<String> {
    let db = kind.database()?;
    let name = kind.object_name()?;
    let col_list = if cols.is_empty() {
        "*".to_string()
    } else {
        cols.iter().map(|c| quote_ident(c)).collect::<Vec<_>>().join("\n      ,")
    };
    Some(format!(
        "/****** Script for SelectTopNRows command from SQLKraft ******/\nSELECT TOP (1000) {col_list}\n  FROM {}.{name}\n",
        quote_ident(db)
    ))
}

pub fn column_names_query(kind: &NodeKind) -> Option<String> {
    let db = kind.database()?;
    let full = format!("{}.{}", quote_ident(db), kind.object_name()?);
    Some(format!(
        "SELECT name FROM {}.sys.columns WHERE object_id = OBJECT_ID({}) ORDER BY column_id",
        quote_ident(db),
        quote_literal(&full)
    ))
}

pub fn execute_proc_script(kind: &NodeKind) -> Option<String> {
    let db = kind.database()?;
    let name = kind.object_name()?;
    Some(format!(
        "USE {};\nGO\n\nDECLARE @return_value int;\n\nEXEC @return_value = {name}\n    -- @param = value\n;\n\nSELECT 'Return Value' = @return_value;\nGO\n",
        quote_ident(db)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn database_badges() {
        assert_eq!(Badge::for_database("ONLINE", "0"), Badge::None);
        assert_eq!(Badge::for_database("ONLINE", "1"), Badge::Contained);
        assert_eq!(Badge::for_database("OFFLINE", "1"), Badge::Offline);
        assert_eq!(Badge::for_database("RESTORING", "0"), Badge::Restoring);
        assert_eq!(Badge::for_database("SUSPECT", "0"), Badge::Warning);
        assert_eq!(Badge::for_database("RECOVERY_PENDING", "0"), Badge::Warning);
    }

    #[test]
    fn script_proc_reads_target_database_modules() {
        let kind = NodeKind::Procedure { db: "shop".into(), schema: "dbo".into(), name: "GetOrders".into() };
        let q = script_create_query(&kind).unwrap();
        assert!(q.contains("[shop].sys.sql_modules"), "{q}");
        assert!(q.contains("OBJECT_ID(N'[shop].[dbo].[GetOrders]')") || q.contains("OBJECT_ID('[shop].[dbo].[GetOrders]')"), "{q}");
        assert!(!q.contains("OBJECT_DEFINITION"), "{q}");
    }

    #[test]
    fn types() {
        assert_eq!(type_spec("nvarchar", "100", "0", "0"), "nvarchar(50)");
        assert_eq!(type_spec("varchar", "-1", "0", "0"), "varchar(max)");
        assert_eq!(type_spec("decimal", "9", "18", "2"), "decimal(18,2)");
        assert_eq!(type_spec("int", "4", "10", "0"), "int");
    }

    #[test]
    fn create_table() {
        let kind = NodeKind::Table { db: "shop".into(), schema: "dbo".into(), name: "Orders".into() };
        let c = |v: &[&str]| -> Vec<Cell> {
            v.iter().map(|x| if x.is_empty() { None } else { Some(x.to_string()) }).collect()
        };
        let cols = vec![
            c(&["Id", "int", "4", "10", "0", "0", "1", "1", "1", "", "", ""]),
            c(&["Note", "nvarchar", "200", "0", "0", "1", "0", "", "", "", "(N'')", ""]),
        ];
        let keys = vec![c(&["PK_Orders", "1", "0", "CLUSTERED", "Id", "0"])];
        let sql = build_create_table(&kind, &cols, &keys);
        assert!(sql.contains("CREATE TABLE [dbo].[Orders]"));
        assert!(sql.contains("[Id] int IDENTITY(1,1) NOT NULL"));
        assert!(sql.contains("[Note] nvarchar(100) NULL DEFAULT (N'')"));
        assert!(sql.contains("CONSTRAINT [PK_Orders] PRIMARY KEY CLUSTERED ([Id] ASC)"));
    }
}
