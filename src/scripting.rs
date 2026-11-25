//! Script as CREATE / ALTER / DROP, using the best syntax the target server supports.
//!
//! - `CREATE OR ALTER` for views, procedures, functions and triggers needs SQL Server 2016 SP1
//!   (13.0.4001) or Azure SQL. Older servers get a stub-then-`ALTER` script instead.
//! - `DROP ... IF EXISTS` needs SQL Server 2016 (13.0). Older servers get an `OBJECT_ID` check.

use serde::{Deserialize, Serialize};

use crate::explorer::NodeKind;
use crate::sql::{Tok, quote_ident, quote_literal, tokenize};

/// What the user asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptMode {
    Create,
    Alter,
    Drop,
    DropAndCreate,
}

impl ScriptMode {
    pub fn label(self) -> &'static str {
        match self {
            ScriptMode::Create => "Script as CREATE",
            ScriptMode::Alter => "Script as ALTER",
            ScriptMode::Drop => "Script as DROP",
            ScriptMode::DropAndCreate => "Script as DROP and CREATE",
        }
    }
}

/// Which server the generated scripts are meant to run on (a setting).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ScriptTarget {
    /// Whatever server the object came from.
    #[default]
    Connected,
    /// SQL Server 2012, 2014 or 2016 RTM: no `CREATE OR ALTER`.
    Legacy,
    /// SQL Server 2016 SP1 or later, or Azure SQL.
    Modern,
}

impl ScriptTarget {
    pub fn label(self) -> &'static str {
        match self {
            ScriptTarget::Connected => "Connected server",
            ScriptTarget::Legacy => "SQL Server 2012 – 2016 RTM",
            ScriptTarget::Modern => "SQL Server 2016 SP1+ / Azure SQL",
        }
    }
}

/// The syntax features a script may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Syntax {
    pub create_or_alter: bool,
    pub drop_if_exists: bool,
}

impl Syntax {
    pub const LEGACY: Syntax = Syntax { create_or_alter: false, drop_if_exists: false };
    pub const MODERN: Syntax = Syntax { create_or_alter: true, drop_if_exists: true };

    /// From `SERVERPROPERTY('ProductVersion')` and `SERVERPROPERTY('EngineEdition')`.
    pub fn for_server(product_version: &str, engine_edition: i32) -> Syntax {
        // Azure SQL Database (5) and Managed Instance (8) report 12.0.x but support everything.
        if matches!(engine_edition, 5 | 8) {
            return Syntax::MODERN;
        }
        let mut parts = product_version.split('.').map(|p| p.trim().parse::<u32>().unwrap_or(0));
        let major = parts.next().unwrap_or(0);
        let _minor = parts.next();
        let build = parts.next().unwrap_or(0);
        if major == 0 {
            // Unknown: the conservative syntax runs everywhere.
            return Syntax::LEGACY;
        }
        Syntax {
            create_or_alter: major > 13 || (major == 13 && build >= 4001),
            drop_if_exists: major >= 13,
        }
    }

    pub fn for_target(target: ScriptTarget, product_version: &str, engine_edition: i32) -> Syntax {
        match target {
            ScriptTarget::Connected => Syntax::for_server(product_version, engine_edition),
            ScriptTarget::Legacy => Syntax::LEGACY,
            ScriptTarget::Modern => Syntax::MODERN,
        }
    }
}

/// `sys.objects.type` codes for modules.
fn type_keyword(kind: &NodeKind) -> &'static str {
    match kind {
        NodeKind::Table { .. } => "TABLE",
        NodeKind::View { .. } => "VIEW",
        NodeKind::Procedure { .. } => "PROCEDURE",
        NodeKind::Function { .. } => "FUNCTION",
        _ => "",
    }
}

fn header(kind: &NodeKind) -> String {
    format!("USE {};\nGO\n\n", quote_ident(kind.database().unwrap_or("master")))
}

/// The DROP statement for an object, with no `USE`/`GO` around it.
pub fn drop_statement(kind: &NodeKind, syntax: Syntax) -> Option<String> {
    let name = kind.object_name()?;
    let ty = type_keyword(kind);
    Some(if syntax.drop_if_exists {
        format!("DROP {ty} IF EXISTS {name};")
    } else {
        format!("IF OBJECT_ID({}) IS NOT NULL\n    DROP {ty} {name};", quote_literal(&name))
    })
}

/// Script as DROP for any object.
pub fn drop_script(kind: &NodeKind, syntax: Syntax) -> Option<String> {
    Some(format!("{}{}\nGO\n", header(kind), drop_statement(kind, syntax)?))
}

