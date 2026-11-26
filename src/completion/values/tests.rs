use super::*;
use crate::completion::Catalog;
use crate::db::Cell;

fn rows(r: &[&[&str]]) -> Vec<Vec<Cell>> {
    r.iter().map(|r| r.iter().map(|c| Some(c.to_string())).collect()).collect()
}

/// dbo.Places with character columns of each kind, plus ones whose values are never offered.
fn places() -> Catalog {
    Catalog::from_rows(
        rows(&[&["dbo"], &["geo"]]),
        rows(&[&["dbo", "Places", "U"], &["dbo", "PlaceView", "V"], &["geo", "Towns", "U"]]),
        rows(&[
            &["dbo", "Places", "Id", "int", "4", "10", "0", "0", "int", "0"],
            &["dbo", "Places", "City", "nvarchar", "100", "0", "0", "1", "nvarchar", "0"],
            &["dbo", "Places", "Code", "varchar", "10", "0", "0", "0", "varchar", "0"],
            &["dbo", "Places", "State", "char", "2", "0", "0", "0", "char", "0"],
            &["dbo", "Places", "Notes", "nvarchar", "-1", "0", "0", "1", "nvarchar", "0"],
            &["dbo", "Places", "Upper City", "nvarchar", "200", "0", "0", "1", "nvarchar", "1"],
            &["dbo", "Places", "Login", "sysname", "256", "0", "0", "0", "nvarchar", "0"],
            &["dbo", "Places", "Body", "text", "16", "0", "0", "1", "text", "0"],
            &["dbo", "PlaceView", "City", "nvarchar", "100", "0", "0", "1", "nvarchar", "0"],
            &["geo", "Towns", "Name", "varchar", "50", "0", "0", "0", "varchar", "0"],
        ]),
    )
}

/// Spot at the `|` in `s`.
fn at(s: &str) -> Option<Spot> {
    let caret = s.find('|').expect("a | marks the caret");
    spot(&s.replace('|', ""), caret)
}

fn resolved(s: &str) -> Resolved {
    let cat = places();
    let dbs = vec!["Shop".to_string()];
    let lookup = |db: &str| db.eq_ignore_ascii_case("shop").then_some(&cat);
    let meta = Metadata { current_db: "Shop", databases: &dbs, catalog: &lookup };
    resolve(&at(s).expect("a value spot"), &meta)
}

fn target(s: &str) -> Target {
    match resolved(s) {
        Resolved::Column(t) => t,
        other => panic!("no column for {s}: {other:?}"),
    }
}

fn v(value: &str, count: u64) -> Value {
    Value { value: Some(value.to_string()), count }
}

#[test]
fn operators() {
    let op = |s| at(s).map(|p| p.op);
    assert_eq!(op("SELECT * FROM Places WHERE City = |"), Some(Op::Eq));
    assert_eq!(op("SELECT * FROM Places WHERE City='|"), Some(Op::Eq));
    assert_eq!(op("SELECT * FROM Places WHERE City <> 'ma|"), Some(Op::NotEq));
    assert_eq!(op("SELECT * FROM Places WHERE City != 'ma|"), Some(Op::NotEq));
    assert_eq!(op("SELECT * FROM Places WHERE City LIKE 'ma|"), Some(Op::Like));
    assert_eq!(op("SELECT * FROM Places WHERE City NOT LIKE 'ma|"), Some(Op::Like));
    assert_eq!(op("SELECT * FROM Places WHERE City IN (|"), Some(Op::In));
    assert_eq!(op("SELECT * FROM Places WHERE City IN ('|"), Some(Op::In));
    assert_eq!(op("SELECT * FROM Places WHERE City IN ('Madison', |"), Some(Op::In));
    assert_eq!(op("SELECT * FROM Places WHERE City NOT IN (N'Madison', 'x', N'ma|"), Some(Op::In));
    assert_eq!(op("SELECT * FROM Places WHERE Id > 1 AND City = 'ma|"), Some(Op::Eq));
    assert_eq!(op("SELECT * FROM Places p JOIN Places q ON q.City = 'ma|"), Some(Op::Eq));
    assert_eq!(op("UPDATE Places SET Code = 'x' WHERE City = 'ma|"), Some(Op::Eq));
    // Not value comparisons.
    assert_eq!(op("SELECT * FROM Places WHERE City >= 'ma|"), None);
    assert_eq!(op("SELECT * FROM Places WHERE City <= 'ma|"), None);
    assert_eq!(op("SELECT * FROM Places WHERE City > 'ma|"), None);
    assert_eq!(op("SELECT * FROM Places WHERE City IN (SELECT |"), None);
    assert_eq!(op("SELECT * FROM Places WHERE City IN (SELECT x FROM y WHERE z = 1, |"), None);
    assert_eq!(op("SELECT * FROM Places WHERE UPPER(City) = 'ma|"), None);
    assert_eq!(op("SELECT * FROM Places WHERE @v = 'ma|"), None);
    assert_eq!(op("SELECT * FROM Places WHERE 'a' = 'ma|"), None);
    assert_eq!(op("DECLARE @x nvarchar(10) = 'ma|"), None);
    assert_eq!(op("SELECT 'ma|"), None);
}

