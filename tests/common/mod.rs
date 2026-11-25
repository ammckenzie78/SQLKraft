//! Shared helpers for live tests against a real SQL Server.
//!
//! Configure with environment variables:
//!
//! - `SQLKRAFT_TEST_SERVER`: e.g. `localhost,1433`. When unset, live tests print a note and pass
//!   without doing anything, so `cargo test` works on machines without a server.
//! - `SQLKRAFT_TEST_USER` / `SQLKRAFT_TEST_PASSWORD`: a SQL login. Leave the user unset for
//!   integrated authentication. The login needs `dbcreator`: every test creates its own database
//!   (`sqlkraft_test_<uuid>`) and drops it afterwards, so tests and parallel runs never collide.
//!
//! tiberius only speaks TCP, so TCP must be enabled on the server.

#![allow(dead_code)]

use std::time::{Duration, Instant};

use sqlkraft::db::{AuthKind, Cell, ConnectionInfo, Event, Session};
use sqlkraft::explorer::{Children, Node, NodeKind};
use sqlkraft::sql::{quote_ident, quote_literal, split_batches};

/// Connection settings from the environment, or `None` to skip live tests.
pub fn config() -> Option<ConnectionInfo> {
    let server = std::env::var("SQLKRAFT_TEST_SERVER").ok().filter(|s| !s.trim().is_empty())?;
    let mut info = ConnectionInfo { server, ..Default::default() };
    match std::env::var("SQLKRAFT_TEST_USER") {
        Ok(u) if !u.is_empty() => {
            info.user = u;
            info.password = std::env::var("SQLKRAFT_TEST_PASSWORD").unwrap_or_default();
        }
        _ => info.auth = AuthKind::Integrated,
    }
    Some(info)
}

/// Returns the settings, or prints why the test is skipped.
#[macro_export]
macro_rules! live_or_skip {
    () => {
        match common::config() {
            Some(info) => info,
            None => {
                eprintln!("skipped: set SQLKRAFT_TEST_SERVER to run live tests");
                return;
            }
        }
    };
}

/// Reads events until `until` matches one or the timeout passes.
pub fn collect(session: &Session, until: impl Fn(&Event) -> bool, timeout: Duration) -> Vec<Event> {
    let start = Instant::now();
    let mut out = Vec::new();
    while start.elapsed() < timeout {
        if let Ok(e) = session.events.recv_timeout(Duration::from_millis(100)) {
            let stop = until(&e);
            out.push(e);
            if stop {
                break;
            }
        }
    }
    out
}

pub fn finished(e: &Event) -> bool {
    matches!(e, Event::ExecFinished { .. } | Event::ConnectFailed(_))
}

/// A connected session plus a throwaway database seeded with known objects.
///
/// The session starts in the login's default database (normally `master`), like the Object
/// Explorer connection, so queries that wrongly depend on the current database fail here.
pub struct Fixture {
    pub rt: tokio::runtime::Runtime,
    pub session: Session,
    pub info: ConnectionInfo,
    pub db: String,
    pub spid: i32,
    extra_dbs: Vec<String>,
}

impl Fixture {
    /// Connects, creates `sqlkraft_test_<uuid>` and seeds it with [`SEED`].
    pub fn new(info: ConnectionInfo) -> Self {
        let f = Self::empty(info);
        let db = f.db.clone();
        f.create_database(&db);
        f.seed(&db, SEED);
        f
    }

    /// Connects and picks a database name, without creating anything.
    pub fn empty(info: ConnectionInfo) -> Self {
        sqlkraft::capture::install();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let session = Session::spawn(rt.handle(), info.clone(), std::sync::Arc::new(|| {}));
        let ev = collect(&session, |e| matches!(e, Event::Connected(_) | Event::ConnectFailed(_)), Duration::from_secs(30));
        let spid = match ev.last() {
            Some(Event::Connected(facts)) => facts.spid,
            other => panic!("could not connect to the test server: {other:?}"),
        };
        Self { rt, session, info, db: unique_name(), spid, extra_dbs: Vec::new() }
    }

    /// Another empty database dropped with this fixture.
    pub fn extra_database(&mut self) -> String {
        let db = unique_name();
        self.create_database(&db);
        self.extra_dbs.push(db.clone());
        db
    }

    fn create_database(&self, db: &str) {
        self.exec(&format!("CREATE DATABASE {}", quote_ident(db)));
    }

    /// Runs each `GO`-separated batch of `script` inside `db` with `sp_executesql`, so the
    /// session's own current database is left alone.
    pub fn seed(&self, db: &str, script: &str) {
        for batch in split_batches(script, 0) {
            if batch.text.trim().is_empty() {
                continue;
            }
            self.exec(&format!("EXEC {}.sys.sp_executesql {}", quote_ident(db), quote_literal(&batch.text)));
        }
    }

    /// Runs one statement through the Object Explorer path and returns its first result set.
    pub fn fetch(&self, sql: &str) -> Vec<Vec<Cell>> {
        match self.try_fetch(sql) {
            Ok(rows) => rows,
            Err(e) => panic!("query failed: {e:#}\n{sql}"),
        }
    }

