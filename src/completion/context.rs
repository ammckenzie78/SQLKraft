//! Where the caret is: the word, its qualifier, the clause and the tables in scope.

use std::ops::Range;

use crate::sql::{self, Tok};


/// What kind of thing belongs where the caret is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Clause {
    /// After FROM / JOIN / UPDATE / INTO / TABLE: tables and views, and table-valued functions
    /// when `tvf` (FROM, JOIN, APPLY).
    Objects { tvf: bool },
    /// After EXEC / EXECUTE.
    Procedures,
    /// After USE.
    Databases,
    /// In a SELECT list, WHERE, ON, GROUP BY, ORDER BY, SET, ...: columns first.
    Expression,
    /// Anywhere else: keywords and functions.
    General,
}

/// A table (or view, or table-valued function) named in the statement, with its alias.
#[derive(Debug, Clone, PartialEq)]
pub struct TableRef {
    /// `[db.][schema.]name`, unquoted; an empty part is `db..name`.
    pub parts: Vec<String>,
    pub alias: Option<String>,
}

impl TableRef {
    pub fn name(&self) -> &str {
        self.parts.last().map(String::as_str).unwrap_or("")
    }

    pub fn schema(&self) -> Option<&str> {
        (self.parts.len() >= 2).then(|| self.parts[self.parts.len() - 2].as_str()).filter(|s| !s.is_empty())
    }

    pub fn database(&self) -> Option<&str> {
        (self.parts.len() >= 3).then(|| self.parts[self.parts.len() - 3].as_str()).filter(|s| !s.is_empty())
    }
}

/// Where the caret is, as far as completion cares.
#[derive(Debug, Clone, PartialEq)]
pub struct Context {
    /// Byte range of the whole word under the caret; accepting replaces it.
    pub word: Range<usize>,
    /// The part of the word before the caret, unquoted: what is matched.
    pub prefix: String,
    /// The word is in `[...]` (or `"..."`), so whatever is inserted is bracketed.
    pub bracketed: bool,
    /// The dotted names before the word: `db.schema.|` is `["db", "schema"]`.
    pub qualifier: Vec<String>,
    pub clause: Clause,
    /// Tables of the caret's statement.
    pub tables: Vec<TableRef>,
    /// Tables of the batch's other statements, nearest first: only for resolving `x.` when the
    /// statement itself has no `x`.
    pub other_tables: Vec<TableRef>,
}

/// A significant token of the batch.
#[derive(Debug, Clone, Copy)]
struct T<'a> {
    kind: Tok,
    text: &'a str,
    start: usize,
    end: usize,
}

impl T<'_> {
    fn kw(&self, k: &str) -> bool {
        self.kind == Tok::Keyword && self.text.eq_ignore_ascii_case(k)
    }

    fn kw_in(&self, ks: &[&str]) -> bool {
        self.kind == Tok::Keyword && ks.iter().any(|k| self.text.eq_ignore_ascii_case(k))
    }

    fn ident(&self, w: &str) -> bool {
        self.kind == Tok::Identifier && self.text.eq_ignore_ascii_case(w)
    }

    fn op(&self, c: &str) -> bool {
        self.kind == Tok::Operator && self.text == c
    }
}

fn wordish(k: Tok) -> bool {
    matches!(k, Tok::Identifier | Tok::Keyword | Tok::Function)
}

fn is_dot(t: Option<&T>) -> bool {
    t.is_some_and(|t| t.op("."))
}

/// `[a]]b]` → `a]b`, `"a"` → `a`, `a` → `a`.
pub fn unquote(s: &str) -> String {
    if let Some(inner) = s.strip_prefix('[') {
        inner.strip_suffix(']').unwrap_or(inner).replace("]]", "]")
    } else if let Some(inner) = s.strip_prefix('"') {
        inner.strip_suffix('"').unwrap_or(inner).to_string()
    } else {
        s.to_string()
    }
}