#[test]
fn typed_text_and_replaced_range() {
    let s = at("SELECT * FROM Places WHERE City = 'ma|").unwrap();
    assert!(s.quoted);
    assert_eq!(s.typed, "ma");
    assert_eq!(s.replace, 34..37, "from the quote to the caret");

    let s = at("SELECT * FROM Places WHERE City = N'ma|").unwrap();
    assert_eq!((s.typed.as_str(), s.replace.start), ("ma", 34), "the N prefix is replaced too");

    // A closed literal with the caret inside: the whole literal is replaced.
    let s = at("SELECT * FROM Places WHERE City = 'ma|di' AND 1=1").unwrap();
    assert_eq!((s.typed.as_str(), s.replace.clone()), ("ma", 34..40));

    let s = at("SELECT * FROM Places WHERE City = 'O''|").unwrap();
    assert_eq!(s.typed, "O'", "doubled quotes are one quote");

    let s = at("SELECT * FROM Places WHERE City = '|").unwrap();
    assert_eq!((s.typed.as_str(), s.quoted), ("", true));

    // Without a quote: the bare word, or nothing yet.
    let s = at("SELECT * FROM Places WHERE City = ma|").unwrap();
    assert_eq!((s.typed.as_str(), s.quoted, s.replace.clone()), ("ma", false, 34..36));
    let s = at("SELECT * FROM Places WHERE City = |").unwrap();
    assert_eq!((s.typed.as_str(), s.replace.clone()), ("", 34..34));
    let s = at("SELECT * FROM Places WHERE City =|").unwrap();
    assert_eq!(s.replace, 33..33);

    // Just after a finished literal there is nothing to complete.
    assert_eq!(at("SELECT * FROM Places WHERE City = 'Madison'|"), None);
    // Before the opening quote of N'...' (between N and the quote).
    assert_eq!(at("SELECT * FROM Places WHERE City = N|'x'"), None);
}

#[test]
fn not_in_comments_or_other_strings() {
    assert_eq!(at("SELECT * FROM Places WHERE City = -- 'ma|"), None);
    assert_eq!(at("SELECT * FROM Places WHERE City = /* 'ma| */"), None);
    assert_eq!(at("SELECT * FROM Places /* WHERE City = 'ma| */"), None);
    assert_eq!(at("PRINT 'WHERE City = ma|'"), None);
    assert_eq!(at("EXEC ('SELECT * FROM Places WHERE City = ''ma|')"), None);
    assert_eq!(at("SELECT * FROM Places WHERE City = [ma|"), None, "an identifier, not a value");
    assert_eq!(at("SELECT * FROM Places WHERE Id = 12|"), None, "numbers are not suggested");
    // A string left open on an earlier line does not make the next lines values.
    assert_eq!(at("SELECT * FROM Places WHERE City = 'ma\nAND Code = x|"), None);
    // A closed comment before the value is skipped.
    assert_eq!(at("SELECT * FROM Places WHERE City = /* c */ 'ma|").map(|s| s.op), Some(Op::Eq));
}

