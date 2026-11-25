//! Object Explorer and scripting against a live server. See `tests/common/mod.rs` for setup.

mod common;

use common::Fixture;
use sqlkraft::db::{Event, MsgKind};
use sqlkraft::explorer::{self, NodeKind};
use sqlkraft::sql::quote_ident;

fn labels(children: &[(String, NodeKind)]) -> Vec<&str> {
    children.iter().map(|(l, _)| l.as_str()).collect()
}

#[test]
fn explorer_tree_lists_seeded_objects() {
    let f = Fixture::new(live_or_skip!());
    let db = f.db.as_str();

    let dbs = f.browse(&["Databases"]);
    assert_eq!(dbs[0].0, "System Databases");
    assert!(labels(&dbs).contains(&db), "{db} missing from {:?}", labels(&dbs));
    assert!(labels(&f.browse(&["Databases", "System Databases"])).contains(&"master"));

    let tables = f.browse(&["Databases", db, "Tables"]);
    assert_eq!(labels(&tables), ["dbo.Customers", "sales.Order Lines"]);
    assert_eq!(
        tables[1].1,
        NodeKind::Table { db: db.into(), schema: "sales".into(), name: "Order Lines".into() }
    );

    let cols = f.browse(&["Databases", db, "Tables", "dbo.Customers", "Columns"]);
    assert_eq!(
        labels(&cols),
        [
            "Id (PK, identity, int, not null)",
            "Name (nvarchar(100), not null)",
            "Email (varchar(255), null)",
            "Balance (decimal(18,2), not null)",
            "CreatedAt (datetime2(3), not null)",
            "Notes (nvarchar(max), null)",
            "Shout (computed, nvarchar(100), null)",
        ]
    );
    let cols = f.browse(&["Databases", db, "Tables", "sales.Order Lines", "Columns"]);
    assert_eq!(labels(&cols)[1], "Line]No (PK, int, not null)");

    let idx = f.browse(&["Databases", db, "Tables", "dbo.Customers", "Indexes"]);
    assert_eq!(
        labels(&idx),
        [
            "IX_Customers_Name (nonclustered)",
            "PK_Customers (primary key, clustered)",
            "UQ_Customers_Email (unique, nonclustered)",
        ]
    );

    assert_eq!(labels(&f.browse(&["Databases", db, "Views"])), ["dbo.ActiveCustomers"]);
    let view_cols = f.browse(&["Databases", db, "Views", "dbo.ActiveCustomers", "Columns"]);
    assert_eq!(view_cols.len(), 2);

    let procs = f.browse(&["Databases", db, "Programmability", "Stored Procedures"]);
    assert_eq!(labels(&procs), ["dbo.GetCustomer", "sales.Odd]Name Proc"]);

    let funcs = f.browse(&["Databases", db, "Programmability", "Functions"]);
    assert_eq!(labels(&funcs), ["dbo.Twice", "sales.LinesFor"]);
    assert!(matches!(funcs[0].1, NodeKind::Function { table_valued: false, .. }));
    assert!(matches!(funcs[1].1, NodeKind::Function { table_valued: true, .. }));

    if !f.info.user.is_empty() {
        let logins = f.browse(&["Security", "Logins"]);
        let me = format!("{} (sql login)", f.info.user);
        assert!(labels(&logins).contains(&me.as_str()), "{me} missing from {:?}", labels(&logins));
    }
}

fn object(f: &Fixture, folder: &[&str], label: &str) -> NodeKind {
    let mut path = vec!["Databases", f.db.as_str()];
    path.extend_from_slice(folder);
    f.browse(&path)
        .into_iter()
        .find(|(l, _)| l == label)
        .unwrap_or_else(|| panic!("no {label}"))
        .1
}

#[test]
fn script_as_create_for_modules_outside_current_database() {
    let f = Fixture::new(live_or_skip!());
    let cases = [
        (vec!["Views"], "dbo.ActiveCustomers", "CREATE VIEW dbo.ActiveCustomers"),
        (vec!["Programmability", "Stored Procedures"], "dbo.GetCustomer", "CREATE PROCEDURE dbo.GetCustomer"),
        (vec!["Programmability", "Stored Procedures"], "sales.Odd]Name Proc", "CREATE PROCEDURE sales.[Odd]]Name Proc]"),
        (vec!["Programmability", "Functions"], "dbo.Twice", "CREATE FUNCTION dbo.Twice"),
        (vec!["Programmability", "Functions"], "sales.LinesFor", "CREATE FUNCTION sales.LinesFor"),
    ];
    for (folder, label, expected) in cases {
        let kind = object(&f, &folder, label);
        let q = explorer::script_create_query(&kind).unwrap();
        let rows = f.fetch(&q);
        let def = rows.first().and_then(|r| r[0].clone()).unwrap_or_else(|| panic!("no definition for {label}"));
        assert!(def.contains(expected), "{label}: {def}");
        let script = explorer::wrap_definition(&kind, &def);
        assert!(script.starts_with(&format!("USE {};\nGO\n", quote_ident(&f.db))), "{script}");
    }
}

