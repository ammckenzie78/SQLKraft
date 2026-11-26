//! The table designer's model: a table's columns as the grid edits them, and the T-SQL that
//! creates it (New Table…) or changes it from how it was loaded (Design). No UI here.
//!
//! The designer never changes anything itself. It writes a script for review, and changes that
//! `ALTER TABLE` can't make (identity changes, moving columns) are refused with a message
//! instead of being turned into a table rebuild.

use crate::db::Cell;
use crate::sql::{quote_ident, quote_literal};

/// Which of length / precision / scale a type takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeParams {
    None,
    /// `varchar(50)`, `nvarchar(max)`
    Length,
    /// `decimal(18,2)`
    PrecisionScale,
    /// `datetime2(3)`
    Scale,
}

pub fn type_params(ty: &str) -> TypeParams {
    match ty.trim().to_ascii_lowercase().as_str() {
        "varchar" | "char" | "varbinary" | "binary" | "nvarchar" | "nchar" => TypeParams::Length,
        "decimal" | "numeric" => TypeParams::PrecisionScale,
        "datetime2" | "time" | "datetimeoffset" => TypeParams::Scale,
        _ => TypeParams::None,
    }
}

/// Types offered in the designer's type list (any other type name can be typed in).
pub const COMMON_TYPES: &[&str] = &[
    "int", "bigint", "smallint", "tinyint", "bit", "decimal", "numeric", "money", "float", "real", "date", "datetime",
    "datetime2", "datetimeoffset", "time", "char", "varchar", "nchar", "nvarchar", "binary", "varbinary",
    "uniqueidentifier", "xml",
];

/// The message for changes that need the table to be rebuilt.
pub const REBUILD: &str = "is not supported yet — it needs a table rebuild";

/// One row of the designer grid.
#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    pub name: String,
    /// The type name alone (`nvarchar`); the sizes are in the next three fields.
    pub data_type: String,
    /// A number or `max`, for the `Length` types.
    pub length: String,
    pub precision: String,
    pub scale: String,
    pub nullable: bool,
    /// The default expression as T-SQL (`(0)`, `getdate()`), or empty for none.
    pub default: String,
    pub identity: bool,
    pub seed: String,
    pub increment: String,
    pub primary_key: bool,
    /// A computed column's expression. Computed columns are shown but can only be renamed or
    /// dropped.
    pub computed: Option<String>,
    /// From the catalog: kept when the column is altered, so ALTER COLUMN doesn't reset it.
    pub collation: Option<String>,
    /// The name of the column's default constraint in the catalog.
    pub default_name: Option<String>,
    /// Position in the existing primary key (1-based) and its sort order.
    pub key_ordinal: Option<u32>,
    pub key_descending: bool,
    /// For Design: the index of this column in the table as loaded; `None` for a new column.
    pub source: Option<usize>,
}

impl Column {
    /// A new column as the designer adds it: `nvarchar(50) NULL`.
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            data_type: "nvarchar".into(),
            length: "50".into(),
            precision: "18".into(),
            scale: "0".into(),
            nullable: true,
            default: String::new(),
            identity: false,
            seed: "1".into(),
            increment: "1".into(),
            primary_key: false,
            computed: None,
            collation: None,
            default_name: None,
            key_ordinal: None,
            key_descending: false,
            source: None,
        }
    }

    /// `nvarchar(50)`, `decimal(18,2)`, `datetime2(3)`, or why the sizes are wrong.
    pub fn type_sql(&self) -> Result<String, String> {
        let ty = self.data_type.trim();
        let who = || format!("Column {}", self.name);
        if ty.is_empty() {
            return Err(format!("{}: choose a data type.", who()));
        }
        match type_params(ty) {
            TypeParams::None => Ok(ty.to_string()),
            TypeParams::Length => {
                let len = self.length.trim();
                if len.eq_ignore_ascii_case("max") {
                    return Ok(format!("{ty}(max)"));
                }
                let limit = if ty.eq_ignore_ascii_case("nvarchar") || ty.eq_ignore_ascii_case("nchar") { 4000 } else { 8000 };
                match len.parse::<u32>() {
                    Ok(n) if (1..=limit).contains(&n) => Ok(format!("{ty}({n})")),
                    _ => Err(format!("{}: {ty} needs a length from 1 to {limit}, or max.", who())),
                }
            }
            TypeParams::PrecisionScale => {
                let p = self.precision.trim().parse::<u32>().ok().filter(|p| (1..=38).contains(p));
                let Some(p) = p else { return Err(format!("{}: {ty} needs a precision from 1 to 38.", who())) };
                match self.scale.trim().parse::<u32>() {
                    Ok(s) if s <= p => Ok(format!("{ty}({p},{s})")),
                    _ => Err(format!("{}: {ty} needs a scale from 0 to the precision ({p}).", who())),
                }
            }
            TypeParams::Scale => match self.scale.trim() {
                "" => Ok(ty.to_string()),
                s => match s.parse::<u32>() {
                    Ok(s) if s <= 7 => Ok(format!("{ty}({s})")),
                    _ => Err(format!("{}: {ty} needs a scale from 0 to 7.", who())),
                },
            },
        }
    }

    fn identity_values(&self) -> Option<(i128, i128)> {
        Some((self.seed.trim().parse().ok()?, self.increment.trim().parse().ok()?))
    }

    /// Collation to keep when the column is altered: only for character types.
    fn kept_collation(&self) -> Option<&str> {
        let ty = self.data_type.trim().to_ascii_lowercase();
        let chars = matches!(ty.as_str(), "char" | "varchar" | "nchar" | "nvarchar" | "text" | "ntext");
        self.collation.as_deref().filter(|_| chars)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    pub schema: String,
    pub name: String,
    pub columns: Vec<Column>,
    /// The primary key constraint's name and `CLUSTERED` / `NONCLUSTERED`, as loaded.
    pub pk_name: Option<String>,
    pub pk_type: Option<String>,
}

impl Table {
    /// `[schema].[name]`
    pub fn full_name(&self) -> String {
        format!("{}.{}", quote_ident(&self.schema), quote_ident(&self.name))
    }