#[test]
fn columns_resolve_through_aliases_and_qualifiers() {
    let t = target("SELECT * FROM dbo.Places c WHERE c.City = 'ma|");
    assert_eq!((t.db.as_str(), t.schema.as_str(), t.table.as_str(), t.column.name.as_str()), ("Shop", "dbo", "Places", "City"));
    assert_eq!(t.literal, Literal::Unicode);
    assert_eq!(target("SELECT * FROM Places WHERE City = 'ma|").table, "Places");
    assert_eq!(target("SELECT * FROM Places AS p WHERE [p].[city] = 'ma|").column.name, "City");
    assert_eq!(target("SELECT * FROM Places WHERE Places.Code = 'x|").literal, Literal::Ansi);
    assert_eq!(target("SELECT * FROM Places WHERE dbo.Places.State = 'x|").column.name, "State");
    assert_eq!(target("SELECT * FROM Places WHERE Shop.dbo.Places.City = 'x|").db, "Shop");
    assert_eq!(target("SELECT * FROM geo.Towns t, Places p WHERE Name = 'x|").table, "Towns");
    assert_eq!(target("SELECT * FROM geo.Towns t JOIN Places p ON p.City = t.Name WHERE t.Name IN ('a', '|").schema, "geo");
    assert_eq!(target("SELECT * FROM Places WHERE Login = 'sa|").literal, Literal::Unicode, "sysname is nvarchar");

    // Not columns whose values are offered.
    assert_eq!(resolved("SELECT * FROM Places c WHERE c.Nope = 'x|"), Resolved::No);
    assert_eq!(resolved("SELECT * FROM Places c WHERE x.City = 'x|"), Resolved::No, "unknown alias");
    assert_eq!(resolved("SELECT * FROM Places c WHERE c.Notes = 'x|"), Resolved::No, "nvarchar(max)");
    assert_eq!(resolved("SELECT * FROM Places c WHERE c.[Upper City] = 'x|"), Resolved::No, "not persisted");
    assert_eq!(resolved("SELECT * FROM Places c WHERE c.Body = 'x|"), Resolved::No, "text");
    assert_eq!(resolved("SELECT * FROM Places c WHERE c.Id = |"), Resolved::No, "int");
    assert_eq!(resolved("SELECT * FROM PlaceView v WHERE v.City = 'x|"), Resolved::No, "views are not read");
    assert_eq!(resolved("SELECT * FROM Other.dbo.T t WHERE t.City = 'x|"), Resolved::Missing(vec!["Other".into()]));
}

#[test]
fn like_escaping_and_literals() {
    assert_eq!(like_escape("100%_[a]"), "100[%][_][[]a]");
    assert_eq!(like_escape("plain"), "plain");
    assert_eq!(literal("O'Brien", Literal::Unicode), "N'O''Brien'");
    assert_eq!(literal("O'Brien", Literal::Ansi), "'O''Brien'");
}

#[test]
fn value_query_is_bounded_escaped_and_ranks_prefixes_first() {
    let t = target("SELECT * FROM dbo.Places c WHERE c.City = 'ma|");
    let q = value_query(&t, "m'a%_[", 200_000);
    assert_eq!(
        q,
        "SET LOCK_TIMEOUT 1000; SELECT TOP (25) s.v, COUNT_BIG(*) AS n \
         FROM (SELECT TOP (200000) [City] AS v FROM [Shop].[dbo].[Places]) AS s \
         WHERE UPPER(s.v) LIKE UPPER(N'%m''a[%][_][[]%') \
         GROUP BY s.v ORDER BY CASE WHEN UPPER(s.v) LIKE UPPER(N'm''a[%][_][[]%') THEN 0 ELSE 1 END, n DESC, s.v"
    );
    // varchar: no N prefix; the cap is the setting's.
    let t = target("SELECT * FROM Places WHERE Code = 'ab|");
    let q = value_query(&t, "ab", 50);
    assert!(q.contains("SELECT TOP (50) [Code] AS v FROM [Shop].[dbo].[Places]"), "{q}");
    assert!(q.contains("LIKE UPPER('%ab%')") && !q.contains("N'"), "{q}");
    // Nothing typed: every value (and NULL), most frequent first; never a 0-row cap.
    let q = value_query(&t, "", 0);
    assert!(q.contains("TOP (1) [Code]") && !q.contains("WHERE") && q.ends_with("ORDER BY n DESC, s.v"), "{q}");
    // Names with odd characters are bracketed.
    let mut t = t;
    t.table = "Odd]Table".into();
    t.column.name = "Odd Col".into();
    let q = value_query(&t, "x", 10);
    assert!(q.contains("[Odd Col] AS v FROM [Shop].[dbo].[Odd]]Table]"), "{q}");
    assert_eq!(
        size_query(&t),
        "SELECT CAST(ISNULL(SUM(p.rows), 0) AS bigint) FROM [Shop].sys.partitions p \
         WHERE p.object_id = OBJECT_ID(N'[Shop].[dbo].[Odd]]Table]') AND p.index_id IN (0, 1)"
    );
}

