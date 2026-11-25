//! The table designer against a live server: load a fixture table, script changes, run the
//! script, and check the result in the catalog. See `tests/common/mod.rs` for setup.

mod common;

use common::Fixture;
use sqlkraft::db::{Event, MsgKind};
use sqlkraft::designer::{self, Column, Design};
use sqlkraft::sql::{quote_ident, quote_literal};

/// Loads `[schema].[name]` from the fixture database on the fixture's session, which is in
/// master, like the Object Explorer connection.
fn load(f: &Fixture, schema: &str, name: &str) -> Design {
    let rows = f.fetch(&designer::load_query(&f.db, schema, name));
    Design::existing(f.db.clone(), designer::table_from_rows(schema, name, &rows).unwrap())
}

/// Runs a script like a query tab and fails on any error message.
fn run(f: &mut Fixture, script: &str) {
    let events = f.execute(script);
    let errors: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Event::Message(m) if m.kind == MsgKind::Error => Some(m.text.clone()),
            _ => None,
        })
        .collect();
    assert!(errors.is_empty(), "{errors:?}\n{script}");
    assert!(events.iter().any(|e| matches!(e, Event::ExecFinished { had_errors: false, .. })), "{events:?}");
}

/// (column, type, nullable) in column order, read from the catalog.
fn columns(f: &Fixture, schema: &str, name: &str) -> Vec<(String, String, bool)> {
    let d = quote_ident(&f.db);
    let full = format!("{d}.{}.{}", quote_ident(schema), quote_ident(name));
    let rows = f.fetch(&format!(
        "SELECT c.name, ty.name, c.max_length, c.precision, c.scale, c.is_nullable FROM {d}.sys.columns c \
         JOIN {d}.sys.types ty ON ty.user_type_id = c.user_type_id \
         WHERE c.object_id = OBJECT_ID({}) ORDER BY c.column_id",
        quote_literal(&full)
    ));
    let s = |r: &[Option<String>], i: usize| r[i].clone().unwrap_or_default();
    rows.iter()
        .map(|r| (s(r, 0), sqlkraft::explorer::type_spec(&s(r, 1), &s(r, 2), &s(r, 3), &s(r, 4)), s(r, 5) == "1"))
        .collect()
}

/// (constraint, column, definition) of the table's defaults, by column.
fn defaults(f: &Fixture, schema: &str, name: &str) -> Vec<(String, String, String)> {
    let d = quote_ident(&f.db);
    let full = format!("{d}.{}.{}", quote_ident(schema), quote_ident(name));
    let rows = f.fetch(&format!(
        "SELECT dc.name, c.name, dc.definition FROM {d}.sys.default_constraints dc \
         JOIN {d}.sys.columns c ON c.object_id = dc.parent_object_id AND c.column_id = dc.parent_column_id \
         WHERE dc.parent_object_id = OBJECT_ID({}) ORDER BY c.column_id",
        quote_literal(&full)
    ));
    rows.iter().map(|r| (r[0].clone().unwrap(), r[1].clone().unwrap(), r[2].clone().unwrap())).collect()
}

/// The primary key's name and its columns in key order.
fn primary_key(f: &Fixture, schema: &str, name: &str) -> (String, Vec<String>) {
    let d = quote_ident(&f.db);
    let full = format!("{d}.{}.{}", quote_ident(schema), quote_ident(name));
    let rows = f.fetch(&format!(
        "SELECT kc.name, c.name FROM {d}.sys.key_constraints kc \
         JOIN {d}.sys.index_columns ic ON ic.object_id = kc.parent_object_id AND ic.index_id = kc.unique_index_id \
         JOIN {d}.sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
         WHERE kc.parent_object_id = OBJECT_ID({}) AND kc.type = 'PK' ORDER BY ic.key_ordinal",
        quote_literal(&full)
    ));
    let pk = rows.first().and_then(|r| r[0].clone()).unwrap_or_default();
    (pk, rows.iter().map(|r| r[1].clone().unwrap()).collect())
}

fn col<'a>(d: &'a mut Design, name: &str) -> &'a mut Column {
    d.table.columns.iter_mut().find(|c| c.name == name).unwrap_or_else(|| panic!("no column {name}"))
}

