use super::*;
use crate::db::Cell;

fn rows(r: &[&[&str]]) -> Vec<Vec<Cell>> {
    r.iter().map(|r| r.iter().map(|c| Some(c.to_string())).collect()).collect()
}

/// Like the test fixture: dbo.Customers, sales.[Order Lines], a view, procs and functions.
fn shop() -> Catalog {
    Catalog::from_rows(
        rows(&[&["dbo"], &["sales"]]),
        rows(&[
            &["dbo", "ActiveCustomers", "V"],
            &["dbo", "Customers", "U"],
            &["dbo", "GetCustomer", "P"],
            &["dbo", "Orders", "U"],
            &["dbo", "Twice", "FN"],
            &["sales", "LinesFor", "IF"],
            &["sales", "Order Lines", "U"],
        ]),
        rows(&[
            &["dbo", "Customers", "Id", "int", "4", "10", "0", "0"],
            &["dbo", "Customers", "Name", "nvarchar", "200", "0", "0", "0"],
            &["dbo", "Customers", "Email", "varchar", "255", "0", "0", "1"],
            &["dbo", "Orders", "OrderId", "int", "4", "10", "0", "0"],
            &["dbo", "Orders", "CustomerId", "int", "4", "10", "0", "0"],
            &["dbo", "Orders", "order_date", "date", "3", "10", "0", "1"],
            &["sales", "Order Lines", "Line]No", "int", "4", "10", "0", "0"],
            &["sales", "Order Lines", "Qty", "smallint", "2", "5", "0", "0"],
        ]),
    )
}

fn archive() -> Catalog {
    Catalog::from_rows(
        rows(&[&["dbo"], &["old"]]),
        rows(&[&["old", "Invoices", "U"]]),
        rows(&[&["old", "Invoices", "InvoiceNo", "int", "4", "10", "0", "0"]]),
    )
}

/// Context at the `|` in `s`.
fn ctx(s: &str) -> Option<Context> {
    let caret = s.find('|').expect("a | marks the caret");
    context(&s.replace('|', ""), caret)
}

fn names(s: &str) -> Vec<String> {
    run(s).items.into_iter().map(|i| i.name).collect()
}

fn run(s: &str) -> Suggestions {
    let (shop, archive) = (shop(), archive());
    let dbs = vec!["Archive".to_string(), "Shop".to_string(), "master".to_string()];
    let lookup = |db: &str| match db.to_lowercase().as_str() {
        "shop" => Some(&shop),
        "archive" => Some(&archive),
        _ => None,
    };
    let meta = Metadata { current_db: "Shop", databases: &dbs, catalog: &lookup };
    let c = ctx(s).expect("a context");
    suggest(&c, &meta)
}

fn first(s: &str) -> Item {
    run(s).items.into_iter().next().unwrap_or_else(|| panic!("nothing offered for {s}"))
}

fn alias_table(s: &str, alias: &str) -> Option<Vec<String>> {
    let c = ctx(s)?;
    c.tables.into_iter().find(|t| t.alias.as_deref() == Some(alias)).map(|t| t.parts)
}

#[test]
fn clauses() {
    let clause = |s| ctx(s).unwrap().clause;
    assert_eq!(clause("SELECT * FROM |"), Clause::Objects { tvf: true });
    assert_eq!(clause("SELECT * FROM a JOIN dbo.Cu|"), Clause::Objects { tvf: true });
    assert_eq!(clause("UPDATE |"), Clause::Objects { tvf: false });
    assert_eq!(clause("INSERT INTO |"), Clause::Objects { tvf: false });
    assert_eq!(clause("TRUNCATE TABLE |"), Clause::Objects { tvf: false });
    assert_eq!(clause("EXEC dbo.Get|"), Clause::Procedures);
    assert_eq!(clause("execute |"), Clause::Procedures);
    assert_eq!(clause("USE |"), Clause::Databases);
    assert_eq!(clause("SELECT |"), Clause::Expression);
    assert_eq!(clause("SELECT a, b|"), Clause::Expression);
    assert_eq!(clause("SELECT COUNT(*), |"), Clause::Expression);
    assert_eq!(clause("SELECT * FROM t WHERE |"), Clause::Expression);
    assert_eq!(clause("SELECT * FROM t WHERE x IN (1, |"), Clause::Expression);
    assert_eq!(clause("SELECT * FROM a x JOIN b y ON |"), Clause::Expression);
    assert_eq!(clause("SELECT a FROM t GROUP BY |"), Clause::Expression);
    assert_eq!(clause("SELECT a FROM t ORDER BY a, |"), Clause::Expression);
    assert_eq!(clause("UPDATE t SET |"), Clause::Expression);
    assert_eq!(clause("SELECT * FROM a x, |"), Clause::Objects { tvf: true });
    assert_eq!(clause("SELECT * FROM a x |"), Clause::General);
    assert_eq!(clause("|"), Clause::General);
    assert_eq!(clause("SELECT 1;\n|"), Clause::General);
}

