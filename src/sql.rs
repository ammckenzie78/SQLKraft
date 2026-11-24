//! T-SQL text utilities: batch splitting on `GO`, quoting, and a tokenizer for highlighting.

#[derive(Debug, Clone, PartialEq)]
pub struct Batch {
    pub text: String,
    /// 0-based line in the editor where the batch starts; added to server error line numbers.
    pub start_line: usize,
    /// `GO 5` runs the batch five times.
    pub repeat: u32,
}

/// Wraps an identifier in brackets, escaping `]`.
pub fn quote_ident(name: &str) -> String {
    format!("[{}]", name.replace(']', "]]"))
}

/// An N'...' literal with quotes escaped.
pub fn quote_literal(s: &str) -> String {
    format!("N'{}'", s.replace('\'', "''"))
}

/// Splits a script into batches separated by lines containing only `GO [count]`, ignoring `GO`
/// inside strings and block comments, like sqlcmd/SSMS.
pub fn split_batches(script: &str, line_offset: usize) -> Vec<Batch> {
    let mut batches = Vec::new();
    let mut current = String::new();
    let mut start_line = line_offset;
    let mut in_block_comment = 0usize; // block comments nest in T-SQL
    let mut in_string = false;
    let mut in_bracket = false;

    for (idx, line) in script.split_inclusive('\n').enumerate() {
        let line_no = line_offset + idx;
        if in_block_comment == 0 && !in_string && !in_bracket {
            if let Some(repeat) = go_line(line) {
                push_batch(&mut batches, &mut current, start_line, repeat);
                start_line = line_no + 1;
                continue;
            }
        }
        // Advance lexical state across this line.
        let b = line.as_bytes();
        let mut i = 0;
        while i < b.len() {
            let c = b[i];
            let next = b.get(i + 1).copied();
            if in_block_comment > 0 {
                if c == b'*' && next == Some(b'/') {
                    in_block_comment -= 1;
                    i += 1;
                } else if c == b'/' && next == Some(b'*') {
                    in_block_comment += 1;
                    i += 1;
                }
            } else if in_string {
                if c == b'\'' {
                    if next == Some(b'\'') {
                        i += 1;
                    } else {
                        in_string = false;
                    }
                }
            } else if in_bracket {
                if c == b']' {
                    if next == Some(b']') {
                        i += 1;
                    } else {
                        in_bracket = false;
                    }
                }
            } else {
                match (c, next) {
                    (b'-', Some(b'-')) => break,
                    (b'/', Some(b'*')) => {
                        in_block_comment += 1;
                        i += 1;
                    }
                    (b'\'', _) => in_string = true,
                    (b'[', _) => in_bracket = true,
                    _ => {}
                }
            }
            i += 1;
        }
        current.push_str(line);
    }
    push_batch(&mut batches, &mut current, start_line, 1);
    batches
}

fn push_batch(batches: &mut Vec<Batch>, current: &mut String, start_line: usize, repeat: u32) {
    let text = std::mem::take(current);
    if !text.trim().is_empty() {
        batches.push(Batch { text, start_line, repeat });
    }
}

/// `GO`, `go 3`, `GO -- comment` → Some(repeat).
fn go_line(line: &str) -> Option<u32> {
    let line = match line.find("--") {
        Some(i) => &line[..i],
        None => line,
    };
    let mut parts = line.split_whitespace();
    let first = parts.next()?;
    if !first.eq_ignore_ascii_case("go") {
        return None;
    }
    let repeat = match parts.next() {
        None => 1,
        Some(n) => n.parse::<u32>().ok().filter(|n| *n > 0)?,
    };
    if parts.next().is_some() {
        return None;
    }
    Some(repeat)
}

// ---------------------------------------------------------------------------------------------
// Tokenizer (for syntax highlighting)
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tok {
    Keyword,
    Function,
    Identifier,
    String,
    Number,
    Comment,
    Variable,
    Operator,
    Whitespace,
}