    /// The primary key's columns in key order: the existing key's order first, then columns
    /// newly added to the key in grid order.
    pub fn key_columns(&self) -> Vec<&Column> {
        let mut keys: Vec<(usize, &Column)> = self.columns.iter().enumerate().filter(|(_, c)| c.primary_key).collect();
        keys.sort_by_key(|(i, c)| (c.key_ordinal.unwrap_or(u32::MAX), *i));
        keys.into_iter().map(|(_, c)| c).collect()
    }

    fn key_list(&self) -> String {
        let cols: Vec<String> = self
            .key_columns()
            .iter()
            .map(|c| format!("{} {}", quote_ident(&c.name), if c.key_descending { "DESC" } else { "ASC" }))
            .collect();
        cols.join(", ")
    }

    fn pk_constraint_name(&self) -> String {
        self.pk_name.clone().unwrap_or_else(|| format!("PK_{}", self.name.trim()))
    }

    fn default_constraint_name(&self, c: &Column) -> String {
        c.default_name.clone().unwrap_or_else(|| format!("DF_{}_{}", self.name.trim(), c.name.trim()))
    }

    /// A name for a new column that isn't taken: `Column<n>` for the n-th row, or the next free
    /// number after it.
    pub fn next_column_name(&self) -> String {
        (self.columns.len() + 1..)
            .map(|i| format!("Column{i}"))
            .find(|n| !self.columns.iter().any(|c| c.name.eq_ignore_ascii_case(n)))
            .unwrap_or_default()
    }

    /// Problems that stop any script: names, types, keys and identity.
    fn validate(&self, errors: &mut Vec<String>) {
        if self.schema.trim().is_empty() {
            errors.push("Enter a schema name.".into());
        }
        if self.name.trim().is_empty() {
            errors.push("Enter a table name.".into());
        }
        if self.columns.is_empty() {
            errors.push("Add at least one column.".into());
        }
        for (i, c) in self.columns.iter().enumerate() {
            if c.name.trim().is_empty() {
                errors.push(format!("Column {} has no name.", i + 1));
                continue;
            }
            if self.columns[..i].iter().any(|o| o.name.trim().eq_ignore_ascii_case(c.name.trim())) {
                errors.push(format!("Column name {} is used more than once.", c.name));
            }
            if c.computed.is_some() {
                continue;
            }
            if let Err(e) = c.type_sql() {
                errors.push(e);
            }
            if c.primary_key && c.nullable {
                errors.push(format!("Column {}: primary key columns can't allow nulls.", c.name));
            }
            if c.identity {
                match c.identity_values() {
                    Some((_, 0)) | None => {
                        errors.push(format!("Column {}: identity seed and increment must be whole numbers, and the increment not 0.", c.name))
                    }
                    Some(_) => {}
                }
                if c.nullable {
                    errors.push(format!("Column {}: identity columns can't allow nulls.", c.name));
                }
                if !c.default.trim().is_empty() {
                    errors.push(format!("Column {}: identity columns can't have a default.", c.name));
                }
            }
        }
        if self.columns.iter().filter(|c| c.identity && c.computed.is_none()).count() > 1 {
            errors.push("A table can have only one identity column.".into());
        }
    }
}

/// What the designer edits: a new table, or an existing one and how it was loaded.
#[derive(Debug, Clone, PartialEq)]
pub struct Design {
    pub db: String,
    /// The table as loaded, for Design; `None` for New Table….
    pub original: Option<Table>,
    pub table: Table,
}

impl Design {
    /// New Table…: `dbo.Table_1` with no columns yet.
    pub fn new_table(db: impl Into<String>) -> Self {
        let table = Table { schema: "dbo".into(), name: "Table_1".into(), columns: Vec::new(), pk_name: None, pk_type: None };
        Self { db: db.into(), original: None, table }
    }

    /// Design: an existing table, loaded with [`table_from_rows`].
    pub fn existing(db: impl Into<String>, table: Table) -> Self {
        Self { db: db.into(), original: Some(table.clone()), table }
    }

    pub fn is_new(&self) -> bool {
        self.original.is_none()
    }

    /// The script for review: CREATE TABLE for a new table, ALTER statements for Design.
    /// Errs with every problem found (including changes that need a rebuild).
    pub fn script(&self) -> Result<String, Vec<String>> {
        let mut out = format!("USE {};\nGO\n\n", quote_ident(&self.db));
        if self.is_new() {
            out.push_str(&self.create_table()?);
            return Ok(out);
        }
        let statements = self.alter_statements()?;
        out.push_str(&format!(
            "-- Changes to {}. Runs as one transaction: if any statement fails, none of them is kept.\n\
             SET XACT_ABORT ON;\nBEGIN TRANSACTION;\n\n",
            self.table.full_name()
        ));
        for s in statements {
            out.push_str(&s);
            out.push('\n');
        }
        out.push_str("\nCOMMIT TRANSACTION;\nGO\n");
        Ok(out)
    }

    /// `CREATE TABLE` for New Table….
    pub fn create_table(&self) -> Result<String, Vec<String>> {
        let t = &self.table;
        let mut errors = Vec::new();
        t.validate(&mut errors);
        if !errors.is_empty() {
            return Err(errors);
        }
        let mut lines = Vec::new();
        for c in &t.columns {
            lines.push(format!("    {}", column_definition(t, c).map_err(|e| vec![e])?));
        }
        if !t.key_columns().is_empty() {
            lines.push(format!(
                "    CONSTRAINT {} PRIMARY KEY {} ({})",
                quote_ident(&t.pk_constraint_name()),
                t.pk_type.as_deref().unwrap_or("CLUSTERED"),
                t.key_list()
            ));
        }
        Ok(format!("CREATE TABLE {}\n(\n{}\n);\nGO\n", t.full_name(), lines.join(",\n")))
    }