/// A string token that has its closing quote.
fn closed_string(s: &str) -> bool {
    let inner = s.strip_prefix(['N', 'n']).unwrap_or(s);
    let Some(inner) = inner.strip_prefix('\'') else { return true };
    let trailing = inner.chars().rev().take_while(|&c| c == '\'').count();
    trailing % 2 == 1
}

/// A `GO` line: `GO` alone on its line, optionally with a count and a comment.
fn is_go(text: &str, t: &T) -> bool {
    if !t.kw("GO") {
        return false;
    }
    let line_start = text[..t.start].rfind('\n').map_or(0, |i| i + 1);
    let line_end = text[t.end..].find('\n').map_or(text.len(), |i| t.end + i);
    let rest = &text[t.end..line_end];
    let rest = rest.find("--").map_or(rest, |i| &rest[..i]);
    text[line_start..t.start].trim().is_empty() && rest.trim().chars().all(|c| c.is_ascii_digit())
}

const STATEMENT_START: &[&str] = &[
    "SELECT", "INSERT", "UPDATE", "DELETE", "MERGE", "CREATE", "ALTER", "DROP", "DECLARE", "IF", "WHILE", "EXEC",
    "EXECUTE", "USE", "PRINT", "RETURN", "TRUNCATE", "BEGIN", "GRANT", "DENY", "REVOKE",
];

/// Statement number of each token. `;` ends a statement; so does a statement keyword that
/// cannot continue the current one (`SELECT` after `UNION` or in `INSERT ... SELECT` doesn't).
fn statements(sig: &[T]) -> Vec<usize> {
    let mut ids = Vec::with_capacity(sig.len());
    let (mut id, mut depth, mut first) = (0usize, 0i32, None::<usize>);
    for (i, t) in sig.iter().enumerate() {
        if t.op(";") {
            ids.push(id);
            id += 1;
            depth = 0;
            first = None;
            continue;
        }
        if depth == 0 && first.is_some() && t.kw_in(STATEMENT_START) {
            let prev = &sig[i - 1];
            let in_insert = first.is_some_and(|f| sig[f].kw("INSERT"));
            let continues = if t.kw("SELECT") || t.kw("EXEC") || t.kw("EXECUTE") {
                in_insert || prev.kw_in(&["UNION", "ALL", "EXCEPT", "INTERSECT", "AS"]) || prev.op(")") || prev.op("(")
            } else {
                prev.kw_in(&["ON", "FOR", "OF", "AFTER", "GRANT", "DENY", "REVOKE"]) || prev.op(",")
            };
            if !continues {
                id += 1;
                first = None;
            }
        }
        if t.op("(") {
            depth += 1;
        } else if t.op(")") {
            depth = (depth - 1).max(0);
        }
        first.get_or_insert(i);
        ids.push(id);
    }
    ids
}

/// The table at `sig[j]`, and where it ends.
fn parse_ref(sig: &[T], j: usize) -> Option<(Option<TableRef>, usize)> {
    let first = sig.get(j)?;
    let mut k = j + 1;
    let mut parts = Vec::new();
    if first.op("(") {
        k = skip_parens(sig, j);
    } else if matches!(first.kind, Tok::Identifier | Tok::Function) {
        parts.push(unquote(first.text));
        while is_dot(sig.get(k)) {
            k += 1;
            match sig.get(k) {
                Some(t) if t.op(".") => parts.push(String::new()),
                Some(t) if wordish(t.kind) => {
                    parts.push(unquote(t.text));
                    k += 1;
                }
                _ => break,
            }
        }
        if sig.get(k).is_some_and(|t| t.op("(")) {
            k = skip_parens(sig, k);
        }
    } else {
        return None;
    }
    let mut alias = None;
    match sig.get(k) {
        Some(t) if t.kw("AS") => {
            if let Some(a) = sig.get(k + 1).filter(|a| a.kind == Tok::Identifier) {
                alias = Some(unquote(a.text));
                k += 2;
            }
        }
        Some(t) if t.kind == Tok::Identifier && !["APPLY", "USING", "TABLESAMPLE"].iter().any(|w| t.ident(w)) => {
            alias = Some(unquote(t.text));
            k += 1;
        }
        _ => {}
    }
    if parts.len() > 3 {
        parts.drain(..parts.len() - 3);
    }
    Some(((!parts.is_empty()).then_some(TableRef { parts, alias }), k))
}