/// Script as DROP and CREATE for a table, given its CREATE TABLE script (which starts with
/// its own `USE`/`GO`).
pub fn drop_and_create_table(kind: &NodeKind, create_script: &str, syntax: Syntax) -> Option<String> {
    let drop = drop_statement(kind, syntax)?;
    let body = create_script.strip_prefix(&header(kind)).unwrap_or(create_script);
    Some(format!("{}{drop}\nGO\n\n{body}", header(kind)))
}

/// Builds the script for a view, procedure or function from its stored definition
/// (`sys.sql_modules.definition`) and `sys.objects.type` (`V`, `P`, `FN`, `IF`, `TF`, `TR`).
pub fn module_script(kind: &NodeKind, definition: &str, object_type: &str, mode: ScriptMode, syntax: Syntax) -> String {
    let def = definition.trim();
    let mut out = header(kind);
    match mode {
        ScriptMode::Create => {
            if syntax.create_or_alter {
                out.push_str(&rewrite_create(def, "CREATE OR ALTER"));
            } else {
                out.push_str(&rewrite_create(def, "CREATE"));
            }
        }
        ScriptMode::Alter => {
            if syntax.create_or_alter {
                out.push_str(&rewrite_create(def, "CREATE OR ALTER"));
            } else {
                if let Some(stub) = stub(kind, object_type) {
                    out.push_str(&stub);
                    out.push_str("\nGO\n\n");
                }
                out.push_str(&rewrite_create(def, "ALTER"));
            }
        }
        ScriptMode::Drop => {
            if let Some(d) = drop_statement(kind, syntax) {
                out.push_str(&d);
            }
        }
        ScriptMode::DropAndCreate => {
            if let Some(d) = drop_statement(kind, syntax) {
                out.push_str(&d);
                out.push_str("\nGO\n\n");
            }
            out.push_str(&rewrite_create(def, "CREATE"));
        }
    }
    out.push_str("\nGO\n");
    out
}

/// Creates a placeholder object when it doesn't exist, so the `ALTER` that follows works on
/// servers without `CREATE OR ALTER`. The placeholder has the same object type: SQL Server can't
/// `ALTER` a scalar function into a table-valued one, or an inline function into a
/// multi-statement one.
fn stub(kind: &NodeKind, object_type: &str) -> Option<String> {
    let name = kind.object_name()?;
    let create = match object_type.trim() {
        "P" => format!("CREATE PROCEDURE {name} AS RETURN;"),
        "V" => format!("CREATE VIEW {name} AS SELECT 1 AS stub;"),
        "FN" => format!("CREATE FUNCTION {name}() RETURNS int AS BEGIN RETURN NULL; END;"),
        "IF" => format!("CREATE FUNCTION {name}() RETURNS TABLE AS RETURN SELECT 1 AS stub;"),
        "TF" => format!("CREATE FUNCTION {name}() RETURNS @t TABLE (stub int) AS BEGIN RETURN; END;"),
        _ => return None,
    };
    Some(format!(
        "IF OBJECT_ID({}) IS NULL\n    EXEC({});",
        quote_literal(&name),
        quote_literal(&create)
    ))
}