/// Splits `text` into (kind, byte range) tokens. Never fails; unknown bytes become operators.
pub fn tokenize(text: &str) -> Vec<(Tok, std::ops::Range<usize>)> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let start = i;
        let c = b[i];
        let next = b.get(i + 1).copied();
        let kind = if c.is_ascii_whitespace() {
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            Tok::Whitespace
        } else if c == b'-' && next == Some(b'-') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            Tok::Comment
        } else if c == b'/' && next == Some(b'*') {
            let mut depth = 0usize;
            while i < b.len() {
                if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
                    depth += 1;
                    i += 2;
                } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
                    depth -= 1;
                    i += 2;
                    if depth == 0 {
                        break;
                    }
                } else {
                    i += 1;
                }
            }
            Tok::Comment
        } else if c == b'\'' || ((c == b'N' || c == b'n') && next == Some(b'\'')) {
            i += if c == b'\'' { 1 } else { 2 };
            while i < b.len() {
                if b[i] == b'\'' {
                    if b.get(i + 1) == Some(&b'\'') {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            Tok::String
        } else if c == b'[' {
            while i < b.len() {
                if b[i] == b']' {
                    if b.get(i + 1) == Some(&b']') {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            Tok::Identifier
        } else if c == b'"' {
            i += 1;
            while i < b.len() && b[i] != b'"' {
                i += 1;
            }
            i = (i + 1).min(b.len());
            Tok::Identifier
        } else if c == b'@' {
            i += 1;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'@') {
                i += 1;
            }
            Tok::Variable
        } else if c.is_ascii_digit() || (c == b'.' && next.is_some_and(|n| n.is_ascii_digit())) {
            if c == b'0' && matches!(next, Some(b'x' | b'X')) {
                i += 2;
                while i < b.len() && b[i].is_ascii_hexdigit() {
                    i += 1;
                }
            } else {
                while i < b.len() && (b[i].is_ascii_digit() || b[i] == b'.') {
                    i += 1;
                }
                if i < b.len() && matches!(b[i], b'e' | b'E') {
                    i += 1;
                    if i < b.len() && matches!(b[i], b'+' | b'-') {
                        i += 1;
                    }
                    while i < b.len() && b[i].is_ascii_digit() {
                        i += 1;
                    }
                }
            }
            Tok::Number
        } else if c.is_ascii_alphabetic() || c == b'_' || c == b'#' || c >= 0x80 {
            while i < b.len()
                && (b[i].is_ascii_alphanumeric() || matches!(b[i], b'_' | b'#' | b'$') || b[i] >= 0x80)
            {
                i += 1;
            }
            // Never split a UTF-8 sequence.
            while !text.is_char_boundary(i) {
                i += 1;
            }
            let word = &text[start..i];
            if is_keyword(word) {
                Tok::Keyword
            } else if is_function(word) {
                Tok::Function
            } else {
                Tok::Identifier
            }
        } else {
            i += 1;
            while !text.is_char_boundary(i) {
                i += 1;
            }
            Tok::Operator
        };
        out.push((kind, start..i));
    }
    out
}

fn is_keyword(w: &str) -> bool {
    let up = w.to_ascii_uppercase();
    KEYWORDS.binary_search(&up.as_str()).is_ok()
}

fn is_function(w: &str) -> bool {
    let up = w.to_ascii_uppercase();
    FUNCTIONS.binary_search(&up.as_str()).is_ok()
}

/// Sorted for binary search.
pub const KEYWORDS: &[&str] = &[
    "ADD", "ALL", "ALTER", "AND", "ANY", "AS", "ASC", "AUTHORIZATION", "BACKUP", "BEGIN",
    "BETWEEN", "BIGINT", "BINARY", "BIT", "BREAK", "BROWSE", "BULK", "BY", "CASCADE", "CASE",
    "CATCH", "CHAR", "CHECK", "CHECKPOINT", "CLOSE", "CLUSTERED", "COLLATE", "COLUMN", "COMMIT",
    "COMPUTE", "CONSTRAINT", "CONTAINS", "CONTINUE", "CREATE", "CROSS", "CURRENT", "CURSOR",
    "DATABASE", "DATE", "DATETIME", "DATETIME2", "DATETIMEOFFSET", "DBCC", "DEALLOCATE",
    "DECIMAL", "DECLARE", "DEFAULT", "DELETE", "DENY", "DESC", "DISTINCT", "DROP", "ELSE", "END",
    "ESCAPE", "EXCEPT", "EXEC", "EXECUTE", "EXISTS", "EXIT", "FETCH", "FILE", "FLOAT", "FOR",
    "FOREIGN", "FROM", "FULL", "FUNCTION", "GO", "GOTO", "GRANT", "GROUP", "HAVING", "IDENTITY",
    "IF", "IMAGE", "IN", "INDEX", "INNER", "INSERT", "INT", "INTERSECT", "INTO", "IS", "JOIN",
    "KEY", "KILL", "LEFT", "LIKE", "MAX", "MERGE", "MONEY", "NCHAR", "NOCHECK", "NOCOUNT",
    "NOLOCK", "NONCLUSTERED", "NOT", "NTEXT", "NULL", "NUMERIC", "NVARCHAR", "OF", "OFF",
    "OFFSET", "ON", "OPEN", "OPTION", "OR", "ORDER", "OUTER", "OUTPUT", "OVER", "PARTITION",
    "PERCENT", "PIVOT", "PRIMARY", "PRINT", "PROC", "PROCEDURE", "RAISERROR", "READ", "REAL",
    "REFERENCES", "RESTORE", "RETURN", "RETURNS", "REVOKE", "RIGHT", "ROLLBACK", "ROWS",
    "SCHEMA", "SELECT", "SET", "SMALLDATETIME", "SMALLINT", "SMALLMONEY", "SYSNAME", "TABLE",
    "TEXT", "THEN", "THROW", "TIES", "TIME", "TINYINT", "TO", "TOP", "TRAN", "TRANSACTION",
    "TRIGGER", "TRUNCATE", "TRY", "UNION", "UNIQUE", "UNIQUEIDENTIFIER", "UNPIVOT", "UPDATE",
    "USE", "VALUES", "VARBINARY", "VARCHAR", "VIEW", "WAITFOR", "WHEN", "WHERE", "WHILE", "WITH",
    "XML",
];

pub const FUNCTIONS: &[&str] = &[
    "ABS", "AVG", "CAST", "CEILING", "CHARINDEX", "COALESCE", "CONCAT", "CONVERT", "COUNT",
    "COUNT_BIG", "DATEADD", "DATEDIFF", "DATENAME", "DATEPART", "DB_ID", "DB_NAME", "DENSE_RANK",
    "EOMONTH", "FLOOR", "FORMAT", "GETDATE", "GETUTCDATE", "IIF", "ISNULL", "JSON_VALUE", "LAG",
    "LEAD", "LEN", "LOWER", "LTRIM", "MIN", "NEWID", "NTILE", "NULLIF", "OBJECT_ID",
    "OBJECT_NAME", "RANK", "REPLACE", "ROUND", "ROW_NUMBER", "RTRIM", "SCOPE_IDENTITY",
    "STRING_AGG", "STRING_SPLIT", "STUFF", "SUBSTRING", "SUM", "SYSDATETIME", "TRIM",
    "TRY_CAST", "TRY_CONVERT", "UPPER",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_lists_sorted() {
        assert!(KEYWORDS.windows(2).all(|w| w[0] < w[1]));
        assert!(FUNCTIONS.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn splits_on_go() {
        let s = "select 1\nGO\nselect 2\n  go 3  \nselect 3";
        let b = split_batches(s, 0);
        assert_eq!(b.len(), 3);
        assert_eq!(b[0].text, "select 1\n");
        assert_eq!(b[1], Batch { text: "select 2\n".into(), start_line: 2, repeat: 3 });
        assert_eq!(b[2].start_line, 4);
    }

    #[test]
    fn go_inside_comment_or_string_is_ignored() {
        let s = "select 'a\nGO\nb'\n/* \nGO\n*/\nselect 1\ngo";
        let b = split_batches(s, 0);
        assert_eq!(b.len(), 1);
        assert!(b[0].text.contains("select 1"));
    }

    #[test]
    fn not_go() {
        assert_eq!(go_line("GOTO label"), None);
        assert_eq!(go_line("go x"), None);
        assert_eq!(go_line("Go -- run it"), Some(1));
    }

    #[test]
    fn tokens() {
        let t = tokenize("SELECT [a]], b], N'it''s', @x -- hi\n");
        let kinds: Vec<_> = t.iter().filter(|(k, _)| *k != Tok::Whitespace).map(|(k, _)| *k).collect();
        assert_eq!(
            kinds,
            vec![Tok::Keyword, Tok::Identifier, Tok::Operator, Tok::String, Tok::Operator, Tok::Variable, Tok::Comment]
        );
    }

    #[test]
    fn tokenize_unicode_does_not_panic() {
        let t = tokenize("select N'héllo', ñame, 日本 → x");
        let total: usize = t.iter().map(|(_, r)| r.len()).sum();
        assert_eq!(total, "select N'héllo', ñame, 日本 → x".len());
    }

    #[test]
    fn quoting() {
        assert_eq!(quote_ident("a]b"), "[a]]b]");
        assert_eq!(quote_literal("o'neil"), "N'o''neil'");
    }
}
