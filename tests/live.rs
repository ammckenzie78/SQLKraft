//! Query execution against a live server. See `tests/common/mod.rs` for setup; without
//! `SQLKRAFT_TEST_SERVER` these tests are skipped. Every statement here is read-only (temp
//! tables only).

mod common;

use std::time::{Duration, Instant};

use common::{Fixture, collect, finished};
use sqlkraft::db::{Event, MsgKind};
use sqlkraft::sql::split_batches;

/// Cancelling must keep the session: same SPID, temp tables, current database.
#[test]
fn cancel_keeps_the_session() {
    let mut f = Fixture::new(live_or_skip!());
    let db = f.db.clone();
    let setup = format!("USE [{db}];\nCREATE TABLE #keep (id int); INSERT #keep VALUES (7);");
    let ev = f.execute(&setup);
    assert!(matches!(ev.last(), Some(Event::ExecFinished { had_errors: false, .. })), "{ev:?}");

    let long_queries = [
        // Cancelled before the server sends anything.
        "WAITFOR DELAY '00:00:30'",
        // Cancelled while rows are streaming.
        "SELECT a.object_id, b.object_id FROM sys.all_objects a CROSS JOIN sys.all_objects b CROSS JOIN sys.all_objects c",
    ];
    for q in long_queries {
        f.session.execute(split_batches(q, 0));
        std::thread::sleep(Duration::from_millis(700));
        f.session.cancel();
        let ev = collect(&f.session, finished, Duration::from_secs(20));
        assert!(matches!(ev.last(), Some(Event::ExecFinished { cancelled: true, .. })), "{q}: {ev:?}");

        let after = f.execute("SELECT @@SPID, DB_NAME(), (SELECT id FROM #keep)");
        assert!(
            !after.iter().chain(ev.iter()).any(|e| matches!(e, Event::Disconnected(_) | Event::Connected(_))),
            "{q}: session was reset: {ev:?} {after:?}"
        );
        let row = after
            .iter()
            .find_map(|e| if let Event::Rows(r) = e { Some(r[0].clone()) } else { None })
            .unwrap_or_else(|| panic!("{q}: no rows after cancel: {after:?}"));
        assert_eq!(row[0].as_deref(), Some(f.spid.to_string().as_str()), "{q}: same SPID");
        assert_eq!(row[1].as_deref(), Some(db.as_str()), "{q}: same database");
        assert_eq!(row[2].as_deref(), Some("7"), "{q}: temp table survived");
    }
}

#[test]
fn end_to_end() {
    let mut f = Fixture::empty(live_or_skip!());

    let script = "\
SELECT 1 AS one, N'two' AS two, CAST(NULL AS int) AS n, CAST('2024-01-02T03:04:05.1234567' AS datetime2) AS dt,\n\
       CAST(12.50 AS decimal(9,2)) AS d, NEWID() AS g, 0xABCD AS b, CAST(1 AS bit) AS bt\n\
PRINT 'hello from print'\n\
GO\n\
CREATE TABLE #t (id int); INSERT #t VALUES (1),(2),(3); SELECT * FROM #t; DROP TABLE #t;\n\
GO\n\
SELECT * FROM dbo.this_table_does_not_exist\n\
GO 2\n\
USE tempdb\n\
GO\n\
SELECT DB_NAME() AS db\n";
    let ev = f.execute(script);

    let result_sets = ev.iter().filter(|e| matches!(e, Event::ResultSet(_))).count();
    assert_eq!(result_sets, 3, "expected 3 result sets");
    let msgs: Vec<_> = ev
        .iter()
        .filter_map(|e| if let Event::Message(m) = e { Some(m) } else { None })
        .collect();
    assert!(msgs.iter().any(|m| m.text == "hello from print"), "PRINT captured");
    assert!(msgs.iter().any(|m| m.text == "(3 rows affected)"), "row counts captured");
    let errors: Vec<_> = msgs.iter().filter(|m| m.kind == MsgKind::Error).collect();
    assert_eq!(errors.len(), 2, "GO 2 runs the failing batch twice");
    assert!(errors[0].text.starts_with("Msg 208, Level 16"), "{}", errors[0].text);
    assert!(errors[0].text.contains("Line 7"), "line is editor-relative: {}", errors[0].text);
    assert!(ev.iter().any(|e| matches!(e, Event::DatabaseChanged(d) if d == "tempdb")));
    let first_rows = ev.iter().find_map(|e| if let Event::Rows(r) = e { Some(r) } else { None }).unwrap();
    let r = &first_rows[0];
    assert_eq!(r[0].as_deref(), Some("1"));
    assert_eq!(r[1].as_deref(), Some("two"));
    assert_eq!(r[2], None);
    assert_eq!(r[3].as_deref(), Some("2024-01-02 03:04:05.1234567"));
    assert_eq!(r[4].as_deref(), Some("12.50"));
    assert_eq!(r[6].as_deref(), Some("0xABCD"));
    assert_eq!(r[7].as_deref(), Some("1"));
    assert!(matches!(ev.last(), Some(Event::ExecFinished { had_errors: true, cancelled: false, .. })));

    // Cancellation of a long-running query, then the connection must still work.
    f.session.execute(split_batches("WAITFOR DELAY '00:00:30'", 0));
    std::thread::sleep(Duration::from_millis(500));
    let t = Instant::now();
    f.session.cancel();
    let ev = collect(&f.session, finished, Duration::from_secs(15));
    assert!(t.elapsed() < Duration::from_secs(5), "cancel was quick");
    assert!(matches!(ev.last(), Some(Event::ExecFinished { cancelled: true, .. })));

    let ev = f.execute("SELECT 42");
    assert!(ev.iter().any(|e| matches!(e, Event::Rows(r) if r[0][0].as_deref() == Some("42"))), "usable after cancel");

    // Object Explorer style fetch.
    let rows = f.fetch("SELECT name FROM sys.databases ORDER BY name");
    assert!(rows.iter().any(|r| r[0].as_deref() == Some("master")));
}