    /// Design: one statement per change, in an order that works.
    ///
    /// 1. drop the primary key, if its columns change or any of them is altered or dropped;
    /// 2. drop default constraints that go away or change (or block a change of type);
    /// 3. drop columns;
    /// 4. rename columns (`sp_rename`), so everything after uses the new names;
    /// 5. alter columns' type or nullability;
    /// 6. add new columns, with their defaults;
    /// 7. add the defaults of existing columns;
    /// 8. add the primary key back.
    pub fn alter_statements(&self) -> Result<Vec<String>, Vec<String>> {
        let Some(orig) = &self.original else { return Err(vec!["This is a new table: script it with CREATE TABLE.".into()]) };
        let cur = &self.table;
        let mut errors = Vec::new();
        cur.validate(&mut errors);
        let t = cur.full_name();

        // Columns can only be added at the end, and existing ones can't move.
        let mut last_source = None;
        let mut seen_new = false;
        let mut reordered = false;
        for c in &cur.columns {
            match c.source {
                Some(s) => {
                    if seen_new || last_source.is_some_and(|l| s <= l) {
                        reordered = true;
                    }
                    last_source = Some(s);
                }
                None => seen_new = true,
            }
        }
        if reordered {
            errors.push(format!("Reordering columns {REBUILD}. New columns can only be added at the end."));
        }

        // How each kept column changed.
        struct Change<'a> {
            orig: &'a Column,
            cur: &'a Column,
            altered: bool,
            drop_default: bool,
            add_default: bool,
        }
        let mut changes = Vec::new();
        for c in &cur.columns {
            let Some(o) = c.source.and_then(|s| orig.columns.get(s)) else { continue };
            if c.computed.is_some() {
                changes.push(Change { orig: o, cur: c, altered: false, drop_default: false, add_default: false });
                continue;
            }
            let identity_changed =
                c.identity != o.identity || (c.identity && c.identity_values() != o.identity_values());
            if identity_changed {
                errors.push(format!("Column {}: changing IDENTITY or its seed and increment {REBUILD}.", c.name));
            }
            let type_changed = match (c.type_sql(), o.type_sql()) {
                (Ok(a), Ok(b)) => !a.eq_ignore_ascii_case(&b),
                _ => false,
            };
            let altered = type_changed || c.nullable != o.nullable;
            let default_changed = c.default.trim() != o.default.trim();
            // A column with a default can only change length, precision or scale in place.
            let base_type_changed = !c.data_type.trim().eq_ignore_ascii_case(o.data_type.trim());
            let had_default = !o.default.trim().is_empty();
            let drop_default = had_default && (default_changed || (altered && base_type_changed));
            let add_default = !c.default.trim().is_empty() && (default_changed || drop_default);
            changes.push(Change { orig: o, cur: c, altered, drop_default, add_default });
        }
        let kept = |i: usize| cur.columns.iter().any(|c| c.source == Some(i));
        let dropped: Vec<&Column> = orig.columns.iter().enumerate().filter(|(i, _)| !kept(*i)).map(|(_, c)| c).collect();

        // The primary key is dropped and added again when its columns change, or when one of
        // them is altered (ALTER COLUMN fails on a key column).
        let orig_keys: Vec<Option<usize>> = orig.key_columns().iter().map(|c| c.source).collect();
        let cur_keys: Vec<Option<usize>> = cur.key_columns().iter().map(|c| c.source).collect();
        let key_altered = changes.iter().any(|ch| ch.altered && (ch.cur.primary_key || ch.orig.primary_key));
        let rebuild_pk = orig_keys != cur_keys || key_altered;

        if !errors.is_empty() {
            return Err(errors);
        }

        let mut out = Vec::new();
        // 1. Primary key.
        if rebuild_pk && let Some(pk) = &orig.pk_name {
            out.push(format!("ALTER TABLE {t} DROP CONSTRAINT {};", quote_ident(pk)));
        }
        // 2. Default constraints.
        for c in &dropped {
            if let Some(df) = &c.default_name {
                out.push(format!("ALTER TABLE {t} DROP CONSTRAINT {};", quote_ident(df)));
            }
        }
        for ch in changes.iter().filter(|ch| ch.drop_default) {
            if let Some(df) = &ch.orig.default_name {
                out.push(format!("ALTER TABLE {t} DROP CONSTRAINT {};", quote_ident(df)));
            }
        }
        // 3. Columns.
        for c in &dropped {
            out.push(format!("ALTER TABLE {t} DROP COLUMN {};", quote_ident(&c.name)));
        }
        // 4. Renames. If a new name is one another column still has, go through temporary
        // names first (a swap, or A → B while B → C).
        let renames: Vec<&Change> = changes.iter().filter(|ch| ch.cur.name != ch.orig.name).collect();
        let clash = renames.iter().any(|r| {
            changes.iter().any(|other| {
                !std::ptr::eq(other.cur, r.cur) && other.orig.name.trim().eq_ignore_ascii_case(r.cur.name.trim())
            })
        });
        let rename = |from: &str, to: &str| {
            let path = format!("{}.{}", t, quote_ident(from));
            format!("EXEC sys.sp_rename {}, {}, N'COLUMN';", quote_literal(&path), quote_literal(to))
        };
        if clash {
            for (i, r) in renames.iter().enumerate() {
                out.push(rename(&r.orig.name, &temp_name(i)));
            }
            for (i, r) in renames.iter().enumerate() {
                out.push(rename(&temp_name(i), &r.cur.name));
            }
        } else {
            for r in &renames {
                out.push(rename(&r.orig.name, &r.cur.name));
            }
        }
        // 5. Type and nullability.
        for ch in changes.iter().filter(|ch| ch.altered) {
            let c = ch.cur;
            let ty = c.type_sql().map_err(|e| vec![e])?;
            let collate = c.kept_collation().map(|co| format!(" COLLATE {co}")).unwrap_or_default();
            out.push(format!(
                "ALTER TABLE {t} ALTER COLUMN {} {ty}{collate} {};",
                quote_ident(&c.name),
                if c.nullable { "NULL" } else { "NOT NULL" }
            ));
        }
        // 6. New columns.
        for c in cur.columns.iter().filter(|c| c.source.is_none()) {
            out.push(format!("ALTER TABLE {t} ADD {};", column_definition(cur, c).map_err(|e| vec![e])?));
        }
        // 7. Defaults of existing columns.
        for ch in changes.iter().filter(|ch| ch.add_default) {
            let name = cur.default_constraint_name(ch.cur);
            out.push(format!(
                "ALTER TABLE {t} ADD CONSTRAINT {} DEFAULT {} FOR {};",
                quote_ident(&name),
                default_expr(&ch.cur.default),
                quote_ident(&ch.cur.name)
            ));
        }
        // 8. Primary key.
        if rebuild_pk && !cur_keys.is_empty() {
            // With no key before, let the server choose: CLUSTERED unless the table already
            // has a clustered index.
            let kind = orig.pk_type.as_deref().map(|k| format!(" {k}")).unwrap_or_default();
            out.push(format!(
                "ALTER TABLE {t} ADD CONSTRAINT {} PRIMARY KEY{kind} ({});",
                quote_ident(&cur.pk_constraint_name()),
                cur.key_list()
            ));
        }
        if out.is_empty() {
            return Err(vec!["No changes to script.".into()]);
        }
        Ok(out)
    }
}

