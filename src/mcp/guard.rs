//! Read-only checks for queries Claude runs through the MCP server.
//!
//! Two layers, because neither is enough alone:
//! 1. Reject statements that change data, schema, permissions or server state, found with the
//!    tokenizer so keywords inside strings, comments and `[bracketed]` names don't count.
//! 2. Run whatever passes inside a transaction that is always rolled back
//!    (see [`read_only_batches`]).

use crate::sql::{Batch, Tok, split_batches, tokenize};

/// Words that may not appear in a read-only query.
const DENIED: &[&str] = &[
    // Data changes, including SELECT ... INTO.
    "INSERT", "UPDATE", "DELETE", "MERGE", "TRUNCATE", "INTO", "UPDATETEXT", "WRITETEXT", "BULK",
    // Schema and permissions.
    "CREATE", "ALTER", "DROP", "GRANT", "REVOKE", "DENY", "ENABLE", "DISABLE", "RENAME",
    // Arbitrary code and external data.
    "EXEC", "EXECUTE", "SP_EXECUTESQL", "OPENROWSET", "OPENQUERY", "OPENDATASOURCE", "OPENXML",
    // Transaction control: the guard's own rollback must win.
    "BEGIN", "COMMIT", "ROLLBACK", "SAVE", "TRAN", "TRANSACTION",
    // Server and database state.
    "BACKUP", "RESTORE", "DBCC", "KILL", "SHUTDOWN", "RECONFIGURE", "CHECKPOINT", "SETUSER",
    "USE",
];

/// Why a query was refused, or `None` when it looks read-only.
pub fn check_read_only(sql: &str) -> Option<String> {
    for (tok, range) in tokenize(sql) {
        if matches!(tok, Tok::Comment | Tok::String | Tok::Whitespace | Tok::Number | Tok::Operator | Tok::Variable) {
            continue;
        }
        let word = sql[range].to_ascii_uppercase();
        if word.starts_with('[') || word.starts_with('"') {
            continue;
        }
        if DENIED.contains(&word.as_str()) {
            return Some(format!(
                "`{word}` is not allowed: this connection is read-only for Claude. \
                 Allow writes for it in SQLKraft, or ask the user to run the statement."
            ));
        }
    }
    None
}

/// Wraps the query's batches in a transaction that is always rolled back.
pub fn read_only_batches(sql: &str) -> Vec<Batch> {
    let mut batches = vec![Batch { text: "SET XACT_ABORT OFF; BEGIN TRANSACTION;".into(), start_line: 0, repeat: 1 }];
    batches.extend(split_batches(sql, 0));
    batches.push(Batch { text: "IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION;".into(), start_line: 0, repeat: 1 });
    batches
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_plain_reads() {
        for q in [
            "SELECT * FROM dbo.Orders WHERE Status = 'DELETE'",
            "WITH c AS (SELECT 1 AS x) SELECT x FROM c",
            "SELECT [Update], [Insert Date] FROM dbo.Audit -- DROP TABLE x",
            "/* CREATE */ SELECT COUNT(*) FROM sys.objects",
            "DECLARE @n int = 5; SELECT TOP (@n) name FROM sys.tables",
            "SELECT name FROM sys.databases\nGO\nSELECT 2",
            "SET NOCOUNT ON; SELECT 1",
        ] {
            assert_eq!(check_read_only(q), None, "{q}");
        }
    }

    #[test]
    fn refuses_writes_and_escapes() {
        for (q, word) in [
            ("DELETE FROM dbo.Orders", "DELETE"),
            ("select * into #t from dbo.Orders", "INTO"),
            ("update dbo.Orders set x = 1", "UPDATE"),
            ("EXEC sp_who", "EXEC"),
            ("exec('select 1')", "EXEC"),
            ("SELECT 1; COMMIT", "COMMIT"),
            ("DROP TABLE dbo.Orders", "DROP"),
            ("SELECT * FROM OPENROWSET('SQLNCLI', 'x', 'y')", "OPENROWSET"),
            ("USE master", "USE"),
            ("WITH x AS (SELECT 1 a) MERGE dbo.T USING x ON 1=0 WHEN NOT MATCHED THEN INSERT (a) VALUES (a);", "MERGE"),
        ] {
            let why = check_read_only(q).unwrap_or_else(|| panic!("allowed: {q}"));
            assert!(why.contains(&format!("`{word}`")), "{q}: {why}");
        }
    }

    #[test]
    fn wraps_in_a_rolled_back_transaction() {
        let b = read_only_batches("SELECT 1\nGO\nSELECT 2");
        assert_eq!(b.len(), 4);
        assert!(b[0].text.contains("BEGIN TRANSACTION"));
        assert!(b[3].text.contains("ROLLBACK"));
    }
}
