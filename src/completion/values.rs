//! Experimental value suggestions: in `WHERE c.City = 'ma`, the column's most common values
//! containing what was typed. This module decides where that applies, which column it is, the
//! bounded query that reads the values, how they rank and how they are inserted. Fetching and
//! caching are in `app::completion_popup::values`.

use std::ops::Range;

use super::{Clause, Column, Context, Item, Kind, Literal, Metadata, ObjKind, TableRef, context};
use crate::sql::{self, Tok, quote_ident, quote_literal};

/// At most this many values are offered (the `TOP` of the value query). A result with fewer
/// rows holds every matching value of the sample, so it also answers longer typed texts.
pub const LIMIT: usize = 25;
/// `SET LOCK_TIMEOUT` for the value query: never wait long behind writers.
pub const LOCK_TIMEOUT_MS: u32 = 1000;

/// The comparison the value is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    /// `=`
    Eq,
    /// `<>` or `!=`
    NotEq,
    /// `LIKE` / `NOT LIKE`
    Like,
    /// `IN (` / `NOT IN (`, first or later item.
    In,
}

/// The caret is where a value of a column goes.
#[derive(Debug, Clone, PartialEq)]
pub struct Spot {
    /// Byte range replaced on accept: the literal typed so far (from its opening quote, or its
    /// `N`), or the bare word.
    pub replace: Range<usize>,
    /// What was typed of the value, without quotes and with `''` read as `'`.
    pub typed: String,
    /// The caret is inside a string literal.
    pub quoted: bool,
    pub op: Op,
    /// Byte offset where the operator starts (`IS NULL` replaces from here).
    pub op_start: usize,
    /// The completion context at the end of the left-hand column: its name is `prefix`, its
    /// qualifier the alias or table, and the tables in scope.
    pub column: Context,
}

/// The column whose values are wanted, resolved against the catalog.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub db: String,
    pub schema: String,
    pub table: String,
    pub column: Column,
    pub literal: Literal,
}

/// What [`resolve`] found.
#[derive(Debug, Clone, PartialEq)]
pub enum Resolved {
    Column(Target),
    /// A database whose catalog is needed is not loaded yet.
    Missing(Vec<String>),
    /// Not a column whose values can be offered.
    No,
}

/// One distinct value and how often it occurs (in the sample). `None` is NULL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Value {
    pub value: Option<String>,
    pub count: u64,
}

#[derive(Clone, Copy)]
struct T<'a> {
    kind: Tok,
    text: &'a str,
    start: usize,
    end: usize,
}

impl T<'_> {
    fn op(&self, c: &str) -> bool {
        self.kind == Tok::Operator && self.text == c
    }

    fn kw(&self, k: &str) -> bool {
        self.kind == Tok::Keyword && self.text.eq_ignore_ascii_case(k)
    }
}

fn wordish(k: Tok) -> bool {
    matches!(k, Tok::Identifier | Tok::Keyword | Tok::Function)
}