fn temp_name(i: usize) -> String {
    format!("sqlkraft_rename_{}", i + 1)
}

/// A column in CREATE TABLE or ALTER TABLE ADD: `[Name] nvarchar(50) NOT NULL CONSTRAINT
/// [DF_T_Name] DEFAULT (N'')`.
fn column_definition(t: &Table, c: &Column) -> Result<String, String> {
    let mut s = format!("{} {}", quote_ident(&c.name), c.type_sql()?);
    if c.identity {
        let (seed, inc) = c.identity_values().ok_or_else(|| format!("Column {}: bad identity values.", c.name))?;
        s.push_str(&format!(" IDENTITY({seed},{inc})"));
    }
    s.push_str(if c.nullable { " NULL" } else { " NOT NULL" });
    if !c.default.trim().is_empty() {
        s.push_str(&format!(
            " CONSTRAINT {} DEFAULT {}",
            quote_ident(&t.default_constraint_name(c)),
            default_expr(&c.default)
        ));
    }
    Ok(s)
}

/// The default as written after `DEFAULT`: wrapped in parentheses unless it already is.
fn default_expr(expr: &str) -> String {
    let e = expr.trim();
    if fully_parenthesized(e) { e.to_string() } else { format!("({e})") }
}

/// `(0)` and `((1)+(2))` are; `(1)+(2)` and `getdate()` are not. Ignores parentheses in strings.
fn fully_parenthesized(s: &str) -> bool {
    if !s.starts_with('(') || !s.ends_with(')') {
        return false;
    }
    let mut depth = 0i32;
    let mut in_string = false;
    let last = s.len() - 1;
    for (i, ch) in s.char_indices() {
        if in_string {
            in_string = ch != '\'';
            continue;
        }
        match ch {
            '\'' => in_string = true,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 && i != last {
                    return false;
                }
            }
            _ => {}
        }
    }
    depth == 0
}

// ---------------------------------------------------------------------------------------------
// Loading from the catalog
// ---------------------------------------------------------------------------------------------

/// SQL returning one row per column of `[db].[schema].[name]`, everything the designer needs:
/// name, type and sizes, nullability, identity, computed expression, default and its
/// constraint name, collation, and the primary key (name, type, key position, order).
///
/// Names the database in every catalog view, so it works from any current database.
pub fn load_query(db: &str, schema: &str, name: &str) -> String {
    let full = format!("{}.{}.{}", quote_ident(db), quote_ident(schema), quote_ident(name));
    format!(
        "SELECT c.name, ty.name, c.max_length, c.precision, c.scale, c.is_nullable, c.is_identity, \
                CAST(idc.seed_value AS nvarchar(40)), CAST(idc.increment_value AS nvarchar(40)), \
                cc.definition, dc.definition, dc.name, c.collation_name, \
                kc.name, i.type_desc, pkc.key_ordinal, pkc.is_descending_key \
         FROM {d}.sys.columns c \
         JOIN {d}.sys.types ty ON ty.user_type_id = c.user_type_id \
         LEFT JOIN {d}.sys.identity_columns idc ON idc.object_id = c.object_id AND idc.column_id = c.column_id \
         LEFT JOIN {d}.sys.computed_columns cc ON cc.object_id = c.object_id AND cc.column_id = c.column_id \
         LEFT JOIN {d}.sys.default_constraints dc ON dc.parent_object_id = c.object_id AND dc.parent_column_id = c.column_id \
         LEFT JOIN {d}.sys.key_constraints kc ON kc.parent_object_id = c.object_id AND kc.type = 'PK' \
         LEFT JOIN {d}.sys.indexes i ON i.object_id = kc.parent_object_id AND i.index_id = kc.unique_index_id \
         LEFT JOIN {d}.sys.index_columns pkc ON pkc.object_id = i.object_id AND pkc.index_id = i.index_id \
                AND pkc.column_id = c.column_id AND pkc.key_ordinal > 0 \
         WHERE c.object_id = OBJECT_ID({lit}) \
         ORDER BY c.column_id",
        d = quote_ident(db),
        lit = quote_literal(&full)
    )
}