#[test]
fn nothing_inside_strings_comments_numbers_or_variables() {
    assert!(ctx("SELECT 'FROM dbo.Cu|'").is_none());
    assert!(ctx("SELECT 'unterminated |").is_none());
    assert!(ctx("SELECT N'it''s|").is_none());
    assert!(ctx("SELECT 1 -- FROM dbo.|").is_none());
    assert!(ctx("SELECT /* dbo.| */ 1").is_none());
    assert!(ctx("SELECT /* open |").is_none());
    assert!(ctx("SELECT @va|").is_none());
    assert!(ctx("SELECT 12|").is_none());
    // Right after a closed string or comment is fine.
    assert!(ctx("SELECT 'x'|").is_some());
    assert!(ctx("SELECT /* x */|").is_some());
    assert!(ctx("-- note\nSELECT * FROM |").is_some());
}

#[test]
fn word_prefix_and_qualifier() {
    let c = ctx("SELECT * FROM dbo.Cu|").unwrap();
    assert_eq!((c.prefix.as_str(), c.qualifier.clone()), ("Cu", vec!["dbo".to_string()]));
    assert_eq!(c.word, 18..20);

    // Caret mid-word: the prefix stops at the caret, the whole word is replaced.
    let c = ctx("SELECT * FROM dbo.Cus|tomers c").unwrap();
    assert_eq!(c.prefix, "Cus");
    assert_eq!(c.word, 18..27);

    let c = ctx("SELECT c.| FROM dbo.Customers c").unwrap();
    assert_eq!((c.prefix.as_str(), c.qualifier.clone(), c.word.clone()), ("", vec!["c".to_string()], 9..9));

    let c = ctx("SELECT * FROM [Archive].old.|").unwrap();
    assert_eq!(c.qualifier, vec!["Archive", "old"]);
    let c = ctx("SELECT * FROM Archive..Inv|").unwrap();
    assert_eq!(c.qualifier, vec!["Archive", ""]);

    // Inside brackets the prefix is unquoted and the result is bracketed.
    let c = ctx("SELECT * FROM sales.[Order L|").unwrap();
    assert_eq!((c.prefix.as_str(), c.bracketed), ("Order L", true));
    assert_eq!(c.word, 20..28);
}

#[test]
fn aliases_with_and_without_as() {
    let s = "SELECT o.| FROM dbo.Orders AS o JOIN dbo.Customers c ON c.Id = o.CustomerId";
    assert_eq!(alias_table(s, "o"), Some(vec!["dbo".into(), "Orders".into()]));
    assert_eq!(alias_table(s, "c"), Some(vec!["dbo".into(), "Customers".into()]));
    assert_eq!(names(s), ["CustomerId", "order_date", "OrderId"]);
    let s = "SELECT c.| FROM dbo.Orders AS o JOIN dbo.Customers c ON c.Id = o.CustomerId";
    assert_eq!(names(s), ["Email", "Id", "Name"]);
}

#[test]
fn bracketed_names_and_aliases() {
    let s = "SELECT [ol].| FROM [sales].[Order Lines] AS [ol]";
    assert_eq!(alias_table(s, "ol"), Some(vec!["sales".into(), "Order Lines".into()]));
    let items = run(s).items;
    assert_eq!(items.iter().map(|i| i.insert.as_str()).collect::<Vec<_>>(), ["[Line]]No]", "Qty"]);
    assert_eq!(items[0].kind, Kind::Column);
    assert_eq!(items[1].detail, "smallint, not null · Order Lines");
}