/// Where a value goes at byte offset `caret`, if it does: right of `=`, `<>`, `!=`, `LIKE`,
/// `IN (` or a comma in an `IN (...)` list, with or without an opening quote, and with a column
/// (not an expression, variable or literal) on the left, in an expression clause. Never in a
/// comment, nor in a string that is not such a value.
pub fn spot(text: &str, caret: usize) -> Option<Spot> {
    let mut caret = caret.min(text.len());
    while !text.is_char_boundary(caret) {
        caret -= 1;
    }
    let toks = sql::tokenize(text);
    let here = toks.iter().find(|(_, r)| r.start < caret && caret <= r.end);
    let (replace, typed, quoted) = match here {
        Some((Tok::Comment, r)) => {
            let s = &text[r.clone()];
            let open = s.starts_with("--") || s.len() < 4 || !s.ends_with("*/");
            if caret < r.end || open {
                return None;
            }
            (caret..caret, String::new(), false)
        }
        Some((Tok::String, r)) => {
            let s = &text[r.clone()];
            let open = r.start + if s.starts_with(['N', 'n']) { 2 } else { 1 };
            let closed = super::context::closed_string(s);
            if caret == r.end && closed {
                // Just after a finished literal: nothing more to suggest.
                return None;
            }
            if caret < open {
                return None;
            }
            let typed = &text[open..caret];
            if typed.contains('\n') {
                return None;
            }
            let end = if closed { r.end } else { caret };
            (r.start..end, typed.replace("''", "'"), true)
        }
        Some((k, r)) if wordish(*k) => {
            if text[r.clone()].starts_with(['[', '"']) {
                return None;
            }
            (r.clone(), text[r.start..caret].to_string(), false)
        }
        Some((Tok::Whitespace | Tok::Operator, _)) | None => {
            // A caret at the start of a word replaces that word (like the context's word).
            match toks.iter().find(|(k, r)| r.start == caret && wordish(*k)) {
                Some((_, r)) if !text[r.clone()].starts_with(['[', '"']) => (r.clone(), String::new(), false),
                Some(_) => return None,
                None => (caret..caret, String::new(), false),
            }
        }
        Some(_) => return None,
    };
    let sig: Vec<T> = toks
        .iter()
        .filter(|(k, r)| r.end <= replace.start && !matches!(k, Tok::Whitespace | Tok::Comment))
        .map(|(k, r)| T { kind: *k, text: &text[r.clone()], start: r.start, end: r.end })
        .collect();
    let n = sig.len();
    let at = |i: usize| n.checked_sub(i + 1).map(|j| sig[j]);
    let p = at(0)?;
    // (operator, its first token's index from the end, the column's index from the end)
    let (op, first) = if p.op("=") {
        match at(1) {
            Some(b) if b.end == p.start && b.op("!") => (Op::NotEq, 1),
            Some(b) if b.end == p.start && (b.op("<") || b.op(">")) => return None,
            _ => (Op::Eq, 0),
        }
    } else if p.op(">") {
        match at(1) {
            Some(b) if b.end == p.start && b.op("<") => (Op::NotEq, 1),
            _ => return None,
        }
    } else if p.kw("LIKE") {
        (Op::Like, 0)
    } else if p.op("(") || p.op(",") {
        // Back to the list's `(`, over literals and commas only (not a subquery).
        let mut i = 0;
        while let Some(t) = at(i) {
            if t.op("(") {
                break;
            }
            let item = matches!(t.kind, Tok::String | Tok::Number) || t.op(",") || t.op("-") || t.kw("NULL");
            if !item {
                return None;
            }
            i += 1;
        }
        if !at(i + 1).is_some_and(|t| t.kw("IN")) {
            return None;
        }
        (Op::In, i + 1)
    } else {
        return None;
    };
    let mut first = first;
    if matches!(op, Op::Like | Op::In) && at(first + 1).is_some_and(|t| t.kw("NOT")) {
        first += 1;
    }
    let op_start = at(first)?.start;
    let col = at(first + 1).filter(|t| wordish(t.kind))?;
    let cx = context(text, col.end)?;
    if cx.clause != Clause::Expression || cx.prefix.is_empty() {
        return None;
    }
    Some(Spot { replace, typed, quoted, op, op_start, column: cx })
}

/// The table `x` names in `cx`: an alias, or a table without one, of the statement (then of
/// the batch's other statements, by alias).
fn by_alias<'a>(cx: &'a Context, x: &str) -> Option<&'a TableRef> {
    let alias = |r: &&TableRef| r.alias.as_deref().is_some_and(|a| a.eq_ignore_ascii_case(x));
    let name = |r: &&TableRef| r.alias.is_none() && r.name().eq_ignore_ascii_case(x);
    cx.tables.iter().find(alias).or_else(|| cx.tables.iter().find(name)).or_else(|| cx.other_tables.iter().find(alias))
}