/// The table from [`load_query`]'s rows.
pub fn table_from_rows(schema: &str, name: &str, rows: &[Vec<Cell>]) -> Result<Table, String> {
    if rows.is_empty() {
        return Err(format!("Table {schema}.{name} was not found (or you can't see its columns)."));
    }
    let s = |r: &[Cell], i: usize| r.get(i).cloned().flatten().unwrap_or_default();
    let opt = |r: &[Cell], i: usize| r.get(i).cloned().flatten();
    let mut columns = Vec::new();
    for (idx, r) in rows.iter().enumerate() {
        let ty = s(r, 1);
        let max_length: i32 = s(r, 2).parse().unwrap_or(0);
        let params = type_params(&ty);
        let length = match params {
            TypeParams::Length if max_length == -1 => "max".to_string(),
            TypeParams::Length if ty == "nvarchar" || ty == "nchar" => (max_length / 2).to_string(),
            TypeParams::Length => max_length.to_string(),
            _ => String::new(),
        };
        let mut c = Column::new(s(r, 0));
        c.data_type = ty;
        c.length = length;
        if params == TypeParams::PrecisionScale {
            c.precision = s(r, 3);
        }
        if matches!(params, TypeParams::PrecisionScale | TypeParams::Scale) {
            c.scale = s(r, 4);
        }
        c.nullable = s(r, 5) == "1";
        c.identity = s(r, 6) == "1";
        if c.identity {
            c.seed = s(r, 7);
            c.increment = s(r, 8);
        }
        c.computed = opt(r, 9);
        c.default = s(r, 10);
        c.default_name = opt(r, 11);
        c.collation = opt(r, 12);
        c.key_ordinal = opt(r, 15).and_then(|k| k.parse().ok());
        c.primary_key = c.key_ordinal.is_some();
        c.key_descending = s(r, 16) == "1";
        c.source = Some(idx);
        columns.push(c);
    }
    let first = &rows[0];
    Ok(Table {
        schema: schema.to_string(),
        name: name.to_string(),
        columns,
        pk_name: opt(first, 13),
        pk_type: opt(first, 14),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(name: &str, ty: &str) -> Column {
        let mut c = Column::new(name);
        c.data_type = ty.into();
        c.nullable = false;
        c
    }

    /// dbo.Orders as loaded: Id int identity PK, Name nvarchar(100), Qty int default 0,
    /// Note varchar(20) null.
    fn orders() -> Design {
        let mut id = col("Id", "int");
        id.identity = true;
        id.primary_key = true;
        id.key_ordinal = Some(1);
        let mut name = col("Name", "nvarchar");
        name.length = "100".into();
        name.collation = Some("Latin1_General_CI_AS".into());
        let mut qty = col("Qty", "int");
        qty.default = "((0))".into();
        qty.default_name = Some("DF_Orders_Qty".into());
        let mut note = col("Note", "varchar");
        note.length = "20".into();
        note.nullable = true;
        let mut columns = vec![id, name, qty, note];
        for (i, c) in columns.iter_mut().enumerate() {
            c.source = Some(i);
        }
        let t = Table {
            schema: "dbo".into(),
            name: "Orders".into(),
            columns,
            pk_name: Some("PK_Orders".into()),
            pk_type: Some("CLUSTERED".into()),
        };
        Design::existing("shop", t)
    }

    fn stmts(d: &Design) -> Vec<String> {
        d.alter_statements().unwrap_or_else(|e| panic!("{e:?}"))
    }

    fn errors(d: &Design) -> Vec<String> {
        d.alter_statements().expect_err("should be refused")
    }

    #[test]
    fn create_table_with_types_nulls_defaults_identity_and_composite_key() {
        let mut d = Design::new_table("shop");
        d.table.name = "Order Lines".into();
        let mut a = col("OrderId", "int");
        a.primary_key = true;
        let mut b = col("LineNo", "int");
        b.identity = true;
        b.seed = "100".into();
        b.increment = "10".into();
        b.primary_key = true;
        let mut price = col("Price", "decimal");
        price.precision = "18".into();
        price.scale = "2".into();
        price.default = "0".into();
        let mut text = col("Text", "nvarchar");
        text.length = "max".into();
        text.nullable = true;
        let mut code = col("Code", "char");
        code.length = "3".into();
        code.default = "'ABC'".into();
        let mut at = col("At", "datetime2");
        at.scale = "3".into();
        at.default = "sysutcdatetime()".into();
        d.table.columns = vec![a, b, price, text, code, at];
        let sql = d.script().unwrap();
        assert_eq!(
            sql,
            "USE [shop];\nGO\n\n\
             CREATE TABLE [dbo].[Order Lines]\n(\n\
             \x20   [OrderId] int NOT NULL,\n\
             \x20   [LineNo] int IDENTITY(100,10) NOT NULL,\n\
             \x20   [Price] decimal(18,2) NOT NULL CONSTRAINT [DF_Order Lines_Price] DEFAULT (0),\n\
             \x20   [Text] nvarchar(max) NULL,\n\
             \x20   [Code] char(3) NOT NULL CONSTRAINT [DF_Order Lines_Code] DEFAULT ('ABC'),\n\
             \x20   [At] datetime2(3) NOT NULL CONSTRAINT [DF_Order Lines_At] DEFAULT (sysutcdatetime()),\n\
             \x20   CONSTRAINT [PK_Order Lines] PRIMARY KEY CLUSTERED ([OrderId] ASC, [LineNo] ASC)\n\
             );\nGO\n"
        );
    }

    #[test]
    fn create_table_validation() {
        let mut d = Design::new_table("shop");
        assert_eq!(d.script().unwrap_err(), ["Add at least one column."]);
        let mut a = col("A", "varchar");
        a.length = "".into();
        let mut b = col("a", "decimal");
        b.precision = "10".into();
        b.scale = "12".into();
        let mut c = col("C", "int");
        c.primary_key = true;
        c.nullable = true;
        let mut i1 = col("I1", "int");
        i1.identity = true;
        let mut i2 = col("I2", "bigint");
        i2.identity = true;
        i2.increment = "0".into();
        d.table.columns = vec![a, b, c, i1, i2];
        d.table.name = " ".into();
        let e = d.script().unwrap_err();
        assert_eq!(
            e,
            [
                "Enter a table name.",
                "Column A: varchar needs a length from 1 to 8000, or max.",
                "Column name a is used more than once.",
                "Column a: decimal needs a scale from 0 to the precision (10).",
                "Column C: primary key columns can't allow nulls.",
                "Column I2: identity seed and increment must be whole numbers, and the increment not 0.",
                "A table can have only one identity column.",
            ]
        );
    }

    #[test]
    fn type_sql_forms() {
        let mut c = col("x", "nvarchar");
        c.length = "MAX".into();
        assert_eq!(c.type_sql().unwrap(), "nvarchar(max)");
        c.length = "4001".into();
        assert!(c.type_sql().is_err());
        c.data_type = "time".into();
        c.scale = "".into();
        assert_eq!(c.type_sql().unwrap(), "time");
        c.data_type = "uniqueidentifier".into();
        assert_eq!(c.type_sql().unwrap(), "uniqueidentifier");
        c.data_type = " ".into();
        assert!(c.type_sql().is_err());
    }

    #[test]
    fn unchanged_design_has_nothing_to_script() {
        assert_eq!(errors(&orders()), ["No changes to script."]);
    }

    #[test]
    fn add_column() {
        let mut d = orders();
        let mut c = col("Shipped", "bit");
        c.default = "((0))".into();
        d.table.columns.push(c);
        d.table.columns.push(Column::new("Memo"));
        assert_eq!(
            stmts(&d),
            [
                "ALTER TABLE [dbo].[Orders] ADD [Shipped] bit NOT NULL CONSTRAINT [DF_Orders_Shipped] DEFAULT ((0));",
                "ALTER TABLE [dbo].[Orders] ADD [Memo] nvarchar(50) NULL;",
            ]
        );
    }

    #[test]
    fn drop_column_drops_its_default_first() {
        let mut d = orders();
        d.table.columns.remove(2);
        assert_eq!(
            stmts(&d),
            [
                "ALTER TABLE [dbo].[Orders] DROP CONSTRAINT [DF_Orders_Qty];",
                "ALTER TABLE [dbo].[Orders] DROP COLUMN [Qty];",
            ]
        );
    }

    #[test]
    fn alter_type_and_nullability() {
        let mut d = orders();
        d.table.columns[1].length = "200".into();
        d.table.columns[3].nullable = false;
        assert_eq!(
            stmts(&d),
            [
                "ALTER TABLE [dbo].[Orders] ALTER COLUMN [Name] nvarchar(200) COLLATE Latin1_General_CI_AS NOT NULL;",
                "ALTER TABLE [dbo].[Orders] ALTER COLUMN [Note] varchar(20) NOT NULL;",
            ]
        );
    }

    #[test]
    fn changing_the_base_type_of_a_column_with_a_default_re_adds_it() {
        let mut d = orders();
        d.table.columns[2].data_type = "bigint".into();
        assert_eq!(
            stmts(&d),
            [
                "ALTER TABLE [dbo].[Orders] DROP CONSTRAINT [DF_Orders_Qty];",
                "ALTER TABLE [dbo].[Orders] ALTER COLUMN [Qty] bigint NOT NULL;",
                "ALTER TABLE [dbo].[Orders] ADD CONSTRAINT [DF_Orders_Qty] DEFAULT ((0)) FOR [Qty];",
            ]
        );
        // Nullability alone leaves the default alone.
        let mut d = orders();
        d.table.columns[2].nullable = true;
        assert_eq!(stmts(&d), ["ALTER TABLE [dbo].[Orders] ALTER COLUMN [Qty] int NULL;"]);
    }

    #[test]
    fn add_drop_and_change_defaults() {
        let mut d = orders();
        d.table.columns[3].default = "'n/a'".into();
        assert_eq!(stmts(&d), ["ALTER TABLE [dbo].[Orders] ADD CONSTRAINT [DF_Orders_Note] DEFAULT ('n/a') FOR [Note];"]);

        let mut d = orders();
        d.table.columns[2].default = String::new();
        assert_eq!(stmts(&d), ["ALTER TABLE [dbo].[Orders] DROP CONSTRAINT [DF_Orders_Qty];"]);

        let mut d = orders();
        d.table.columns[2].default = "1".into();
        assert_eq!(
            stmts(&d),
            [
                "ALTER TABLE [dbo].[Orders] DROP CONSTRAINT [DF_Orders_Qty];",
                "ALTER TABLE [dbo].[Orders] ADD CONSTRAINT [DF_Orders_Qty] DEFAULT (1) FOR [Qty];",
            ]
        );
    }

    #[test]
    fn change_primary_key_drops_and_re_adds_it() {
        let mut d = orders();
        d.table.columns[1].primary_key = true;
        assert_eq!(
            stmts(&d),
            [
                "ALTER TABLE [dbo].[Orders] DROP CONSTRAINT [PK_Orders];",
                "ALTER TABLE [dbo].[Orders] ADD CONSTRAINT [PK_Orders] PRIMARY KEY CLUSTERED ([Id] ASC, [Name] ASC);",
            ]
        );
        // Removing the key altogether.
        let mut d = orders();
        d.table.columns[0].primary_key = false;
        assert_eq!(stmts(&d), ["ALTER TABLE [dbo].[Orders] DROP CONSTRAINT [PK_Orders];"]);
        // A key on a table without one: the server picks clustered or not.
        let mut d = orders();
        for t in [d.original.as_mut().unwrap(), &mut d.table] {
            t.columns[0].primary_key = false;
            t.columns[0].key_ordinal = None;
            t.pk_name = None;
            t.pk_type = None;
        }
        d.table.columns[3].primary_key = true;
        d.table.columns[3].nullable = false;
        assert_eq!(
            stmts(&d),
            [
                "ALTER TABLE [dbo].[Orders] ALTER COLUMN [Note] varchar(20) NOT NULL;",
                "ALTER TABLE [dbo].[Orders] ADD CONSTRAINT [PK_Orders] PRIMARY KEY ([Note] ASC);",
            ]
        );
    }

    #[test]
    fn rename_column() {
        let mut d = orders();
        d.table.columns[1].name = "FullName".into();
        assert_eq!(stmts(&d), ["EXEC sys.sp_rename N'[dbo].[Orders].[Name]', N'FullName', N'COLUMN';"]);
    }

    #[test]
    fn rename_and_alter_the_same_column_renames_first() {
        let mut d = orders();
        d.table.columns[3].name = "Comment".into();
        d.table.columns[3].length = "50".into();
        d.table.columns[3].default = "''".into();
        assert_eq!(
            stmts(&d),
            [
                "EXEC sys.sp_rename N'[dbo].[Orders].[Note]', N'Comment', N'COLUMN';",
                "ALTER TABLE [dbo].[Orders] ALTER COLUMN [Comment] varchar(50) NULL;",
                "ALTER TABLE [dbo].[Orders] ADD CONSTRAINT [DF_Orders_Comment] DEFAULT ('') FOR [Comment];",
            ]
        );
    }

    #[test]
    fn swapping_names_goes_through_temporary_names() {
        let mut d = orders();
        d.table.columns[1].name = "Note".into();
        d.table.columns[3].name = "Name".into();
        assert_eq!(
            stmts(&d),
            [
                "EXEC sys.sp_rename N'[dbo].[Orders].[Name]', N'sqlkraft_rename_1', N'COLUMN';",
                "EXEC sys.sp_rename N'[dbo].[Orders].[Note]', N'sqlkraft_rename_2', N'COLUMN';",
                "EXEC sys.sp_rename N'[dbo].[Orders].[sqlkraft_rename_1]', N'Note', N'COLUMN';",
                "EXEC sys.sp_rename N'[dbo].[Orders].[sqlkraft_rename_2]', N'Name', N'COLUMN';",
            ]
        );
    }

    #[test]
    fn dropping_a_key_column_drops_the_key_first_and_re_adds_the_rest() {
        let mut d = orders();
        d.table.columns[1].primary_key = true;
        d.table.columns[1].key_ordinal = Some(2);
        d.original.as_mut().unwrap().columns[1].primary_key = true;
        d.original.as_mut().unwrap().columns[1].key_ordinal = Some(2);
        d.table.columns.remove(0);
        assert_eq!(
            stmts(&d),
            [
                "ALTER TABLE [dbo].[Orders] DROP CONSTRAINT [PK_Orders];",
                "ALTER TABLE [dbo].[Orders] DROP COLUMN [Id];",
                "ALTER TABLE [dbo].[Orders] ADD CONSTRAINT [PK_Orders] PRIMARY KEY CLUSTERED ([Name] ASC);",
            ]
        );
    }

    #[test]
    fn altering_a_key_column_rebuilds_the_key_around_it() {
        let mut d = orders();
        d.table.columns[0].data_type = "bigint".into();
        d.table.columns[0].identity = true;
        assert_eq!(
            stmts(&d),
            [
                "ALTER TABLE [dbo].[Orders] DROP CONSTRAINT [PK_Orders];",
                "ALTER TABLE [dbo].[Orders] ALTER COLUMN [Id] bigint NOT NULL;",
                "ALTER TABLE [dbo].[Orders] ADD CONSTRAINT [PK_Orders] PRIMARY KEY CLUSTERED ([Id] ASC);",
            ]
        );
    }

    #[test]
    fn everything_at_once_in_order() {
        let mut d = orders();
        // Rename Name and widen it, drop Qty (and its default), add a default to Note, add a
        // column, and add Name to the key.
        d.table.columns[1].name = "Title".into();
        d.table.columns[1].length = "150".into();
        d.table.columns[1].primary_key = true;
        d.table.columns[3].default = "'-'".into();
        d.table.columns.remove(2);
        d.table.columns.push(col("Total", "money"));
        let t = "ALTER TABLE [dbo].[Orders]";
        assert_eq!(
            stmts(&d),
            [
                format!("{t} DROP CONSTRAINT [PK_Orders];"),
                format!("{t} DROP CONSTRAINT [DF_Orders_Qty];"),
                format!("{t} DROP COLUMN [Qty];"),
                "EXEC sys.sp_rename N'[dbo].[Orders].[Name]', N'Title', N'COLUMN';".into(),
                format!("{t} ALTER COLUMN [Title] nvarchar(150) COLLATE Latin1_General_CI_AS NOT NULL;"),
                format!("{t} ADD [Total] money NOT NULL;"),
                format!("{t} ADD CONSTRAINT [DF_Orders_Note] DEFAULT ('-') FOR [Note];"),
                format!("{t} ADD CONSTRAINT [PK_Orders] PRIMARY KEY CLUSTERED ([Id] ASC, [Title] ASC);"),
            ]
        );
        let script = d.script().unwrap();
        assert!(script.starts_with("USE [shop];\nGO\n\n"), "{script}");
        assert!(script.contains("SET XACT_ABORT ON;\nBEGIN TRANSACTION;\n"), "{script}");
        assert!(script.ends_with("COMMIT TRANSACTION;\nGO\n"), "{script}");
    }

    #[test]
    fn odd_names_are_quoted() {
        let mut d = orders();
        d.db = "my]db".into();
        for t in [d.original.as_mut().unwrap(), &mut d.table] {
            t.schema = "s p".into();
            t.name = "O'Brien]s".into();
        }
        d.table.columns[3].name = "it's ]odd".into();
        d.table.columns[2].default = "N'it''s'".into();
        d.table.columns.push(col("new ]col", "int"));
        let t = "ALTER TABLE [s p].[O'Brien]]s]";
        assert_eq!(
            stmts(&d),
            [
                format!("{t} DROP CONSTRAINT [DF_Orders_Qty];"),
                "EXEC sys.sp_rename N'[s p].[O''Brien]]s].[Note]', N'it''s ]odd', N'COLUMN';".into(),
                format!("{t} ADD [new ]]col] int NOT NULL;"),
                format!("{t} ADD CONSTRAINT [DF_Orders_Qty] DEFAULT (N'it''s') FOR [Qty];"),
            ]
        );
        assert!(d.script().unwrap().starts_with("USE [my]]db];"));
        // Generated constraint names are quoted too.
        let mut d = Design::new_table("db");
        d.table.name = "a]b".into();
        let mut c = col("c'd", "int");
        c.default = "1".into();
        c.primary_key = true;
        d.table.columns.push(c);
        let sql = d.create_table().unwrap();
        assert!(sql.contains("CREATE TABLE [dbo].[a]]b]"), "{sql}");
        assert!(sql.contains("[c'd] int NOT NULL CONSTRAINT [DF_a]]b_c'd] DEFAULT (1)"), "{sql}");
        assert!(sql.contains("CONSTRAINT [PK_a]]b] PRIMARY KEY CLUSTERED ([c'd] ASC)"), "{sql}");
    }

    #[test]
    fn identity_changes_are_refused() {
        let mut d = orders();
        d.table.columns[0].identity = false;
        let e = errors(&d);
        assert_eq!(e, [format!("Column Id: changing IDENTITY or its seed and increment {REBUILD}.")]);
        assert!(e[0].contains("not supported yet — it needs a table rebuild"));

        let mut d = orders();
        d.table.columns[0].seed = "1000".into();
        assert_eq!(errors(&d).len(), 1);

        let mut d = orders();
        d.table.columns[2].identity = true;
        d.table.columns[2].default = String::new();
        let e = errors(&d);
        assert!(e.iter().any(|m| m.starts_with("Column Qty: changing IDENTITY") && m.contains(REBUILD)), "{e:?}");
    }

    #[test]
    fn reordering_is_refused() {
        let mut d = orders();
        d.table.columns.swap(1, 2);
        let e = errors(&d);
        assert_eq!(e, [format!("Reordering columns {REBUILD}. New columns can only be added at the end.")]);

        // A new column between existing ones is a reorder too.
        let mut d = orders();
        d.table.columns.insert(1, Column::new("Middle"));
        assert!(errors(&d)[0].starts_with("Reordering columns"));
    }

    #[test]
    fn computed_columns_can_be_renamed_or_dropped() {
        let mut d = orders();
        let mut c = col("Shout", "nvarchar");
        c.computed = Some("(upper([Name]))".into());
        c.nullable = true;
        c.source = Some(4);
        d.original.as_mut().unwrap().columns.push(c.clone());
        d.table.columns.push(c);
        assert_eq!(errors(&d), ["No changes to script."]);
        d.table.columns[4].name = "Loud".into();
        assert_eq!(stmts(&d), ["EXEC sys.sp_rename N'[dbo].[Orders].[Shout]', N'Loud', N'COLUMN';"]);
        d.table.columns.pop();
        assert_eq!(stmts(&d), ["ALTER TABLE [dbo].[Orders] DROP COLUMN [Shout];"]);
    }

    #[test]
    fn parentheses() {
        assert!(fully_parenthesized("(0)"));
        assert!(fully_parenthesized("((1)+(2))"));
        assert!(fully_parenthesized("(')')"));
        assert!(!fully_parenthesized("(1)+(2)"));
        assert!(!fully_parenthesized("getdate()"));
        assert_eq!(default_expr(" getdate() "), "(getdate())");
        assert_eq!(default_expr("((0))"), "((0))");
    }

    #[test]
    fn loads_a_table_from_catalog_rows() {
        let c = |v: &[&str]| -> Vec<Cell> { v.iter().map(|x| if x.is_empty() { None } else { Some(x.to_string()) }).collect() };
        let pk = ["PK_T", "CLUSTERED"];
        let rows = vec![
            c(&["Id", "int", "4", "10", "0", "0", "1", "5", "2", "", "", "", "", pk[0], pk[1], "1", "0"]),
            c(&["Name", "nvarchar", "200", "0", "0", "1", "0", "", "", "", "(N'x')", "DF_T_Name", "Latin1_General_CI_AS", pk[0], pk[1], "", ""]),
            c(&["Big", "varbinary", "-1", "0", "0", "1", "0", "", "", "", "", "", "", pk[0], pk[1], "", ""]),
            c(&["Amount", "decimal", "9", "18", "2", "0", "0", "", "", "", "((0))", "DF_T_Amount", "", pk[0], pk[1], "2", "1"]),
            c(&["At", "datetime2", "8", "27", "3", "0", "0", "", "", "", "", "", "", pk[0], pk[1], "", ""]),
            c(&["Calc", "int", "4", "10", "0", "1", "0", "", "", "([Id]*(2))", "", "", "", pk[0], pk[1], "", ""]),
        ];
        let t = table_from_rows("dbo", "T", &rows).unwrap();
        assert_eq!(t.pk_name.as_deref(), Some("PK_T"));
        assert_eq!(t.pk_type.as_deref(), Some("CLUSTERED"));
        let types: Vec<_> = t.columns.iter().map(|c| c.type_sql().unwrap()).collect();
        assert_eq!(types, ["int", "nvarchar(100)", "varbinary(max)", "decimal(18,2)", "datetime2(3)", "int"]);
        assert!(t.columns[0].identity && t.columns[0].seed == "5" && t.columns[0].increment == "2");
        assert_eq!(t.columns[1].default_name.as_deref(), Some("DF_T_Name"));
        assert!(t.columns[1].nullable && !t.columns[0].nullable);
        assert_eq!(t.key_columns().iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["Id", "Amount"]);
        assert!(t.columns[3].key_descending);
        assert_eq!(t.columns[5].computed.as_deref(), Some("([Id]*(2))"));
        assert_eq!(t.columns[4].source, Some(4));
        assert!(table_from_rows("dbo", "Nope", &[]).is_err());
        // Nothing changed after loading.
        assert_eq!(Design::existing("db", t).alter_statements().unwrap_err(), ["No changes to script."]);
    }

    #[test]
    fn load_query_names_the_database() {
        let q = load_query("my]db", "dbo", "T");
        assert!(q.contains("[my]]db].sys.columns"), "{q}");
        assert!(q.contains("[my]]db].sys.default_constraints"), "{q}");
        assert!(q.contains("[my]]db].sys.key_constraints"), "{q}");
        assert!(q.contains("OBJECT_ID(N'[my]]db].[dbo].[T]')"), "{q}");
    }

    #[test]
    fn next_column_name_skips_taken_names() {
        let mut d = Design::new_table("db");
        assert_eq!(d.table.next_column_name(), "Column1");
        d.table.columns.push(Column::new("Id"));
        assert_eq!(d.table.next_column_name(), "Column2");
        d.table.columns.push(Column::new("column3"));
        assert_eq!(d.table.next_column_name(), "Column4");
        d.table.columns[0].name = "Column3".into();
        d.table.columns[1].name = "Column4".into();
        assert_eq!(d.table.next_column_name(), "Column5");
    }
}
