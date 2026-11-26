//! The metadata completion offers: one database's schemas, objects and columns, and the
//! catalog queries that load it.

use std::collections::HashMap;

use crate::db::Cell;
use crate::explorer::type_spec;
use crate::sql::{self, Tok, quote_ident};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ObjKind {
    Table,
    View,
    Procedure,
    ScalarFunction,
    TableFunction,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DbObject {
    pub schema: String,
    pub name: String,
    pub kind: ObjKind,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    pub name: String,
    /// `nvarchar(100), not null`
    pub type_name: String,
    pub nullable: bool,
    /// How a value of this column is written as a literal, when its values can be suggested.
    pub literal: Option<Literal>,
}

/// String literal style for a column whose values can be suggested: character types that are
/// not `(max)` and not computed-without-being-persisted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Literal {
    /// `'...'` for char / varchar.
    Ansi,
    /// `N'...'` for nchar / nvarchar (and sysname).
    Unicode,
}

impl Literal {
    /// `ty` is the system type name, `max_length` as in `sys.columns` (-1 for `(max)`).
    pub fn of(ty: &str, max_length: &str, computed_unpersisted: bool) -> Option<Self> {
        if computed_unpersisted || max_length.trim() == "-1" {
            return None;
        }
        match ty.to_ascii_lowercase().as_str() {
            "char" | "varchar" => Some(Literal::Ansi),
            "nchar" | "nvarchar" => Some(Literal::Unicode),
            _ => None,
        }
    }
}

/// One database's schemas, objects and columns.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    pub schemas: Vec<String>,
    pub objects: Vec<DbObject>,
    /// Keyed by lower-cased (schema, object).
    columns: HashMap<(String, String), Vec<Column>>,
}

impl Catalog {
    /// Schemas, objects and columns of `db`, always named through `[db].sys...` so the result
    /// never depends on the session's current database.
    pub fn queries(db: &str) -> [String; 3] {
        let d = quote_ident(db);
        [
            format!("SELECT s.name FROM {d}.sys.schemas s WHERE s.schema_id < 16384 ORDER BY s.name"),
            format!(
                "SELECT s.name, o.name, RTRIM(o.type) FROM {d}.sys.objects o \
                 JOIN {d}.sys.schemas s ON s.schema_id = o.schema_id \
                 WHERE o.type IN ('U','V','P','PC','FN','FS','IF','TF','FT') AND o.is_ms_shipped = 0 \
                 ORDER BY s.name, o.name"
            ),
            format!(
                "SELECT s.name, o.name, c.name, ty.name, c.max_length, c.precision, c.scale, c.is_nullable, \
                 bt.name, CASE WHEN c.is_computed = 1 AND ISNULL(cc.is_persisted, 0) = 0 THEN 1 ELSE 0 END \
                 FROM {d}.sys.columns c JOIN {d}.sys.objects o ON o.object_id = c.object_id \
                 JOIN {d}.sys.schemas s ON s.schema_id = o.schema_id \
                 JOIN {d}.sys.types ty ON ty.user_type_id = c.user_type_id \
                 LEFT JOIN {d}.sys.types bt ON bt.user_type_id = c.system_type_id \
                 LEFT JOIN {d}.sys.computed_columns cc ON cc.object_id = c.object_id AND cc.column_id = c.column_id \
                 WHERE o.type IN ('U','V','IF','TF','FT') AND o.is_ms_shipped = 0 \
                 ORDER BY s.name, o.name, c.column_id"
            ),
        ]
    }

    /// Builds a catalog from the rows of [`Catalog::queries`], in order.
    pub fn from_rows(schemas: Vec<Vec<Cell>>, objects: Vec<Vec<Cell>>, columns: Vec<Vec<Cell>>) -> Self {
        let s = |r: &[Cell], i: usize| r.get(i).cloned().flatten().unwrap_or_default();
        let objects = objects
            .iter()
            .filter_map(|r| {
                let kind = match s(r, 2).trim() {
                    "U" => ObjKind::Table,
                    "V" => ObjKind::View,
                    "P" | "PC" => ObjKind::Procedure,
                    "FN" | "FS" => ObjKind::ScalarFunction,
                    "IF" | "TF" | "FT" => ObjKind::TableFunction,
                    _ => return None,
                };
                Some(DbObject { schema: s(r, 0), name: s(r, 1), kind })
            })
            .collect();
        let mut map: HashMap<(String, String), Vec<Column>> = HashMap::new();
        for r in &columns {
            let ty = type_spec(&s(r, 3), &s(r, 4), &s(r, 5), &s(r, 6));
            let nullable = s(r, 7) == "1";
            let null = if nullable { "null" } else { "not null" };
            // The system type behind an alias type; older rows (and tests) may not have it.
            let base = Some(s(r, 8)).filter(|b| !b.is_empty()).unwrap_or_else(|| s(r, 3));
            let literal = Literal::of(&base, &s(r, 4), s(r, 9) == "1");
            map.entry((s(r, 0).to_lowercase(), s(r, 1).to_lowercase()))
                .or_default()
                .push(Column { name: s(r, 2), type_name: format!("{ty}, {null}"), nullable, literal });
        }
        Self { schemas: schemas.iter().map(|r| s(r, 0)).collect(), objects, columns: map }
    }

    pub fn has_schema(&self, schema: &str) -> bool {
        self.schemas.iter().any(|s| s.eq_ignore_ascii_case(schema))
    }

    /// An object by name; without a schema, `dbo` wins, then any schema.
    pub fn find(&self, schema: Option<&str>, name: &str) -> Option<&DbObject> {
        let named = |o: &&DbObject| o.name.eq_ignore_ascii_case(name);
        match schema.filter(|s| !s.is_empty()) {
            Some(s) => self.objects.iter().filter(named).find(|o| o.schema.eq_ignore_ascii_case(s)),
            None => {
                let mut it = self.objects.iter().filter(named);
                let first = it.clone().next();
                it.find(|o| o.schema.eq_ignore_ascii_case("dbo")).or(first)
            }
        }
    }

    pub fn columns(&self, schema: &str, name: &str) -> &[Column] {
        self.columns.get(&(schema.to_lowercase(), name.to_lowercase())).map(Vec::as_slice).unwrap_or(&[])
    }
}

/// True when `script` may change the schema (CREATE / ALTER / DROP, `SELECT ... INTO`,
/// `sp_rename`), so cached metadata should be reloaded after it runs.
pub fn has_ddl(script: &str) -> bool {
    let toks = sql::tokenize(script);
    let mut prev: Option<&str> = None;
    for (k, r) in &toks {
        let w = &script[r.clone()];
        match k {
            Tok::Whitespace | Tok::Comment => continue,
            Tok::Keyword if ["CREATE", "ALTER", "DROP"].iter().any(|d| w.eq_ignore_ascii_case(d)) => return true,
            Tok::Keyword
                if w.eq_ignore_ascii_case("INTO")
                    && !prev.is_some_and(|p| p.eq_ignore_ascii_case("INSERT") || p.eq_ignore_ascii_case("MERGE")) =>
            {
                return true;
            }
            Tok::Identifier if w.eq_ignore_ascii_case("sp_rename") => return true,
            _ => {}
        }
        prev = Some(w);
    }
    false
}
