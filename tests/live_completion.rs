//! The completion metadata loader against a live server. See `tests/common/mod.rs`; without
//! `SQLKRAFT_TEST_SERVER` these tests are skipped.

mod common;

use common::Fixture;
use sqlkraft::completion::values::{self, Resolved, Target, Value};
use sqlkraft::completion::{Catalog, Kind, Literal, Metadata, ObjKind, context, suggest};

/// The value suggestions' queries against `dbo.Places` (known frequencies, see
/// `common::PLACES`), run from the fixture session, which is not in the fixture database.
#[test]
fn value_suggestions_rank_match_escape_and_cap() {
    let f = Fixture::new(live_or_skip!());
    f.seed(&f.db, common::PLACES);
    let current = f.fetch("SELECT DB_NAME()")[0][0].clone().unwrap_or_default();
    assert_ne!(current, f.db, "the lookup must work from another database");
    eprintln!("session database: {current}");

    let [schemas, objects, columns] = Catalog::queries(&f.db).map(|q| f.fetch(&q));
    let cat = Catalog::from_rows(schemas, objects, columns);
    let literal = |t: &str, c: &str| cat.columns("dbo", t).iter().find(|x| x.name == c).unwrap().literal;
    assert_eq!(literal("Places", "City"), Some(Literal::Unicode));
    assert_eq!(literal("Places", "Code"), Some(Literal::Ansi));
    assert_eq!(literal("Customers", "Email"), Some(Literal::Ansi));
    assert_eq!(literal("Customers", "Notes"), None, "nvarchar(max)");
    assert_eq!(literal("Customers", "Shout"), None, "computed, not persisted");

    let dbs = vec![f.db.clone()];
    let lookup = |db: &str| db.eq_ignore_ascii_case(&f.db).then_some(&cat);
    let meta = Metadata { current_db: &f.db, databases: &dbs, catalog: &lookup };
    let text = "SELECT * FROM dbo.Places c WHERE c.City = 'ma";
    let spot = values::spot(text, text.len()).expect("a value spot");
    let Resolved::Column(t) = values::resolve(&spot, &meta) else { panic!("c.City not resolved") };
    assert_eq!((t.db.as_str(), t.table.as_str(), t.column.name.as_str()), (f.db.as_str(), "Places", "City"));

    let lookup_values = |t: &Target, typed: &str, cap: u64| -> Vec<Value> {
        values::from_rows(&f.fetch(&values::value_query(t, typed, cap)))
    };
    let names = |v: &[Value]| -> Vec<String> { v.iter().map(|v| v.value.clone().unwrap_or("NULL".into())).collect() };

    // Contains matching (`ma` finds Wautoma), prefix matches first, then by count, then name.
    let got = lookup_values(&t, "ma", 200_000);
    let expected = ["Madison", "Marshfield", "Mauston", "Mayville", "Manitowoc", "Wautoma"];
    assert_eq!(names(&got), expected, "server order");
    assert_eq!(names(&values::rank(&got, "ma")), expected, "the same ranking locally");
    assert_eq!(got.iter().map(|v| v.count).collect::<Vec<_>>(), [5, 3, 2, 2, 1, 4]);
    assert_eq!(names(&lookup_values(&t, "MA", 200_000)), expected, "case-insensitive");
    assert_eq!(names(&lookup_values(&t, "ton", 200_000)), ["Mauston"]);

    // The typed text is literal: no wildcards, quotes escaped.
    assert_eq!(names(&lookup_values(&t, "%", 200_000)), ["100%_[x]"]);
    assert_eq!(names(&lookup_values(&t, "_[", 200_000)), ["100%_[x]"]);
    assert_eq!(names(&lookup_values(&t, "o'b", 200_000)), ["O'Brien"]);

    // Nothing typed: every value, NULL among them (it becomes IS NULL after `=`).
    let all = lookup_values(&t, "", 200_000);
    assert_eq!(all.len(), 10);
    assert_eq!((all[0].value.as_deref(), all[0].count), (Some("Oshkosh"), 6));
    assert!(all.contains(&Value { value: None, count: 2 }), "{all:?}");

    // Insert quoting per type.
    let items = values::items(&got, &t, &spot, false);
    assert_eq!((items[0].insert.as_str(), items[0].detail.as_str(), items[0].kind), ("N'Madison'", "5", Kind::Value));
    let code = "SELECT * FROM dbo.Places WHERE Code = 'W";
    let code_spot = values::spot(code, code.len()).unwrap();
    let Resolved::Column(ct) = values::resolve(&code_spot, &meta) else { panic!("Code not resolved") };
    let code_items = values::items(&lookup_values(&ct, "W", 200_000), &ct, &code_spot, false);
    assert_eq!(code_items.iter().map(|i| (i.insert.as_str(), i.detail.as_str())).collect::<Vec<_>>(), [("'wi'", "27")]);

    // The row cap: only 10 rows are read, so the counts are a sample, marked approximate.
    let rows: u64 = f.fetch(&values::size_query(&t))[0][0].clone().unwrap().parse().unwrap();
    assert_eq!(rows, 27);
    let sample = lookup_values(&t, "", 10);
    assert_eq!(sample.iter().map(|v| v.count).sum::<u64>(), 10, "{sample:?}");
    let cap = 10;
    let approximate = rows > cap;
    let items = values::items(&values::rank(&sample, ""), &t, &spot, approximate);
    assert!(items.iter().all(|i| i.detail.starts_with('~')), "{items:?}");
}

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