    pub fn try_fetch(&self, sql: &str) -> anyhow::Result<Vec<Vec<Cell>>> {
        let rx = self.session.fetch(sql);
        let r = self.rt.block_on(async { tokio::time::timeout(Duration::from_secs(60), rx).await });
        let reply = r.map_err(|_| anyhow::anyhow!("timed out"))?;
        let (_, rows) = reply.map_err(|_| anyhow::anyhow!("connection closed"))??;
        Ok(rows)
    }

    /// Like `fetch` but for statements without results.
    pub fn exec(&self, sql: &str) {
        self.fetch(sql);
    }

    /// Runs a script the way a query tab does (GO batches, messages, result sets).
    pub fn execute(&mut self, script: &str) -> Vec<Event> {
        self.session.execute(split_batches(script, 0));
        collect(&self.session, finished, Duration::from_secs(60))
    }

    /// Expands `node` and waits for its children.
    pub fn expand<'a>(&self, node: &'a mut Node) -> &'a mut Vec<Node> {
        node.ensure_loading(&self.session);
        let start = Instant::now();
        while matches!(node.children, Children::Loading(_)) {
            assert!(start.elapsed() < Duration::from_secs(30), "timed out expanding {}", node.label);
            std::thread::sleep(Duration::from_millis(20));
            node.poll();
        }
        match &mut node.children {
            Children::Loaded(c) => c,
            Children::Failed(e) => panic!("expanding {} failed: {e}", node.label),
            _ => unreachable!(),
        }
    }

    /// Follows `path` (child labels) from a fresh Server node and returns the last node's
    /// children.
    pub fn browse(&self, path: &[&str]) -> Vec<(String, NodeKind)> {
        let mut root = Node::new("server", NodeKind::Server);
        let mut node = &mut root;
        for step in path {
            let children = self.expand(node);
            let labels: Vec<_> = children.iter().map(|c| c.label.clone()).collect();
            node = children
                .iter_mut()
                .find(|c| c.label == *step)
                .unwrap_or_else(|| panic!("no '{step}' in {labels:?}"));
        }
        self.expand(node).iter().map(|c| (c.label.clone(), c.kind.clone())).collect()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let mut dbs = self.extra_dbs.clone();
        dbs.push(self.db.clone());
        for db in dbs {
            let q = quote_ident(&db);
            let lit = quote_literal(&db);
            let sql = format!(
                "USE master; IF DB_ID({lit}) IS NOT NULL BEGIN \
                 ALTER DATABASE {q} SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE {q}; END"
            );
            if let Err(e) = self.try_fetch(&sql) {
                eprintln!("warning: could not drop test database {db}: {e:#}");
            }
        }
    }
}

fn unique_name() -> String {
    format!("sqlkraft_test_{}", uuid::Uuid::new_v4().simple())
}

/// Objects every fixture database starts with. Names cover a non-dbo schema, spaces and `]`.
pub const SEED: &str = r#"
CREATE SCHEMA sales;
GO
CREATE TABLE dbo.Customers (
    Id int IDENTITY(1,1) NOT NULL CONSTRAINT PK_Customers PRIMARY KEY,
    Name nvarchar(100) NOT NULL,
    Email varchar(255) NULL CONSTRAINT UQ_Customers_Email UNIQUE,
    Balance decimal(18,2) NOT NULL CONSTRAINT DF_Customers_Balance DEFAULT (0),
    CreatedAt datetime2(3) NOT NULL CONSTRAINT DF_Customers_CreatedAt DEFAULT (sysutcdatetime()),
    Notes nvarchar(max) NULL,
    Shout AS (upper(Name))
);
GO
CREATE INDEX IX_Customers_Name ON dbo.Customers (Name);
GO
CREATE TABLE sales.[Order Lines] (
    OrderId uniqueidentifier NOT NULL,
    [Line]]No] int NOT NULL,
    Qty smallint NOT NULL,
    Price money NULL,
    Payload varbinary(max) NULL,
    CONSTRAINT PK_OrderLines PRIMARY KEY (OrderId, [Line]]No] DESC)
);
GO
INSERT dbo.Customers (Name, Email, Balance) VALUES (N'Ada', 'ada@example.com', 10.50), (N'Brian', NULL, 0);
GO
CREATE VIEW dbo.ActiveCustomers AS SELECT Id, Name FROM dbo.Customers WHERE Balance > 0;
GO
CREATE PROCEDURE dbo.GetCustomer @Id int AS SELECT Id, Name FROM dbo.Customers WHERE Id = @Id;
GO
CREATE PROCEDURE sales.[Odd]]Name Proc] AS SELECT 42 AS answer;
GO
CREATE FUNCTION dbo.Twice (@x int) RETURNS int AS BEGIN RETURN @x * 2 END;
GO
CREATE FUNCTION sales.LinesFor (@o uniqueidentifier) RETURNS TABLE AS
RETURN SELECT * FROM sales.[Order Lines] WHERE OrderId = @o;
GO
"#;