/// Replaces the leading `CREATE` (or `CREATE OR ALTER`, or `ALTER`) of a module definition with
/// `verb`, leaving comments, casing and everything after the object type untouched.
pub fn rewrite_create(definition: &str, verb: &str) -> String {
    let words: Vec<(String, std::ops::Range<usize>)> = tokenize(definition)
        .into_iter()
        .filter(|(t, _)| !matches!(t, Tok::Whitespace | Tok::Comment))
        .take(4)
        .map(|(_, r)| (definition[r.clone()].to_ascii_uppercase(), r))
        .collect();
    let w = |i: usize| words.get(i).map(|(s, _)| s.as_str());
    // Index of the object type word (PROC, PROCEDURE, VIEW, FUNCTION, TRIGGER).
    let type_at = match (w(0), w(1), w(2)) {
        (Some("CREATE"), Some("OR"), Some("ALTER")) => 3,
        (Some("CREATE" | "ALTER"), _, _) => 1,
        _ => return definition.to_string(),
    };
    let Some((_, ty)) = words.get(type_at) else { return definition.to_string() };
    let start = words[0].1.start;
    format!("{}{verb} {}", &definition[..start], &definition[ty.start..])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn proc() -> NodeKind {
        NodeKind::Procedure { db: "shop".into(), schema: "dbo".into(), name: "GetOrders".into() }
    }

    #[test]
    fn syntax_from_version() {
        assert_eq!(Syntax::for_server("11.0.7001.0", 3), Syntax::LEGACY); // 2012
        assert_eq!(Syntax::for_server("12.0.6024.0", 3), Syntax::LEGACY); // 2014
        assert_eq!(Syntax::for_server("13.0.1601.5", 3), Syntax { create_or_alter: false, drop_if_exists: true }); // 2016 RTM
        assert_eq!(Syntax::for_server("13.0.4001.0", 3), Syntax::MODERN); // 2016 SP1
        assert_eq!(Syntax::for_server("15.0.2210.4", 3), Syntax::MODERN); // 2019
        assert_eq!(Syntax::for_server("16.0.1000.6", 2), Syntax::MODERN); // 2022
        assert_eq!(Syntax::for_server("12.0.2000.8", 5), Syntax::MODERN); // Azure SQL Database
        assert_eq!(Syntax::for_server("", 0), Syntax::LEGACY);
    }

    #[test]
    fn rewrite_keeps_everything_but_the_verb() {
        assert_eq!(
            rewrite_create("-- header\n/* note */ create   proc dbo.x as select 1", "CREATE OR ALTER"),
            "-- header\n/* note */ CREATE OR ALTER proc dbo.x as select 1"
        );
        assert_eq!(rewrite_create("CREATE FUNCTION [s].[f]() RETURNS int", "ALTER"), "ALTER FUNCTION [s].[f]() RETURNS int");
        assert_eq!(rewrite_create("create or alter view v as select 1", "ALTER"), "ALTER view v as select 1");
        assert_eq!(rewrite_create("ALTER PROCEDURE p AS RETURN", "CREATE"), "CREATE PROCEDURE p AS RETURN");
        // Not a module definition: left alone.
        assert_eq!(rewrite_create("SELECT 1", "ALTER"), "SELECT 1");
    }

    #[test]
    fn alter_on_modern_server_is_create_or_alter() {
        let s = module_script(&proc(), "CREATE PROCEDURE dbo.GetOrders AS SELECT 1", "P", ScriptMode::Alter, Syntax::MODERN);
        assert_eq!(s, "USE [shop];\nGO\n\nCREATE OR ALTER PROCEDURE dbo.GetOrders AS SELECT 1\nGO\n");
    }

    #[test]
    fn alter_on_legacy_server_creates_a_stub_first() {
        let s = module_script(&proc(), "CREATE PROCEDURE dbo.GetOrders AS SELECT 1", "P", ScriptMode::Alter, Syntax::LEGACY);
        assert_eq!(
            s,
            "USE [shop];\nGO\n\n\
             IF OBJECT_ID(N'[dbo].[GetOrders]') IS NULL\n    EXEC(N'CREATE PROCEDURE [dbo].[GetOrders] AS RETURN;');\nGO\n\n\
             ALTER PROCEDURE dbo.GetOrders AS SELECT 1\nGO\n"
        );
    }

    #[test]
    fn create_uses_create_or_alter_when_available() {
        let def = "CREATE PROCEDURE dbo.GetOrders AS SELECT 1";
        assert!(module_script(&proc(), def, "P", ScriptMode::Create, Syntax::MODERN).contains("CREATE OR ALTER PROCEDURE"));
        assert!(module_script(&proc(), def, "P", ScriptMode::Create, Syntax::LEGACY).contains("\nCREATE PROCEDURE"));
    }

    #[test]
    fn drop_syntax() {
        assert_eq!(drop_statement(&proc(), Syntax::MODERN).unwrap(), "DROP PROCEDURE IF EXISTS [dbo].[GetOrders];");
        assert_eq!(
            drop_statement(&proc(), Syntax::LEGACY).unwrap(),
            "IF OBJECT_ID(N'[dbo].[GetOrders]') IS NOT NULL\n    DROP PROCEDURE [dbo].[GetOrders];"
        );
        let t = NodeKind::Table { db: "shop".into(), schema: "dbo".into(), name: "O'Brien".into() };
        assert_eq!(
            drop_statement(&t, Syntax::LEGACY).unwrap(),
            "IF OBJECT_ID(N'[dbo].[O''Brien]') IS NOT NULL\n    DROP TABLE [dbo].[O'Brien];"
        );
    }

    #[test]
    fn drop_and_create_table_keeps_one_use() {
        let t = NodeKind::Table { db: "shop".into(), schema: "dbo".into(), name: "T".into() };
        let create = "USE [shop];\nGO\n\nCREATE TABLE [dbo].[T]\n(\n    [a] int NULL\n);\nGO\n";
        assert_eq!(
            drop_and_create_table(&t, create, Syntax::MODERN).unwrap(),
            "USE [shop];\nGO\n\nDROP TABLE IF EXISTS [dbo].[T];\nGO\n\nCREATE TABLE [dbo].[T]\n(\n    [a] int NULL\n);\nGO\n"
        );
    }
}
