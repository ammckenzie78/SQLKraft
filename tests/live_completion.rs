//! The completion metadata loader against a live server. See `tests/common/mod.rs`; without
//! `SQLKRAFT_TEST_SERVER` these tests are skipped.

mod common;

use common::Fixture;
use sqlkraft::completion::{Catalog, Kind, Metadata, ObjKind, context, suggest};

/// The catalog queries name the database themselves: the fixture session stays in its login's
/// default database (normally master), never the fixture database.
#[test]
fn catalog_loads_the_fixture_from_another_database() {
    let f = Fixture::new(live_or_skip!());
    let current = f.fetch("SELECT DB_NAME()");
    let current = current[0][0].clone().unwrap_or_default();
    assert_ne!(current, f.db, "the session must not be in the fixture database");
    eprintln!("session database: {current}");

    let [schemas, objects, columns] = Catalog::queries(&f.db).map(|q| f.fetch(&q));
    let cat = Catalog::from_rows(schemas, objects, columns);

    assert!(cat.has_schema("dbo") && cat.has_schema("sales"), "{:?}", cat.schemas);
    let kind = |schema: &str, name: &str| cat.find(Some(schema), name).map(|o| o.kind);
    assert_eq!(kind("dbo", "Customers"), Some(ObjKind::Table));
    assert_eq!(kind("sales", "Order Lines"), Some(ObjKind::Table));
    assert_eq!(kind("dbo", "ActiveCustomers"), Some(ObjKind::View));
    assert_eq!(kind("dbo", "GetCustomer"), Some(ObjKind::Procedure));
    assert_eq!(kind("sales", "Odd]Name Proc"), Some(ObjKind::Procedure));
    assert_eq!(kind("dbo", "Twice"), Some(ObjKind::ScalarFunction));
    assert_eq!(kind("sales", "LinesFor"), Some(ObjKind::TableFunction));
    assert_eq!(cat.objects.len(), 7, "{:?}", cat.objects);

    let cols = |schema: &str, name: &str| -> Vec<(String, String)> {
        cat.columns(schema, name).iter().map(|c| (c.name.clone(), c.type_name.clone())).collect()
    };
    let customers = cols("dbo", "Customers");
    let expected = [
        ("Id", "int, not null"),
        ("Name", "nvarchar(100), not null"),
        ("Email", "varchar(255), null"),
        ("Balance", "decimal(18,2), not null"),
        ("CreatedAt", "datetime2(3), not null"),
        ("Notes", "nvarchar(max), null"),
    ];
    assert_eq!(&customers[..6], expected.map(|(n, t)| (n.to_string(), t.to_string())));
    assert_eq!(customers[6].0, "Shout");
    let lines: Vec<String> = cols("sales", "Order Lines").into_iter().map(|c| c.0).collect();
    assert_eq!(lines, ["OrderId", "Line]No", "Qty", "Price", "Payload"]);
    assert_eq!(cols("dbo", "ActiveCustomers").len(), 2, "views have columns");
    assert_eq!(cols("sales", "LinesFor").len(), 5, "inline table-valued functions have columns");

    // End to end: what the editor would offer with this catalog.
    let dbs = vec![f.db.clone()];
    let lookup = |db: &str| db.eq_ignore_ascii_case(&f.db).then_some(&cat);
    let meta = Metadata { current_db: &f.db, databases: &dbs, catalog: &lookup };
    let offer = |s: &str| {
        let c = context(s, s.len()).unwrap();
        suggest(&c, &meta).items
    };
    let items = offer("SELECT * FROM sales.Ord");
    assert_eq!((items[0].insert.as_str(), items[0].kind), ("[Order Lines]", Kind::Table));
    let items = offer("SELECT * FROM sales.[Order Lines] AS l WHERE l.L");
    assert_eq!(items[0].insert, "[Line]]No]");
    let items = offer("EXEC sales.");
    assert_eq!(items.iter().map(|i| i.insert.as_str()).collect::<Vec<_>>(), ["[Odd]]Name Proc]"]);
}