#[test]
fn ranking_prefix_then_count_then_name() {
    let values = vec![v("Wautoma", 40), v("Mauston", 20), v("Madison", 50), v("Oshkosh", 60), v("Mayville", 20), v("MARSHFIELD", 30)];
    let names: Vec<_> = rank(&values, "MA").into_iter().map(|v| v.value.unwrap()).collect();
    assert_eq!(names, ["Madison", "MARSHFIELD", "Mauston", "Mayville", "Wautoma"]);
    let with_null = vec![v("b", 1), Value { value: None, count: 5 }, v("a", 3)];
    assert_eq!(rank(&with_null, ""), vec![Value { value: None, count: 5 }, v("a", 3), v("b", 1)]);
    assert_eq!(rank(&with_null, "a"), vec![v("a", 3)], "NULL matches only nothing typed");
}

#[test]
fn items_insert_quoted_for_the_type() {
    let s = at("SELECT * FROM dbo.Places c WHERE c.City = 'ma|").unwrap();
    let t = target("SELECT * FROM dbo.Places c WHERE c.City = 'ma|");
    let it = items(&[v("O'Hare", 1204)], &t, &s, false);
    assert_eq!((it[0].name.as_str(), it[0].insert.as_str(), it[0].detail.as_str()), ("O'Hare", "N'O''Hare'", "1,204"));
    assert_eq!((it[0].kind, it[0].replace.clone()), (Kind::Value, None));
    assert_eq!(items(&[v("x", 1_234_567)], &t, &s, true)[0].detail, "~1,234,567");

    let s = at("SELECT * FROM Places WHERE Code = 'a|").unwrap();
    let t = target("SELECT * FROM Places WHERE Code = 'a|");
    assert_eq!(items(&[v("ab", 1)], &t, &s, false)[0].insert, "'ab'");

    // In a LIKE the value's wildcards are made literal.
    let s = at("SELECT * FROM Places WHERE Code LIKE '5|").unwrap();
    assert_eq!(items(&[v("50%_off", 1)], &t, &s, false)[0].insert, "'50[%][_]off'");

    // NULL: `IS NULL` replacing the operator, only for nullable columns after = / <>.
    let text = "SELECT * FROM Places WHERE City = '";
    let s = spot(text, text.len()).unwrap();
    let t = target("SELECT * FROM Places WHERE City = '|");
    let null = Value { value: None, count: 3 };
    let it = items(std::slice::from_ref(&null), &t, &s, false);
    assert_eq!((it[0].name.as_str(), it[0].insert.as_str()), ("IS NULL", "IS NULL"));
    assert_eq!(it[0].replace, Some(32..35), "from `=` to the caret");
    let s = at("SELECT * FROM Places WHERE City <> |").unwrap();
    assert_eq!(items(std::slice::from_ref(&null), &t, &s, false)[0].insert, "IS NOT NULL");
    let s = at("SELECT * FROM Places WHERE City IN (|").unwrap();
    assert!(items(std::slice::from_ref(&null), &t, &s, false).is_empty());
    let s = at("SELECT * FROM Places WHERE Code = |").unwrap();
    let t = target("SELECT * FROM Places WHERE Code = |");
    assert!(items(&[null], &t, &s, false).is_empty(), "Code is NOT NULL");
}

#[test]
fn counts_read_from_rows() {
    let r = vec![vec![Some("Madison".to_string()), Some("12".to_string())], vec![None, Some("3".to_string())]];
    assert_eq!(from_rows(&r), vec![v("Madison", 12), Value { value: None, count: 3 }]);
    assert_eq!(count_label(0, false), "0");
    assert_eq!(count_label(999, false), "999");
    assert_eq!(count_label(1000, false), "1,000");
}