/// Scripts each table, runs the script in a second database, and checks the copy has the same
/// columns, defaults, computed columns and keys.
#[test]
fn script_as_create_table_round_trips() {
    let mut f = Fixture::new(live_or_skip!());
    let target = f.extra_database();
    f.seed(&target, "CREATE SCHEMA sales;");

    for label in ["dbo.Customers", "sales.Order Lines"] {
        let kind = object(&f, &["Tables"], label);
        let cols = f.fetch(&explorer::script_create_query(&kind).unwrap());
        let keys = f.fetch(&explorer::script_keys_query(&kind).unwrap());
        let script = explorer::build_create_table(&kind, &cols, &keys);
        let script = script.replacen(&quote_ident(&f.db), &quote_ident(&target), 1);

        let ev = f.execute(&script);
        let errors: Vec<_> = ev
            .iter()
            .filter_map(|e| match e {
                Event::Message(m) if m.kind == MsgKind::Error => Some(m.text.clone()),
                _ => None,
            })
            .collect();
        assert!(errors.is_empty(), "{label} script failed: {errors:?}\n{script}");

        let NodeKind::Table { schema, name, .. } = &kind else { unreachable!() };
        assert_eq!(
            table_shape(&f, &target, schema, name),
            table_shape(&f, &f.db, schema, name),
            "{label} differs after round trip:\n{script}"
        );
    }
}

fn table_shape(f: &Fixture, db: &str, schema: &str, name: &str) -> Vec<Vec<Option<String>>> {
    let d = quote_ident(db);
    let full = sqlkraft::sql::quote_literal(&format!("{d}.{}.{}", quote_ident(schema), quote_ident(name)));
    let mut cols = f.fetch(&format!(
        "SELECT c.name, ty.name, c.max_length, c.precision, c.scale, c.is_nullable, c.is_identity, \
                cc.definition, dc.definition \
         FROM {d}.sys.columns c JOIN {d}.sys.types ty ON ty.user_type_id = c.user_type_id \
         LEFT JOIN {d}.sys.computed_columns cc ON cc.object_id = c.object_id AND cc.column_id = c.column_id \
         LEFT JOIN {d}.sys.default_constraints dc ON dc.parent_object_id = c.object_id AND dc.parent_column_id = c.column_id \
         WHERE c.object_id = OBJECT_ID({full}) ORDER BY c.column_id"
    ));
    cols.extend(f.fetch(&format!(
        "SELECT i.name, i.type_desc, i.is_primary_key, i.is_unique_constraint, c.name, ic.key_ordinal, ic.is_descending_key \
         FROM {d}.sys.indexes i \
         JOIN {d}.sys.index_columns ic ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
         JOIN {d}.sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
         WHERE i.object_id = OBJECT_ID({full}) AND (i.is_primary_key = 1 OR i.is_unique_constraint = 1) \
         ORDER BY i.name, ic.key_ordinal"
    )));
    assert!(!cols.is_empty(), "no columns for {db}.{schema}.{name}");
    cols
}

#[test]
fn select_top_and_execute_procedure_scripts_run() {
    let mut f = Fixture::new(live_or_skip!());

    let kind = object(&f, &["Tables"], "dbo.Customers");
    let names: Vec<String> = f
        .fetch(&explorer::column_names_query(&kind).unwrap())
        .into_iter()
        .map(|r| r[0].clone().unwrap())
        .collect();
    assert_eq!(names, ["Id", "Name", "Email", "Balance", "CreatedAt", "Notes", "Shout"]);
    let rows = f.fetch(&explorer::select_top_script(&kind, &names).unwrap());
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][1].as_deref(), Some("Ada"));
    assert_eq!(rows[0][6].as_deref(), Some("ADA"));

    let kind = object(&f, &["Programmability", "Stored Procedures"], "sales.Odd]Name Proc");
    let ev = f.execute(&explorer::execute_proc_script(&kind).unwrap());
    let rows: Vec<_> = ev
        .iter()
        .filter_map(|e| if let Event::Rows(r) = e { Some(r.clone()) } else { None })
        .collect();
    assert_eq!(rows.len(), 2, "{ev:?}");
    assert_eq!(rows[0][0][0].as_deref(), Some("42"));
    assert_eq!(rows[1][0][0].as_deref(), Some("0"), "return value");
    assert!(matches!(ev.last(), Some(Event::ExecFinished { had_errors: false, .. })), "{ev:?}");
}

#[test]
fn fixture_databases_are_dropped() {
    let info = live_or_skip!();
    let db = {
        let f = Fixture::new(info.clone());
        f.db.clone()
    };
    let check = Fixture::empty(info);
    let rows = check.fetch(&format!("SELECT DB_ID({})", sqlkraft::sql::quote_literal(&db)));
    assert_eq!(rows[0][0], None, "{db} still exists");
}
