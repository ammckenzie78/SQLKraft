//! Live tests against a real SQL Server. Ignored by default; run with
//!
//! ```sh
//! SQLKRAFT_TEST_SERVER=localhost,1433 SQLKRAFT_TEST_USER=sa SQLKRAFT_TEST_PASSWORD=... \
//!     cargo test --test live -- --ignored --nocapture
//! ```
//! Leave SQLKRAFT_TEST_USER unset to use integrated authentication.
//! Every statement here is read-only (temp tables only).

use std::time::{Duration, Instant};

use sqlkraft::db::{AuthKind, ConnectionInfo, Event, MsgKind, Session};
use sqlkraft::sql::split_batches;

fn info() -> ConnectionInfo {
    let server = std::env::var("SQLKRAFT_TEST_SERVER").unwrap_or_else(|_| "localhost".into());
    let mut info = ConnectionInfo { server, ..Default::default() };
    match std::env::var("SQLKRAFT_TEST_USER") {
        Ok(u) => {
            info.user = u;
            info.password = std::env::var("SQLKRAFT_TEST_PASSWORD").unwrap_or_default();
        }
        Err(_) => info.auth = AuthKind::Integrated,
    }
    info
}

fn collect(session: &Session, until: impl Fn(&Event) -> bool, timeout: Duration) -> Vec<Event> {
    let start = Instant::now();
    let mut out = Vec::new();
    while start.elapsed() < timeout {
        match session.events.recv_timeout(Duration::from_millis(100)) {
            Ok(e) => {
                let stop = until(&e);
                println!("{e:?}");
                out.push(e);
                if stop {
                    break;
                }
            }
            Err(_) => continue,
        }
    }
    out
}

fn finished(e: &Event) -> bool {
    matches!(e, Event::ExecFinished { .. } | Event::ConnectFailed(_))
}

#[test]
#[ignore]
fn end_to_end() {
    sqlkraft::capture::install();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let mut s = Session::spawn(rt.handle(), info(), std::sync::Arc::new(|| {}));

    let ev = collect(&s, |e| matches!(e, Event::Connected(_) | Event::ConnectFailed(_)), Duration::from_secs(20));
    assert!(matches!(ev.last(), Some(Event::Connected(_))), "connect failed: {ev:?}");

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
    s.execute(split_batches(script, 0));
    let ev = collect(&s, finished, Duration::from_secs(30));

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
    s.execute(split_batches("WAITFOR DELAY '00:00:30'", 0));
    std::thread::sleep(Duration::from_millis(500));
    let t = Instant::now();
    s.cancel();
    let ev = collect(&s, finished, Duration::from_secs(15));
    assert!(t.elapsed() < Duration::from_secs(5), "cancel was quick");
    assert!(matches!(ev.last(), Some(Event::ExecFinished { cancelled: true, .. })));

    s.execute(split_batches("SELECT 42", 0));
    let ev = collect(&s, finished, Duration::from_secs(10));
    assert!(ev.iter().any(|e| matches!(e, Event::Rows(r) if r[0][0].as_deref() == Some("42"))), "usable after cancel");

    // Object Explorer style fetch.
    let rx = s.fetch("SELECT name FROM sys.databases ORDER BY name");
    let (_, rows) = rt.block_on(rx).unwrap().unwrap();
    assert!(rows.iter().any(|r| r[0].as_deref() == Some("master")));
}
