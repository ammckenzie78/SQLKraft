//! Context-aware completion for the query editor: what the caret is in, which tables are in
//! scope, what to offer, how to rank it and how to insert it, plus the database metadata model
//! and the catalog queries that fill it. No UI: see `app::completion_popup`.

mod catalog;
mod context;
mod suggest;
pub mod values;

pub use catalog::{Catalog, Column, DbObject, Literal, ObjKind, has_ddl};
pub use context::{Clause, Context, TableRef, context, unquote};
pub use suggest::{MAX_ITEMS, Metadata, Suggestions, suggest};

use crate::sql::{KEYWORDS, quote_ident};

/// What an offered item is; shown in the popup next to its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Kind {
    Keyword,
    Function,
    Table,
    View,
    Column,
    Procedure,
    Schema,
    Database,
    /// A column's value (experimental value suggestions).
    Value,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Keyword => "keyword",
            Kind::Function => "function",
            Kind::Table => "table",
            Kind::View => "view",
            Kind::Column => "column",
            Kind::Procedure => "procedure",
            Kind::Schema => "schema",
            Kind::Database => "database",
            Kind::Value => "value",
        }
    }
}

/// One entry in the completion list.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub name: String,
    pub kind: Kind,
    /// A column's type, an object's schema, ...
    pub detail: String,
    /// What replaces the word under the caret when accepted.
    pub insert: String,
    /// Byte range of the text replaced instead of the context's word (`= '` becomes
    /// `IS NULL`).
    pub replace: Option<std::ops::Range<usize>>,
}


/// How well `name` matches what was typed: 0 exact, 1 prefix, 2 segments (`cuid` →
/// `CustomerId`, `ord_li` → `order_lines`), `None` not at all. Case-insensitive.
pub fn score(prefix: &str, name: &str) -> Option<u8> {
    if prefix.is_empty() {
        return Some(1);
    }
    let (p, n) = (prefix.to_lowercase(), name.to_lowercase());
    if p == n {
        Some(0)
    } else if n.starts_with(&p) {
        Some(1)
    } else {
        let segs: Vec<Vec<char>> = segments(name).iter().map(|s| s.to_lowercase().chars().collect()).collect();
        let p: Vec<char> = p.chars().filter(|c| !matches!(c, '_' | ' ')).collect();
        segment_match(&p, &segs).then_some(2)
    }
}

/// Splits `CustomerId`, `order_lines`, `XMLParser2` into `Customer`/`Id`, `order`/`lines`,
/// `XML`/`Parser`/`2`.
pub fn segments(name: &str) -> Vec<String> {
    let chars: Vec<char> = name.chars().collect();
    let mut out = Vec::new();
    let mut cur = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if matches!(c, '_' | ' ' | '-' | '.') {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            continue;
        }
        if let Some(prev) = cur.chars().last() {
            let next_lower = chars.get(i + 1).is_some_and(|n| n.is_lowercase());
            let boundary = (c.is_uppercase() && (prev.is_lowercase() || prev.is_ascii_digit()))
                || (c.is_uppercase() && prev.is_uppercase() && next_lower)
                || (c.is_ascii_digit() != prev.is_ascii_digit());
            if boundary {
                out.push(std::mem::take(&mut cur));
            }
        }
        cur.push(c);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Can `p` be spelled by prefixes of `segs`, in order (segments may be skipped, the first
/// may not)?
fn segment_match(p: &[char], segs: &[Vec<char>]) -> bool {
    fn go(p: &[char], segs: &[Vec<char>], first: bool) -> bool {
        if p.is_empty() {
            return true;
        }
        for (j, seg) in segs.iter().enumerate() {
            if first && j > 0 {
                break;
            }
            let max = seg.len().min(p.len());
            for k in (1..=max).rev() {
                if seg[..k] == p[..k] && go(&p[k..], &segs[j + 1..], false) {
                    return true;
                }
            }
        }
        false
    }
    go(p, segs, true)
}

/// Needs `[...]`: not a regular identifier, or a reserved word.
pub fn needs_quoting(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else { return true };
    if !(first.is_alphabetic() || matches!(first, '_' | '#' | '@')) {
        return true;
    }
    if !chars.all(|c| c.is_alphanumeric() || matches!(c, '_' | '#' | '@' | '$')) {
        return true;
    }
    KEYWORDS.binary_search(&name.to_ascii_uppercase().as_str()).is_ok()
}

fn quote_unless_plain(name: &str) -> String {
    if needs_quoting(name) { quote_ident(name) } else { name.to_string() }
}

#[cfg(test)]
mod tests;