/// Which column of which user table the spot compares, if its values can be offered.
pub fn resolve(spot: &Spot, meta: &Metadata) -> Resolved {
    let cx = &spot.column;
    let name = cx.prefix.as_str();
    // Candidate (db, schema, table) triples.
    let refs: Vec<(Option<String>, Option<String>, String)> = match cx.qualifier.as_slice() {
        [] => cx
            .tables
            .iter()
            .map(|r| (r.database().map(str::to_string), r.schema().map(str::to_string), r.name().to_string()))
            .collect(),
        [x] => match by_alias(cx, x) {
            Some(r) => vec![(r.database().map(str::to_string), r.schema().map(str::to_string), r.name().to_string())],
            None => vec![(None, None, x.clone())],
        },
        [s, t] => vec![(None, Some(s.clone()), t.clone())],
        [d, s, t] => vec![(Some(d.clone()), Some(s.clone()), t.clone())],
        _ => return Resolved::No,
    };
    let mut missing = Vec::new();
    for (db, schema, table) in refs {
        let db = db.unwrap_or_else(|| meta.current_db.to_string());
        let Some(cat) = (meta.catalog)(&db) else {
            if !db.is_empty() && !missing.iter().any(|m: &String| m.eq_ignore_ascii_case(&db)) {
                missing.push(db);
            }
            continue;
        };
        let Some(obj) = cat.find(schema.as_deref(), &table) else { continue };
        let Some(col) = cat.columns(&obj.schema, &obj.name).iter().find(|c| c.name.eq_ignore_ascii_case(name)) else {
            continue;
        };
        // Views can be arbitrarily expensive to read; only plain tables.
        return match (obj.kind, col.literal) {
            (ObjKind::Table, Some(literal)) => Resolved::Column(Target {
                db: meta.databases.iter().find(|d| d.eq_ignore_ascii_case(&db)).cloned().unwrap_or(db),
                schema: obj.schema.clone(),
                table: obj.name.clone(),
                column: col.clone(),
                literal,
            }),
            _ => Resolved::No,
        };
    }
    if missing.is_empty() { Resolved::No } else { Resolved::Missing(missing) }
}

/// `%`, `_` and `[` made literal for a LIKE pattern, by bracketing them.
pub fn like_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '%' | '_' | '[' => {
                out.push('[');
                out.push(c);
                out.push(']');
            }
            _ => out.push(c),
        }
    }
    out
}

/// A string literal for a column of this kind: `N'...'` or `'...'`, quotes doubled.
pub fn literal(s: &str, kind: Literal) -> String {
    match kind {
        Literal::Unicode => quote_literal(s),
        Literal::Ansi => format!("'{}'", s.replace('\'', "''")),
    }
}

/// `[db].[schema].[table]`.
fn table_name(t: &Target) -> String {
    format!("{}.{}.{}", quote_ident(&t.db), quote_ident(&t.schema), quote_ident(&t.table))
}

/// The table's row count from `sys.partitions` (heap or clustered index), named through the
/// database so the session's current database never matters.
pub fn size_query(t: &Target) -> String {
    format!(
        "SELECT CAST(ISNULL(SUM(p.rows), 0) AS bigint) FROM {}.sys.partitions p \
         WHERE p.object_id = OBJECT_ID({}) AND p.index_id IN (0, 1)",
        quote_ident(&t.db),
        quote_literal(&table_name(t)),
    )
}