#[test]
fn design_loads_customers_and_alters_it() {
    let mut f = Fixture::new(live_or_skip!());
    let mut d = load(&f, "dbo", "Customers");

    // As loaded.
    let t = &d.table;
    assert_eq!(t.pk_name.as_deref(), Some("PK_Customers"));
    let names: Vec<_> = t.columns.iter().map(|c| c.name.as_str()).collect();
    assert_eq!(names, ["Id", "Name", "Email", "Balance", "CreatedAt", "Notes", "Shout"]);
    let id = &t.columns[0];
    assert!(id.identity && id.primary_key && !id.nullable && id.seed == "1" && id.increment == "1");
    assert_eq!(t.columns[3].type_sql().unwrap(), "decimal(18,2)");
    assert_eq!(t.columns[3].default_name.as_deref(), Some("DF_Customers_Balance"));
    assert_eq!(t.columns[4].type_sql().unwrap(), "datetime2(3)");
    assert_eq!(t.columns[5].type_sql().unwrap(), "nvarchar(max)");
    assert!(t.columns[6].computed.is_some());
    assert_eq!(d.alter_statements().unwrap_err(), ["No changes to script."]);

    // Balance changes type (its default must be dropped and added back), Notes is renamed and
    // resized, CreatedAt (with a default) is dropped, a column is added and Name joins the key.
    col(&mut d, "Balance").data_type = "money".into();
    let notes = col(&mut d, "Notes");
    notes.name = "Remarks".into();
    notes.length = "500".into();
    d.table.columns.retain(|c| c.name != "CreatedAt");
    let mut status = Column::new("Status");
    status.data_type = "tinyint".into();
    status.nullable = false;
    status.default = "1".into();
    d.table.columns.push(status);
    col(&mut d, "Name").primary_key = true;

    let script = d.script().unwrap();
    run(&mut f, &script);

    assert_eq!(
        columns(&f, "dbo", "Customers"),
        [
            ("Id".into(), "int".into(), false),
            ("Name".into(), "nvarchar(100)".into(), false),
            ("Email".into(), "varchar(255)".into(), true),
            ("Balance".into(), "money".into(), false),
            ("Remarks".into(), "nvarchar(500)".into(), true),
            ("Shout".into(), "nvarchar(100)".into(), true),
            ("Status".into(), "tinyint".into(), false),
        ]
    );
    assert_eq!(
        defaults(&f, "dbo", "Customers"),
        [
            ("DF_Customers_Balance".into(), "Balance".into(), "((0))".into()),
            ("DF_Customers_Status".into(), "Status".into(), "((1))".into()),
        ]
    );
    assert_eq!(primary_key(&f, "dbo", "Customers"), ("PK_Customers".into(), vec!["Id".into(), "Name".into()]));
    // The data survived.
    let rows = f.fetch(&format!("SELECT COUNT(*), SUM(Status) FROM {}.dbo.Customers", quote_ident(&f.db)));
    assert_eq!(rows[0], [Some("2".into()), Some("2".into())]);

    // Loading again shows the new definition, with nothing left to change.
    let again = load(&f, "dbo", "Customers");
    assert_eq!(again.alter_statements().unwrap_err(), ["No changes to script."]);
}

#[test]
fn design_alters_a_table_with_odd_names() {
    let mut f = Fixture::new(live_or_skip!());
    let mut d = load(&f, "sales", "Order Lines");
    assert_eq!(d.table.pk_name.as_deref(), Some("PK_OrderLines"));
    assert!(d.table.columns[1].key_descending, "[Line]]No] DESC in the key");

    // Rename a key column to a name with a quote and a bracket (a rename alone leaves the key
    // alone), change Qty's type and give it a default, and change Price's type.
    d.table.columns[1].name = "Line ]No'".into();
    let qty = col(&mut d, "Qty");
    qty.data_type = "int".into();
    qty.default = "1".into();
    let price = col(&mut d, "Price");
    price.data_type = "decimal".into();
    price.precision = "19".into();
    price.scale = "4".into();
    let script = d.script().unwrap();
    assert!(!script.contains("PK_OrderLines"), "a rename leaves the key alone:\n{script}");
    run(&mut f, &script);

    assert_eq!(
        columns(&f, "sales", "Order Lines"),
        [
            ("OrderId".into(), "uniqueidentifier".into(), false),
            ("Line ]No'".into(), "int".into(), false),
            ("Qty".into(), "int".into(), false),
            ("Price".into(), "decimal(19,4)".into(), true),
            ("Payload".into(), "varbinary(max)".into(), true),
        ]
    );
    assert_eq!(defaults(&f, "sales", "Order Lines"), [("DF_Order Lines_Qty".into(), "Qty".into(), "((1))".into())]);
    assert_eq!(primary_key(&f, "sales", "Order Lines"), ("PK_OrderLines".into(), vec!["OrderId".into(), "Line ]No'".into()]));
}

#[test]
fn new_table_script_creates_the_table() {
    let mut f = Fixture::new(live_or_skip!());
    let mut d = Design::new_table(f.db.clone());
    d.table.schema = "sales".into();
    d.table.name = "Widget's [Parts]".into();
    let mut id = Column::new("Id");
    id.data_type = "bigint".into();
    id.identity = true;
    id.seed = "100".into();
    id.increment = "5".into();
    id.nullable = false;
    id.primary_key = true;
    let mut region = Column::new("Region");
    region.data_type = "char".into();
    region.length = "2".into();
    region.nullable = false;
    region.primary_key = true;
    region.default = "'NZ'".into();
    let mut price = Column::new("Price");
    price.data_type = "numeric".into();
    price.precision = "10".into();
    price.scale = "3".into();
    let mut at = Column::new("At");
    at.data_type = "datetimeoffset".into();
    at.scale = "2".into();
    at.nullable = false;
    at.default = "sysdatetimeoffset()".into();
    let label = Column::new("Label");
    d.table.columns = vec![id, region, price, at, label];

    let script = d.script().unwrap();
    run(&mut f, &script);

    let (s, t) = ("sales", "Widget's [Parts]");
    assert_eq!(
        columns(&f, s, t),
        [
            ("Id".into(), "bigint".into(), false),
            ("Region".into(), "char(2)".into(), false),
            ("Price".into(), "numeric(10,3)".into(), true),
            ("At".into(), "datetimeoffset(2)".into(), false),
            ("Label".into(), "nvarchar(50)".into(), true),
        ]
    );
    assert_eq!(
        defaults(&f, s, t),
        [
            ("DF_Widget's [Parts]_Region".into(), "Region".into(), "('NZ')".into()),
            ("DF_Widget's [Parts]_At".into(), "At".into(), "(sysdatetimeoffset())".into()),
        ]
    );
    assert_eq!(primary_key(&f, s, t), ("PK_Widget's [Parts]".into(), vec!["Id".into(), "Region".into()]));

    // Identity seed and increment, through the designer's own loader.
    let loaded = load(&f, s, t);
    let id = &loaded.table.columns[0];
    assert!(id.identity && id.seed == "100" && id.increment == "5", "{id:?}");
    assert_eq!(loaded.table.pk_type.as_deref(), Some("CLUSTERED"));
}