#[test]
fn multi_join_and_comma_joins() {
    let s = "SELECT | FROM dbo.Orders o, Shop.dbo.ActiveCustomers a\n  INNER JOIN dbo.Customers c ON c.Id = o.CustomerId\n  LEFT JOIN sales.[Order Lines] l ON 1 = 1";
    let c = ctx(s).unwrap();
    let tables: Vec<(String, String)> = c.tables.iter().map(|t| (t.name().into(), t.alias.clone().unwrap())).collect();
    let expected = [("Orders", "o"), ("ActiveCustomers", "a"), ("Customers", "c"), ("Order Lines", "l")];
    assert_eq!(tables, expected.map(|(n, a)| (n.to_string(), a.to_string())));
    assert_eq!(c.tables[1].database(), Some("Shop"));
    // Columns of every table in scope come first.
    let n = names(s);
    assert_eq!(&n[..3], ["CustomerId", "Email", "Id"]);
    assert!(n.contains(&"Qty".to_string()) && n.contains(&"SELECT".to_string()));
    // A table without an alias is qualified by its name.
    assert_eq!(names("SELECT Customers.| FROM dbo.Customers WHERE 1 = 1"), ["Email", "Id", "Name"]);
    // Subqueries, table hints and APPLY don't confuse it.
    let s = "SELECT x.| FROM dbo.Customers c WITH (NOLOCK) CROSS APPLY sales.LinesFor(c.Id) x JOIN (SELECT 1 AS n) d ON 1 = 1";
    assert_eq!(alias_table(s, "x"), Some(vec!["sales".into(), "LinesFor".into()]));
    assert_eq!(alias_table(s, "c"), Some(vec!["dbo".into(), "Customers".into()]));
}

#[test]
fn statements_have_their_own_aliases() {
    let s = "SELECT * FROM dbo.Customers x\nSELECT x.| FROM dbo.Orders x\nGO\nSELECT * FROM sales.[Order Lines] x";
    assert_eq!(names(s), ["CustomerId", "order_date", "OrderId"]);
    // `x` defined only in another statement of the batch is still resolved.
    let s = "SELECT * FROM dbo.Customers c;\nSELECT c.|";
    assert!(ctx(s).unwrap().tables.is_empty());
    assert_eq!(names(s), ["Email", "Id", "Name"]);
    // Other batches don't count.
    assert!(names("SELECT * FROM dbo.Customers c\nGO\nSELECT c.|").is_empty());
    // INSERT ... SELECT and UNION stay one statement.
    let s = "INSERT INTO dbo.Orders (CustomerId) SELECT c.| FROM dbo.Customers c";
    assert_eq!(names(s), ["Email", "Id", "Name"]);
    let s = "SELECT 1 FROM dbo.Orders o UNION ALL SELECT o.| FROM dbo.Customers c";
    assert_eq!(names(s), ["CustomerId", "order_date", "OrderId"]);
}

#[test]
fn schema_and_database_qualified() {
    // schema. → that schema's objects, filtered by clause.
    assert_eq!(names("SELECT * FROM dbo.|"), ["ActiveCustomers", "Customers", "Orders"]);
    assert_eq!(names("SELECT * FROM sales.|"), ["LinesFor", "Order Lines"]);
    assert_eq!(names("EXEC dbo.|"), ["GetCustomer"]);
    // db. → its schemas; db.schema. → its objects; db.schema.object. → columns.
    assert_eq!(names("SELECT * FROM Archive.|"), ["dbo", "old"]);
    assert_eq!(names("SELECT * FROM Archive.old.|"), ["Invoices"]);
    assert_eq!(names("SELECT Archive.old.Invoices.| FROM Archive.old.Invoices"), ["InvoiceNo"]);
    let s = "SELECT i.| FROM [Archive].[old].[Invoices] i";
    assert_eq!(names(s), ["InvoiceNo"]);
    // db..object uses dbo.
    assert!(names("SELECT * FROM Archive..|").is_empty());
    // schema.object. → columns.
    assert_eq!(names("SELECT dbo.Customers.| FROM dbo.Customers"), ["Email", "Id", "Name"]);
}

#[test]
fn unloaded_databases_are_reported() {
    let s = run("SELECT * FROM master.|");
    assert!(s.items.is_empty());
    assert_eq!(s.missing, ["master"]);
    assert!(run("SELECT * FROM dbo.|").missing.is_empty());
}

