//! What to offer in a [`Context`], best first, and what to insert.

use super::{Catalog, Clause, Context, Item, Kind, ObjKind, TableRef, needs_quoting, quote_unless_plain, score};
use crate::sql::{FUNCTIONS, KEYWORDS, quote_ident};

/// What [`suggest`] knows about the connection.
pub struct Metadata<'a> {
    /// The tab's database.
    pub current_db: &'a str,
    /// Every database on the server.
    pub databases: &'a [String],
    /// A database's catalog, if loaded.
    pub catalog: &'a dyn Fn(&str) -> Option<&'a Catalog>,
}

#[derive(Debug, Default)]
pub struct Suggestions {
    pub items: Vec<Item>,
    /// Databases whose catalog would have been used but is not loaded (yet).
    pub missing: Vec<String>,
}

pub const MAX_ITEMS: usize = 200;

struct Builder<'c, 'm> {
    ctx: &'c Context,
    meta: &'c Metadata<'m>,
    out: Vec<(u8, Item)>,
    missing: Vec<String>,
}

impl<'m> Builder<'_, 'm> {
    fn catalog(&mut self, db: &str) -> Option<&'m Catalog> {
        let c = (self.meta.catalog)(db);
        if c.is_none() && !self.missing.iter().any(|m| m.eq_ignore_ascii_case(db)) {
            self.missing.push(db.to_string());
        }
        c
    }

    /// The real name of a database the server has, if `name` is one.
    fn database(&self, name: &str) -> Option<String> {
        self.meta.databases.iter().find(|d| d.eq_ignore_ascii_case(name)).cloned()
    }

    fn quote(&self, name: &str) -> String {
        if self.ctx.bracketed || needs_quoting(name) { quote_ident(name) } else { name.to_string() }
    }

    fn push(&mut self, prio: u8, kind: Kind, name: &str, detail: String, insert: String) {
        self.out.push((prio, Item { name: name.to_string(), kind, detail, insert, replace: None }));
    }

    fn columns_of(&mut self, db: Option<&str>, schema: Option<&str>, name: &str, prio: u8) {
        let db = db.unwrap_or(self.meta.current_db).to_string();
        let Some(cat) = self.catalog(&db) else { return };
        let Some(obj) = cat.find(schema, name) else { return };
        for c in cat.columns(&obj.schema, &obj.name) {
            let insert = self.quote(&c.name);
            self.push(prio, Kind::Column, &c.name, format!("{} · {}", c.type_name, obj.name), insert);
        }
    }

    /// Objects of `cat` fitting the clause; `schema` limits them to one schema (and inserts
    /// bare names), otherwise non-dbo objects are inserted schema-qualified.
    fn objects(&mut self, cat: &Catalog, schema: Option<&str>, prio: u8) {
        let clause = self.ctx.clause;
        for o in &cat.objects {
            if schema.is_some_and(|s| !o.schema.eq_ignore_ascii_case(s)) {
                continue;
            }
            let fits = match clause {
                Clause::Objects { tvf } => {
                    matches!(o.kind, ObjKind::Table | ObjKind::View) || (tvf && o.kind == ObjKind::TableFunction)
                }
                Clause::Procedures => o.kind == ObjKind::Procedure,
                Clause::Expression => o.kind == ObjKind::ScalarFunction || schema.is_some(),
                Clause::General | Clause::Databases => schema.is_some(),
            };
            if !fits {
                continue;
            }
            let (kind, detail) = match o.kind {
                ObjKind::Table => (Kind::Table, o.schema.clone()),
                ObjKind::View => (Kind::View, o.schema.clone()),
                ObjKind::Procedure => (Kind::Procedure, o.schema.clone()),
                ObjKind::ScalarFunction => (Kind::Function, o.schema.clone()),
                ObjKind::TableFunction => (Kind::Function, format!("{} · table-valued", o.schema)),
            };
            // Scalar functions must be called schema-qualified.
            let qualify =
                schema.is_none() && (!o.schema.eq_ignore_ascii_case("dbo") || o.kind == ObjKind::ScalarFunction);
            let insert = if qualify {
                format!("{}.{}", quote_unless_plain(&o.schema), self.quote(&o.name))
            } else {
                self.quote(&o.name)
            };
            self.push(prio, kind, &o.name, detail, insert);
        }
    }

    fn schemas(&mut self, cat: &Catalog, db: &str, prio: u8) {
        for s in &cat.schemas {
            let insert = self.quote(s);
            self.push(prio, Kind::Schema, s, db.to_string(), insert);
        }
    }

    fn databases(&mut self, prio: u8) {
        for d in self.meta.databases {
            let insert = self.quote(d);
            self.push(prio, Kind::Database, d, String::new(), insert);
        }
    }

    fn words(&mut self, prio_fn: u8, prio_kw: u8) {
        let lower = {
            let letters: Vec<char> = self.ctx.prefix.chars().filter(|c| c.is_alphabetic()).collect();
            !letters.is_empty() && letters.iter().all(|c| c.is_lowercase())
        };
        let case = |w: &str| if lower { w.to_lowercase() } else { w.to_string() };
        for f in FUNCTIONS {
            self.push(prio_fn, Kind::Function, f, "built-in".into(), case(f));
        }
        for k in KEYWORDS {
            self.push(prio_kw, Kind::Keyword, k, String::new(), case(k));
        }
    }

    fn resolve_alias(&self, x: &str) -> Option<TableRef> {
        let ctx = self.ctx;
        let by_alias = |r: &&TableRef| r.alias.as_deref().is_some_and(|a| a.eq_ignore_ascii_case(x));
        let by_name = |r: &&TableRef| r.alias.is_none() && r.name().eq_ignore_ascii_case(x);
        ctx.tables
            .iter()
            .find(by_alias)
            .or_else(|| ctx.tables.iter().find(by_name))
            .or_else(|| ctx.other_tables.iter().find(by_alias))
            .cloned()
    }

    fn qualified(&mut self) {
        let q = self.ctx.qualifier.clone();
        let current = self.meta.current_db.to_string();
        match q.as_slice() {
            [x] => {
                if let Some(r) = self.resolve_alias(x) {
                    self.columns_of(r.database(), r.schema(), r.name(), 0);
                }
                if let Some(cat) = self.catalog(&current)
                    && cat.has_schema(x)
                {
                    self.objects(cat, Some(x), 1);
                }
                if let Some(db) = self.database(x)
                    && let Some(cat) = self.catalog(&db)
                {
                    self.schemas(cat, &db, 2);
                }
            }
            [a, b] => {
                let schema = if b.is_empty() { "dbo" } else { b.as_str() };
                if let Some(db) = self.database(a)
                    && let Some(cat) = self.catalog(&db)
                    && cat.has_schema(schema)
                {
                    self.objects(cat, Some(schema), 0);
                }
                if !b.is_empty()
                    && let Some(cat) = self.catalog(&current)
                    && cat.has_schema(a)
                {
                    self.columns_of(None, Some(a), b, 1);
                }
            }
            [db, s, o] => {
                if let Some(db) = self.database(db) {
                    self.columns_of(Some(&db), Some(s), o, 0);
                }
            }
            _ => {}
        }
    }

    fn unqualified(&mut self) {
        let current = self.meta.current_db.to_string();
        match self.ctx.clause {
            Clause::Objects { .. } | Clause::Procedures => {
                if let Some(cat) = self.catalog(&current) {
                    self.objects(cat, None, 0);
                    self.schemas(cat, &current, 1);
                }
                self.databases(2);
            }
            Clause::Databases => self.databases(0),
            Clause::Expression => {
                for r in self.ctx.tables.clone() {
                    self.columns_of(r.database(), r.schema(), r.name(), 0);
                }
                if let Some(cat) = self.catalog(&current) {
                    self.objects(cat, None, 1);
                }
                self.words(1, 2);
            }
            Clause::General => self.words(1, 0),
        }
    }
}

/// What to offer in `ctx`, best first.
pub fn suggest(ctx: &Context, meta: &Metadata) -> Suggestions {
    let mut b = Builder { ctx, meta, out: Vec::new(), missing: Vec::new() };
    if ctx.qualifier.is_empty() {
        b.unqualified();
    } else {
        b.qualified();
    }
    let mut scored: Vec<(u8, u8, String, Item)> = b
        .out
        .into_iter()
        .filter_map(|(prio, item)| score(&ctx.prefix, &item.name).map(|s| (s, prio, item.name.to_lowercase(), item)))
        .collect();
    scored.sort_by(|a, b| (a.0, a.1, &a.2).cmp(&(b.0, b.1, &b.2)));
    let mut seen = std::collections::HashSet::new();
    let items = scored
        .into_iter()
        .map(|(_, _, _, i)| i)
        .filter(|i| seen.insert((i.insert.to_lowercase(), i.kind)))
        .take(MAX_ITEMS)
        .collect();
    Suggestions { items, missing: b.missing }
}