/// The index after the `)` matching the `(` at `sig[j]`.
fn skip_parens(sig: &[T], j: usize) -> usize {
    let mut depth = 0;
    for (i, t) in sig.iter().enumerate().skip(j) {
        if t.op("(") {
            depth += 1;
        } else if t.op(")") {
            depth -= 1;
            if depth == 0 {
                return i + 1;
            }
        }
    }
    sig.len()
}

/// Every table named after FROM / JOIN / UPDATE / INTO / MERGE / APPLY / USING, with the index
/// of its first token.
fn table_refs(sig: &[T]) -> Vec<(usize, TableRef)> {
    let mut out = Vec::new();
    for (i, t) in sig.iter().enumerate() {
        let from = t.kw("FROM");
        if !(from || t.kw_in(&["JOIN", "UPDATE", "INTO", "MERGE"]) || t.ident("APPLY") || t.ident("USING")) {
            continue;
        }
        let mut j = i + 1;
        while let Some((r, next)) = parse_ref(sig, j) {
            if let Some(r) = r {
                out.push((j, r));
            }
            j = next;
            if !(from && sig.get(j).is_some_and(|t| t.op(","))) {
                break;
            }
            j += 1;
        }
    }
    out
}

/// What kind of thing goes after `before` (the statement's tokens up to the word).
fn clause(before: &[T]) -> Clause {
    let Some(prev) = before.last() else { return Clause::General };
    if prev.kw_in(&["FROM", "JOIN"]) || prev.ident("APPLY") {
        return Clause::Objects { tvf: true };
    }
    if prev.kw_in(&["UPDATE", "INTO", "TABLE", "MERGE", "DELETE"]) {
        return Clause::Objects { tvf: false };
    }
    if prev.kw_in(&["EXEC", "EXECUTE"]) {
        return Clause::Procedures;
    }
    if prev.kw("USE") {
        return Clause::Databases;
    }
    // The nearest keyword at this nesting level (an unmatched `(` is looked through).
    let mut depth = 0;
    for t in before.iter().rev() {
        if t.op(")") {
            depth += 1;
        } else if t.op("(") {
            depth = (depth - 1).max(0);
        } else if depth == 0 && t.kind == Tok::Keyword {
            if t.kw("FROM") {
                // `FROM a x, |`
                return if prev.op(",") { Clause::Objects { tvf: true } } else { Clause::General };
            }
            let statement = [
                "JOIN", "UPDATE", "INTO", "TABLE", "EXEC", "EXECUTE", "USE", "MERGE", "DELETE", "INSERT", "CREATE", "ALTER",
                "DROP", "DECLARE", "BEGIN", "END", "GO", "PROC", "PROCEDURE", "VIEW", "FUNCTION", "TRIGGER", "INDEX",
            ];
            return if t.kw_in(&statement) { Clause::General } else { Clause::Expression };
        }
    }
    Clause::General
}