#[test]
fn unqualified_objects() {
    let s = run("SELECT * FROM Ord|");
    let got: Vec<_> = s.items.iter().map(|i| (i.name.as_str(), i.insert.as_str(), i.kind)).collect();
    assert_eq!(got, [("Order Lines", "sales.[Order Lines]", Kind::Table), ("Orders", "Orders", Kind::Table)]);
    assert_eq!(names("SELECT * FROM |").first().map(String::as_str), Some("ActiveCustomers"));
    // Schemas and databases are offered after the objects.
    let n = names("SELECT * FROM s|");
    assert_eq!(n, ["sales", "Shop"]);
    assert_eq!(first("USE Ar|").kind, Kind::Database);
    assert_eq!(first("EXEC Get|").insert, "GetCustomer");
    // Scalar functions in expressions are schema-qualified.
    assert_eq!(first("SELECT Twi|").insert, "dbo.Twice");
}

#[test]
fn ranking() {
    assert_eq!(score("customerid", "CustomerId"), Some(0));
    assert_eq!(score("cust", "CustomerId"), Some(1));
    assert_eq!(score("cuid", "CustomerId"), Some(2));
    assert_eq!(score("ci", "CustomerId"), Some(2));
    assert_eq!(score("od", "order_date"), Some(2));
    assert_eq!(score("ord_da", "order_date"), Some(2));
    assert_eq!(score("ol", "Order Lines"), Some(2));
    assert_eq!(score("id", "CustomerId"), None, "the first segment must match");
    assert_eq!(score("xyz", "CustomerId"), None);
    assert_eq!(segments("XMLParser2"), ["XML", "Parser", "2"]);
    assert_eq!(segments("order_lines"), ["order", "lines"]);

    // Exact, then prefix, then segment matches; columns before functions before keywords.
    let s = "SELECT o.cuid FROM dbo.Orders o";
    let c = context(s, 13).unwrap();
    assert_eq!(c.prefix, "cuid");
    assert_eq!(names("SELECT o.cuid| FROM dbo.Orders o"), ["CustomerId"]);
    let n = names("SELECT o| FROM dbo.Orders o");
    assert_eq!(&n[..3], ["order_date", "OrderId", "OBJECT_ID"]);
    assert_eq!(names("SELECT * FROM dbo.Customers c WHERE c.n|"), ["Name"]);
    assert_eq!(first("SEL|").name, "SELECT");
}

#[test]
fn inserting_quotes_and_keeps_keyword_case() {
    assert_eq!(first("sel|").insert, "select");
    assert_eq!(first("Sel|").insert, "SELECT");
    assert_eq!(first("SELECT * FROM t wher|").insert, "where");
    assert_eq!(first("SELECT cou|").insert, "count");
    assert_eq!(first("SELECT * FROM sales.Ord|").insert, "[Order Lines]");
    assert_eq!(first("SELECT * FROM sales.[Ord|").insert, "[Order Lines]");
    assert_eq!(first("SELECT * FROM [Cust|").insert, "[Customers]");
    assert!(needs_quoting("Order Lines") && needs_quoting("Line]No") && needs_quoting("order") && needs_quoting("1st"));
    assert!(!needs_quoting("Customers") && !needs_quoting("order_date") && !needs_quoting("#tmp"));
}

#[test]
fn ddl_detection() {
    assert!(has_ddl("create table t (a int)"));
    assert!(has_ddl("ALTER VIEW v AS SELECT 1"));
    assert!(has_ddl("DROP PROCEDURE p"));
    assert!(has_ddl("SELECT * INTO #t FROM x"));
    assert!(has_ddl("EXEC sp_rename 'a', 'b'"));
    assert!(!has_ddl("INSERT INTO t VALUES (1)"));
    assert!(!has_ddl("SELECT 'create table' -- drop it"));
}

#[test]
fn catalog_queries_name_the_database() {
    for q in Catalog::queries("My]Db") {
        assert!(q.contains("[My]]Db].sys."), "{q}");
        assert!(!q.contains(" sys."), "every catalog view is database-qualified: {q}");
    }
    let c = shop();
    assert_eq!(c.find(None, "customers").map(|o| o.schema.as_str()), Some("dbo"));
    assert_eq!(c.find(Some("SALES"), "order lines").map(|o| o.kind), Some(ObjKind::Table));
    let name = Column {
        name: "Name".into(),
        type_name: "nvarchar(100), not null".into(),
        nullable: false,
        literal: Some(Literal::Unicode),
    };
    assert_eq!(c.columns("DBO", "customers")[1], name);
    // Rows without the base type and computed flag (older servers' shape) still load.
    assert_eq!(c.columns("dbo", "customers")[2].literal, Some(Literal::Ansi));
    assert_eq!(c.columns("dbo", "customers")[0].literal, None);
}