/// The most frequent values of the column containing `typed` (case-insensitively), prefix
/// matches first, among the first `cap` rows read; at most [`LIMIT`] of them. With nothing
/// typed, NULL is among them when the column has NULLs.
pub fn value_query(t: &Target, typed: &str, cap: u64) -> String {
    let col = quote_ident(&t.column.name);
    let cap = cap.max(1);
    let esc = like_escape(typed);
    // With nothing typed there is no prefix rank (an ORDER BY constant would be a column position).
    let (filter, first) = if typed.is_empty() {
        (String::new(), String::new())
    } else {
        let contains = literal(&format!("%{esc}%"), t.literal);
        let prefix = literal(&format!("{esc}%"), t.literal);
        (
            format!(" WHERE UPPER(s.v) LIKE UPPER({contains})"),
            format!("CASE WHEN UPPER(s.v) LIKE UPPER({prefix}) THEN 0 ELSE 1 END, "),
        )
    };
    format!(
        "SET LOCK_TIMEOUT {LOCK_TIMEOUT_MS}; \
         SELECT TOP ({LIMIT}) s.v, COUNT_BIG(*) AS n \
         FROM (SELECT TOP ({cap}) {col} AS v FROM {table}) AS s{filter} \
         GROUP BY s.v ORDER BY {first}n DESC, s.v",
        table = table_name(t),
    )
}

/// Rows of [`value_query`] as values.
pub fn from_rows(rows: &[Vec<crate::db::Cell>]) -> Vec<Value> {
    rows.iter()
        .map(|r| Value {
            value: r.first().cloned().flatten(),
            count: r.get(1).cloned().flatten().and_then(|n| n.trim().parse().ok()).unwrap_or(0),
        })
        .collect()
}

/// The values containing `typed` (ignoring case), prefix matches first, then the most frequent,
/// then alphabetically. NULL only matches nothing typed.
pub fn rank(values: &[Value], typed: &str) -> Vec<Value> {
    let typed = typed.to_lowercase();
    let mut out: Vec<(u8, String, &Value)> = values
        .iter()
        .filter_map(|v| match &v.value {
            None => typed.is_empty().then(|| (0, String::new(), v)),
            Some(s) => {
                let l = s.to_lowercase();
                if l.starts_with(&typed) {
                    Some((0, l, v))
                } else if l.contains(&typed) {
                    Some((1, l, v))
                } else {
                    None
                }
            }
        })
        .collect();
    out.sort_by(|a, b| (a.0, std::cmp::Reverse(a.2.count), &a.1).cmp(&(b.0, std::cmp::Reverse(b.2.count), &b.1)));
    out.into_iter().map(|(_, _, v)| v.clone()).collect()
}

/// `1,204`, or `~1,204` when the counts are from a sample.
pub fn count_label(n: u64, approximate: bool) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if approximate { format!("~{out}") } else { out }
}

/// What accepting `value` inserts at `spot`: the literal for the column's type (in a LIKE, with
/// its wildcards made literal). NULL becomes `IS NULL` / `IS NOT NULL`, only after `=` / `<>`.
pub fn insert_for(value: &Value, t: &Target, spot: &Spot) -> Option<(String, Option<Range<usize>>)> {
    match &value.value {
        Some(v) if spot.op == Op::Like => Some((literal(&like_escape(v), t.literal), None)),
        Some(v) => Some((literal(v, t.literal), None)),
        None => {
            let insert = match spot.op {
                Op::Eq => "IS NULL",
                Op::NotEq => "IS NOT NULL",
                Op::Like | Op::In => return None,
            };
            (t.column.nullable).then(|| (insert.to_string(), Some(spot.op_start..spot.replace.end)))
        }
    }
}

/// Completion items for ranked `values`.
pub fn items(values: &[Value], t: &Target, spot: &Spot, approximate: bool) -> Vec<Item> {
    values
        .iter()
        .filter_map(|v| {
            let (insert, replace) = insert_for(v, t, spot)?;
            let name = match &v.value {
                Some(s) => s.replace(['\r', '\n', '\t'], " "),
                None => insert.clone(),
            };
            Some(Item { name, kind: Kind::Value, detail: count_label(v.count, approximate), insert, replace })
        })
        .collect()
}

#[cfg(test)]
mod tests;