/// The completion context at byte offset `caret`, or `None` where nothing should be offered
/// (inside a string, a comment, a number or a variable).
pub fn context(text: &str, caret: usize) -> Option<Context> {
    let mut caret = caret.min(text.len());
    while !text.is_char_boundary(caret) {
        caret -= 1;
    }
    let toks = sql::tokenize(text);
    for (k, r) in &toks {
        let s = &text[r.clone()];
        let inside = r.start < caret && caret < r.end;
        let at_end = r.start < caret && caret == r.end;
        match k {
            Tok::String if inside || (at_end && !closed_string(s)) => return None,
            Tok::Comment if inside || (at_end && (s.starts_with("--") || s.len() < 4 || !s.ends_with("*/"))) => return None,
            Tok::Number | Tok::Variable if inside || at_end => return None,
            _ => {}
        }
    }

    // The word under the caret, whole (a caret mid-word replaces all of it).
    let wi = toks
        .iter()
        .position(|(k, r)| wordish(*k) && r.start < caret && caret <= r.end)
        .or_else(|| toks.iter().position(|(k, r)| wordish(*k) && r.start == caret));
    let (word, prefix, bracketed) = match wi {
        Some(i) => {
            let r = toks[i].1.clone();
            let s = &text[r.clone()];
            match s.chars().next() {
                Some(q @ ('[' | '"')) => {
                    let close = if q == '[' { ']' } else { '"' };
                    let end = if s.len() > 1 && s.ends_with(close) { r.end - 1 } else { r.end };
                    let inner_start = r.start + 1;
                    let inner = if caret > r.start { &text[inner_start..caret.min(end).max(inner_start)] } else { "" };
                    (r, inner.replace("]]", "]"), true)
                }
                _ => (r.clone(), text[r.start..caret.max(r.start)].to_string(), false),
            }
        }
        None => (caret..caret, String::new(), false),
    };

    // Dotted parts before the word: `a.b.|`.
    let mut qualifier = Vec::new();
    let mut qstart = word.start;
    let mut j = toks.iter().position(|(_, r)| r.end == word.start && r.start < word.start);
    while let Some(d) = j {
        let (k, r) = &toks[d];
        if !(*k == Tok::Operator && &text[r.clone()] == ".") {
            break;
        }
        qstart = r.start;
        match d.checked_sub(1).map(|p| (p, &toks[p])) {
            Some((p, (k, r))) if wordish(*k) => {
                qualifier.push(unquote(&text[r.clone()]));
                qstart = r.start;
                j = p.checked_sub(1);
            }
            Some((p, (Tok::Operator, r))) if &text[r.clone()] == "." => {
                qualifier.push(String::new());
                j = Some(p);
            }
            _ => break,
        }
    }
    qualifier.reverse();

    // The batch (between GO lines) and its significant tokens.
    let sig_all: Vec<T> = toks
        .iter()
        .filter(|(k, _)| !matches!(k, Tok::Whitespace | Tok::Comment))
        .map(|(k, r)| T { kind: *k, text: &text[r.clone()], start: r.start, end: r.end })
        .collect();
    let lo = sig_all.iter().rposition(|t| t.end <= qstart && is_go(text, t)).map_or(0, |i| i + 1);
    let hi = sig_all.iter().position(|t| t.start >= word.end && is_go(text, t)).unwrap_or(sig_all.len());
    let sig = &sig_all[lo..hi.max(lo)];

    let ids = statements(sig);
    let before_n = sig.iter().take_while(|t| t.end <= qstart).count();
    let here = match before_n.checked_sub(1) {
        Some(i) if sig[i].op(";") => ids[i] + 1,
        Some(i) => ids[i],
        None => 0,
    };
    let stmt_start = ids.iter().position(|&id| id == here).unwrap_or(before_n).min(before_n);
    let clause = clause(&sig[stmt_start..before_n]);

    let mut tables = Vec::new();
    let mut other: Vec<(usize, TableRef)> = Vec::new();
    for (i, r) in table_refs(sig) {
        // The word being typed is not a table of the statement yet.
        if sig[i].start == qstart && qstart != word.end {
            continue;
        }
        if ids[i] == here {
            tables.push(r);
        } else {
            let distance = i.abs_diff(before_n);
            other.push((distance, r));
        }
    }
    other.sort_by_key(|(d, _)| *d);

    Some(Context {
        word,
        prefix,
        bracketed,
        qualifier,
        clause,
        tables,
        other_tables: other.into_iter().map(|(_, r)| r).collect(),
    })
}
