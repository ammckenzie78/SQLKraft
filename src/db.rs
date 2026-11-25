//! Connections and per-connection worker tasks.
//!
//! Every query window and every Object Explorer server owns its own `Session`: a tokio task that
//! holds one `tiberius::Client` and processes requests one at a time, just like SSMS keeps one
//! connection (one SPID) per query window.

use std::sync::Arc;
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context as _, anyhow, bail};
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use tiberius::{AuthMethod, Client, ColumnData, Config, EncryptionLevel, QueryItem, Row, SqlBrowser};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};
use tokio_util::sync::CancellationToken;

use crate::capture::{self, Captured};
use crate::sql::{Batch, quote_ident};

pub type SqlClient = Client<Compat<TcpStream>>;
pub type Repaint = Arc<dyn Fn() + Send + Sync>;

// ---------------------------------------------------------------------------------------------
// Connection settings
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum AuthKind {
    #[default]
    SqlServer,
    /// NTLM with explicit `DOMAIN\user` + password (works on Linux through sspi-rs).
    Windows,
    /// Current user's credentials (SSPI on Windows, Kerberos on Linux with `--features kerberos`).
    Integrated,
}

impl AuthKind {
    pub fn label(self) -> &'static str {
        match self {
            AuthKind::SqlServer => "SQL Server Authentication",
            AuthKind::Windows => "Windows Authentication (NTLM, user + password)",
            AuthKind::Integrated => "Integrated (current user / Kerberos)",
        }
    }
    pub fn available() -> Vec<AuthKind> {
        let mut v = vec![AuthKind::SqlServer, AuthKind::Windows];
        if cfg!(any(windows, feature = "kerberos")) {
            v.push(AuthKind::Integrated);
        }
        v
    }
    pub fn needs_password(self) -> bool {
        !matches!(self, AuthKind::Integrated)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Encrypt {
    /// Encrypt the whole session (SSMS "Mandatory").
    #[default]
    Mandatory,
    /// Encrypt only the login packet (SSMS "Optional").
    Optional,
    /// No encryption at all. Only for servers without TLS support.
    Off,
}

impl Encrypt {
    pub fn label(self) -> &'static str {
        match self {
            Encrypt::Mandatory => "Mandatory",
            Encrypt::Optional => "Optional (login only)",
            Encrypt::Off => "Off",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ConnectionInfo {
    pub server: String,
    pub auth: AuthKind,
    pub user: String,
    /// Never written to disk.
    #[serde(skip)]
    pub password: String,
    /// Empty = login's default database.
    pub database: String,
    pub encrypt: Encrypt,
    pub trust_server_certificate: bool,
    #[serde(default = "default_timeout")]
    pub connect_timeout_secs: u64,
}

fn default_timeout() -> u64 {
    15
}

impl Default for ConnectionInfo {
    fn default() -> Self {
        Self {
            server: "localhost".into(),
            auth: AuthKind::SqlServer,
            user: "sa".into(),
            password: String::new(),
            database: String::new(),
            encrypt: Encrypt::Mandatory,
            trust_server_certificate: true,
            connect_timeout_secs: 15,
        }
    }
}

impl ConnectionInfo {
    /// What SSMS shows in Object Explorer: `server (SQL Server 16.0.1000 - sa)`.
    pub fn display_user(&self) -> String {
        match self.auth {
            AuthKind::Integrated => whoami(),
            _ => self.user.clone(),
        }
    }

    /// Same login, different starting database.
    pub fn with_database(&self, db: &str) -> Self {
        let mut c = self.clone();
        c.database = db.to_string();
        c
    }

    fn to_config(&self) -> anyhow::Result<Config> {
        let mut config = Config::new();
        let target = parse_server(&self.server)?;
        config.host(&target.host);
        if let Some(port) = target.port {
            config.port(port);
        }
        if let Some(instance) = &target.instance {
            config.instance_name(instance);
        }
        if !self.database.is_empty() {
            config.database(&self.database);
        }
        config.application_name("SQLKraft");
        config.encryption(match self.encrypt {
            Encrypt::Mandatory => EncryptionLevel::Required,
            Encrypt::Optional => EncryptionLevel::Off,
            Encrypt::Off => EncryptionLevel::NotSupported,
        });
        if self.trust_server_certificate {
            config.trust_cert();
        }
        config.authentication(match self.auth {
            AuthKind::SqlServer => AuthMethod::sql_server(&self.user, &self.password),
            AuthKind::Windows => AuthMethod::windows(&self.user, &self.password),
            AuthKind::Integrated => integrated_auth()?,
        });
        Ok(config)
    }
}

#[cfg(any(windows, feature = "kerberos"))]
fn integrated_auth() -> anyhow::Result<AuthMethod> {
    Ok(AuthMethod::Integrated)
}

#[cfg(not(any(windows, feature = "kerberos")))]
fn integrated_auth() -> anyhow::Result<AuthMethod> {
    bail!("Integrated authentication needs a build with `--features kerberos`")
}

fn whoami() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "current user".into())
}

#[derive(Debug, PartialEq)]
pub struct ServerTarget {
    pub host: String,
    pub port: Option<u16>,
    pub instance: Option<String>,
}

/// Accepts the forms SSMS accepts: `host`, `host,port`, `host\INSTANCE`, `tcp:host,port`,
/// `.`, `(local)`.
pub fn parse_server(s: &str) -> anyhow::Result<ServerTarget> {
    let s = s.trim();
    let s = s.strip_prefix("tcp:").unwrap_or(s);
    if s.is_empty() {
        bail!("Server name is empty");
    }
    let (rest, port) = match s.rsplit_once(',') {
        Some((h, p)) => (
            h.trim(),
            Some(p.trim().parse::<u16>().with_context(|| format!("Invalid port '{p}'"))?),
        ),
        None => (s, None),
    };
    let (host, instance) = match rest.split_once('\\') {
        Some((h, i)) if !i.is_empty() => (h, Some(i.to_string())),
        Some((h, _)) => (h, None),
        None => (rest, None),
    };
    let host = match host {
        "." | "(local)" | "(localdb)" => "localhost",
        h => h,
    };
    Ok(ServerTarget { host: host.to_string(), port, instance })
}

pub async fn connect(info: &ConnectionInfo) -> anyhow::Result<SqlClient> {
    let timeout = Duration::from_secs(info.connect_timeout_secs.max(1));
    tokio::time::timeout(timeout, connect_inner(info))
        .await
        .map_err(|_| anyhow!("Connection timed out after {}s", timeout.as_secs()))?
}

async fn connect_inner(info: &ConnectionInfo) -> anyhow::Result<SqlClient> {
    let mut config = info.to_config()?;
    // Azure SQL gateways may redirect us; follow at most a couple of hops.
    for _ in 0..3 {
        let has_instance = parse_server(&info.server)?.instance.is_some();
        let tcp = if has_instance {
            TcpStream::connect_named(&config).await.context("SQL Browser lookup failed")?
        } else {
            TcpStream::connect(config.get_addr())
                .await
                .with_context(|| format!("Could not reach {}", config.get_addr()))?
        };
        tcp.set_nodelay(true)?;
        match Client::connect(config.clone(), tcp.compat_write()).await {
            Ok(client) => return Ok(client),
            Err(tiberius::error::Error::Routing { host, port }) => {
                config.host(&host);
                config.port(port);
            }
            Err(e) => return Err(format_error(&e, 0).into_anyhow()),
        }
    }
    bail!("Too many redirects while connecting")
}

// ---------------------------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ColumnMeta {
    pub name: String,
    pub type_name: String,
    pub numeric: bool,
}

pub type Cell = Option<String>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MsgKind {
    Info,
    Error,
}

#[derive(Debug, Clone)]
pub struct Message {
    pub kind: MsgKind,
    pub text: String,
}

impl Message {
    pub fn info(text: impl Into<String>) -> Self {
        Self { kind: MsgKind::Info, text: text.into() }
    }
    pub fn error(text: impl Into<String>) -> Self {
        Self { kind: MsgKind::Error, text: text.into() }
    }
    fn into_anyhow(self) -> anyhow::Error {
        anyhow!(self.text)
    }
}

#[derive(Debug, Clone)]
pub struct ServerFacts {
    pub database: String,
    pub version: String,
    pub product_version: String,
    pub login: String,
    pub spid: i32,
    /// `SERVERPROPERTY('EngineEdition')`: 2 Standard, 3 Enterprise/Developer, 4 Express,
    /// 5 Azure SQL Database, 8 Azure SQL Managed Instance, ...
    pub engine_edition: i32,
    /// `contained database authentication` is on, so partially contained databases are usable.
    pub contained_auth: bool,
}

#[derive(Debug)]
pub enum Event {
    Connected(ServerFacts),
    ConnectFailed(String),
    ExecStarted,
    ResultSet(Vec<ColumnMeta>),
    Rows(Vec<Vec<Cell>>),
    Message(Message),
    DatabaseChanged(String),
    ExecFinished { elapsed: Duration, cancelled: bool, had_errors: bool },
    Disconnected(String),
}

pub type FetchResult = anyhow::Result<(Vec<ColumnMeta>, Vec<Vec<Cell>>)>;

pub enum Request {
    Execute { batches: Vec<Batch>, cancel: CancellationToken },
    /// Run a query and hand back its first result set; used by Object Explorer & scripting.
    Fetch { sql: String, reply: oneshot::Sender<FetchResult> },
    UseDatabase(String),
    Close,
}

/// UI-side handle to a connection's worker task.
pub struct Session {
    tx: mpsc::UnboundedSender<Request>,
    pub events: std_mpsc::Receiver<Event>,
    pub info: ConnectionInfo,
    cancel: Option<CancellationToken>,
}

impl Session {
    pub fn spawn(rt: &tokio::runtime::Handle, info: ConnectionInfo, repaint: Repaint) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        let (etx, erx) = std_mpsc::channel();
        rt.spawn(worker(info.clone(), rx, etx, repaint));
        Self { tx, events: erx, info, cancel: None }
    }

    pub fn execute(&mut self, batches: Vec<Batch>) {
        let cancel = CancellationToken::new();
        self.cancel = Some(cancel.clone());
        let _ = self.tx.send(Request::Execute { batches, cancel });
    }

    pub fn cancel(&mut self) {
        if let Some(c) = self.cancel.take() {
            c.cancel();
        }
    }

    pub fn fetch(&self, sql: impl Into<String>) -> oneshot::Receiver<FetchResult> {
        let (reply, rx) = oneshot::channel();
        let _ = self.tx.send(Request::Fetch { sql: sql.into(), reply });
        rx
    }

    pub fn use_database(&self, db: &str) {
        let _ = self.tx.send(Request::UseDatabase(db.to_string()));
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.cancel();
        let _ = self.tx.send(Request::Close);
    }
}

struct Out {
    tx: std_mpsc::Sender<Event>,
    repaint: Repaint,
}

impl Out {
    fn send(&self, e: Event) {
        let _ = self.tx.send(e);
        (self.repaint)();
    }
}

async fn worker(
    info: ConnectionInfo,
    mut rx: mpsc::UnboundedReceiver<Request>,
    tx: std_mpsc::Sender<Event>,
    repaint: Repaint,
) {
    let out = Out { tx, repaint };
    let mut client = match open(&info, &out).await {
        Some(c) => Some(c),
        None => return,
    };

    while let Some(req) = rx.recv().await {
        if client.is_none() {
            // The previous connection broke; transparently reconnect like SSMS does.
            client = open(&info, &out).await;
        }
        let Some(c) = client.as_mut() else {
            if let Request::Fetch { reply, .. } = req {
                let _ = reply.send(Err(anyhow!("Not connected")));
            }
            continue;
        };
        let healthy = match req {
            Request::Execute { batches, cancel } => run_batches(c, &batches, &cancel, &out).await,
            Request::Fetch { sql, reply } => {
                let r = fetch(c, &sql).await;
                let broken = r.as_ref().err().is_some_and(is_io_error);
                let _ = reply.send(r);
                !broken
            }
            Request::UseDatabase(db) => {
                let (mtx, mut mrx) = mpsc::unbounded_channel();
                let r = capture::SINK
                    .scope(mtx, async {
                        let sql = format!("USE {}", quote_ident(&db));
                        c.simple_query(sql).await?.into_results().await
                    })
                    .await;
                while let Ok(m) = mrx.try_recv() {
                    if let Captured::Database(d) = m {
                        out.send(Event::DatabaseChanged(d));
                    }
                }
                match r {
                    Ok(_) => true,
                    Err(e) => {
                        let io = matches!(e, tiberius::error::Error::Io { .. });
                        out.send(Event::Message(format_error(&e, 0)));
                        !io
                    }
                }
            }
            Request::Close => break,
        };
        if !healthy {
            client = None;
            out.send(Event::Disconnected("Connection lost; it will be reopened on the next request.".into()));
        }
    }
}

/// Whether the instance allows contained databases to authenticate their own users. Azure SQL
/// Database (engine edition 5) always does. Errors (permissions, older versions) read as "no".
async fn contained_auth(client: &mut SqlClient) -> bool {
    let sql = "SELECT CASE WHEN CAST(SERVERPROPERTY('EngineEdition') AS int) = 5 THEN 1                ELSE (SELECT CAST(value_in_use AS int) FROM sys.configurations                      WHERE name = N'contained database authentication') END";
    matches!(fetch(client, sql).await, Ok((_, rows))
        if rows.first().and_then(|r| r.first().cloned().flatten()).as_deref() == Some("1"))
}

async fn open(info: &ConnectionInfo, out: &Out) -> Option<SqlClient> {
    let mut client = match connect(info).await {
        Ok(c) => c,
        Err(e) => {
            out.send(Event::ConnectFailed(format!("{e:#}")));
            return None;
        }
    };
    let facts = fetch(
        &mut client,
        "SELECT DB_NAME(), @@VERSION, CAST(SERVERPROPERTY('ProductVersion') AS nvarchar(128)), \
         SUSER_SNAME(), CAST(@@SPID AS int), CAST(SERVERPROPERTY('EngineEdition') AS int)",
    )
    .await;
    match facts {
        Ok((_, rows)) if !rows.is_empty() => {
            let r = &rows[0];
            let s = |i: usize| r.get(i).cloned().flatten().unwrap_or_default();
            let contained_auth = contained_auth(&mut client).await;
            out.send(Event::Connected(ServerFacts {
                database: s(0),
                version: s(1),
                product_version: s(2),
                login: s(3),
                spid: s(4).parse().unwrap_or(0),
                engine_edition: s(5).parse().unwrap_or(0),
                contained_auth,
            }));
            Some(client)
        }
        Ok(_) => {
            out.send(Event::ConnectFailed("Server returned no session information".into()));
            None
        }
        Err(e) => {
            out.send(Event::ConnectFailed(format!("{e:#}")));
            None
        }
    }
}

fn is_io_error(e: &anyhow::Error) -> bool {
    matches!(e.downcast_ref::<tiberius::error::Error>(), Some(tiberius::error::Error::Io { .. }))
}

async fn fetch(client: &mut SqlClient, sql: &str) -> FetchResult {
    let mut stream = client.simple_query(sql).await?;
    let mut cols = Vec::new();
    let mut rows = Vec::new();
    let mut first = true;
    while let Some(item) = stream.try_next().await? {
        match item {
            QueryItem::Metadata(m) => {
                if first {
                    cols = m.columns().iter().map(column_meta).collect();
                }
                first = false;
            }
            QueryItem::Row(r) if r.result_index() == 0 => rows.push(convert_row(&r)),
            QueryItem::Row(_) => {}
        }
    }
    Ok((cols, rows))
}

/// Runs the batches of one execution. Returns false if the connection is no longer usable.
async fn run_batches(
    client: &mut SqlClient,
    batches: &[Batch],
    cancel: &CancellationToken,
    out: &Out,
) -> bool {
    out.send(Event::ExecStarted);
    let started = Instant::now();
    let mut had_errors = false;
    let mut cancelled = false;
    let mut healthy = true;

    'batches: for batch in batches {
        for _ in 0..batch.repeat.max(1) {
            if cancel.is_cancelled() {
                cancelled = true;
                break 'batches;
            }
            let (mtx, mrx) = mpsc::unbounded_channel();
            let outcome = capture::SINK
                .scope(mtx, run_one(client, batch, cancel, out, mrx))
                .await;
            had_errors |= outcome.error;
            if outcome.cancelled {
                cancelled = true;
                // Tell the server to stop, and wait for its acknowledgement so the connection
                // is back in sync.
                let ack = tokio::time::timeout(Duration::from_secs(10), client.cancel_query()).await;
                let lost = match ack {
                    Ok(Ok(())) => None,
                    Ok(Err(e)) => Some(e.to_string()),
                    Err(_) => Some("the server did not acknowledge the cancel within 10 seconds".to_string()),
                };
                out.send(Event::Message(Message::error("Query was cancelled by user.")));
                if let Some(why) = lost {
                    healthy = false;
                    out.send(Event::Message(Message::error(format!(
                        "The connection had to be reset after the cancel ({why}). Temp tables, \
                         the current database, SET options and open transactions were lost."
                    ))));
                }
                break 'batches;
            }
            if outcome.broken {
                healthy = false;
                break 'batches;
            }
        }
    }

    out.send(Event::Message(Message::info(format!(
        "\nCompletion time: {}",
        chrono::Local::now().format("%Y-%m-%dT%H:%M:%S%.3f%:z")
    ))));
    out.send(Event::ExecFinished { elapsed: started.elapsed(), cancelled, had_errors });
    healthy
}

/// How long a cancel lets a read that is part-way through a token finish. The rest of the
/// token is already on its way, so this is only reached if the server or network stalls.
const FINISH_TOKEN: Duration = Duration::from_secs(30);

/// After a cancel: when the in-flight read is part-way through a token (`mid_token`), let it
/// finish, so the bytes it consumed aren't lost and the stream stays in step for the cancel
/// acknowledgement. Otherwise dropping it is safe and immediate (WAITFOR or a blocked query
/// costs no extra wait). Returns false if the token never completed.
async fn finish_token<F: std::future::Future>(read: std::pin::Pin<&mut F>, mid_token: &std::sync::atomic::AtomicBool) -> bool {
    if !mid_token.load(std::sync::atomic::Ordering::Acquire) {
        return true;
    }
    tokio::time::timeout(FINISH_TOKEN, read).await.is_ok()
}

struct Outcome {
    error: bool,
    cancelled: bool,
    broken: bool,
}

async fn run_one(
    client: &mut SqlClient,
    batch: &Batch,
    cancel: &CancellationToken,
    out: &Out,
    mut mrx: mpsc::UnboundedReceiver<Captured>,
) -> Outcome {
    let mut outcome = Outcome { error: false, cancelled: false, broken: false };
    let drain = |mrx: &mut mpsc::UnboundedReceiver<Captured>| {
        while let Ok(m) = mrx.try_recv() {
            match m {
                Captured::Info(t) => out.send(Event::Message(Message::info(t))),
                Captured::Done(Some(n)) => out.send(Event::Message(Message::info(format!(
                    "({n} row{} affected)",
                    if n == 1 { "" } else { "s" }
                )))),
                Captured::Done(None) => {}
                Captured::Database(d) => out.send(Event::DatabaseChanged(d)),
            }
        }
    };

    if cancel.is_cancelled() {
        outcome.cancelled = true;
        return outcome;
    }
    // Never race the send itself: a half-sent request poisons the connection. Sending is quick;
    // the wait for the response is what a cancel must be able to interrupt.
    let mid_token = client.mid_token_flag();
    if let Err(e) = client.send_batch(batch.text.as_str()).await {
        drain(&mut mrx);
        outcome.error = true;
        outcome.broken = true;
        out.send(Event::Message(format_error(&e, batch.start_line)));
        return outcome;
    }
    let started = {
        let response = client.batch_results();
        tokio::pin!(response);
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                if !finish_token(response.as_mut(), &mid_token).await {
                    outcome.broken = true;
                }
                None
            }
            r = &mut response => Some(r),
        }
    };
    let mut stream = match started {
        None => {
            outcome.cancelled = true;
            return outcome;
        }
        Some(Err(e)) => {
            drain(&mut mrx);
            outcome.error = true;
            outcome.broken = matches!(e, tiberius::error::Error::Io { .. });
            out.send(Event::Message(format_error(&e, batch.start_line)));
            return outcome;
        }
        Some(Ok(s)) => s,
    };

    let mut buf: Vec<Vec<Cell>> = Vec::new();
    let mut last_flush = Instant::now();
    let flush = |buf: &mut Vec<Vec<Cell>>| {
        if !buf.is_empty() {
            out.send(Event::Rows(std::mem::take(buf)));
        }
    };

    loop {
        let item = {
            let next = stream.try_next();
            tokio::pin!(next);
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    if !finish_token(next.as_mut(), &mid_token).await {
                        outcome.broken = true;
                    }
                    outcome.cancelled = true;
                    break;
                }
                it = &mut next => it,
            }
        };
        match item {
            Ok(Some(QueryItem::Metadata(m))) => {
                flush(&mut buf);
                drain(&mut mrx);
                out.send(Event::ResultSet(m.columns().iter().map(column_meta).collect()));
            }
            Ok(Some(QueryItem::Row(r))) => {
                buf.push(convert_row(&r));
                if buf.len() >= 2000 || last_flush.elapsed() > Duration::from_millis(100) {
                    flush(&mut buf);
                    drain(&mut mrx);
                    last_flush = Instant::now();
                }
            }
            Ok(None) => break,
            Err(e) => {
                flush(&mut buf);
                drain(&mut mrx);
                outcome.error = true;
                outcome.broken = matches!(e, tiberius::error::Error::Io { .. });
                out.send(Event::Message(format_error(&e, batch.start_line)));
                break;
            }
        }
    }
    flush(&mut buf);
    drop(stream);
    drain(&mut mrx);
    outcome
}

/// Formats an error the way SSMS does, with the line number relative to the editor.
pub fn format_error(e: &tiberius::error::Error, line_offset: usize) -> Message {
    match e {
        tiberius::error::Error::Server(t) => {
            let line = t.line() as usize + line_offset;
            let mut head = format!("Msg {}, Level {}, State {}", t.code(), t.class(), t.state());
            if !t.procedure().is_empty() {
                head.push_str(&format!(", Procedure {}", t.procedure()));
            }
            head.push_str(&format!(", Line {line}"));
            Message::error(format!("{head}\n{}", t.message()))
        }
        other => Message::error(other.to_string()),
    }
}

fn column_meta(c: &tiberius::Column) -> ColumnMeta {
    let type_name = format!("{:?}", c.column_type());
    let numeric = ["Int", "Float", "Decimal", "Numeric", "Money"]
        .iter()
        .any(|k| type_name.contains(k));
    ColumnMeta { name: c.name().to_string(), type_name, numeric }
}

fn convert_row(row: &Row) -> Vec<Cell> {
    row.cells().enumerate().map(|(i, (_, data))| cell_text(row, i, data)).collect()
}

fn cell_text(row: &Row, i: usize, data: &ColumnData<'static>) -> Cell {
    use chrono::{DateTime, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime};
    fn opt<T: ToString>(v: &Option<T>) -> Cell {
        v.as_ref().map(|x| x.to_string())
    }
    match data {
        ColumnData::U8(v) => opt(v),
        ColumnData::I16(v) => opt(v),
        ColumnData::I32(v) => opt(v),
        ColumnData::I64(v) => opt(v),
        ColumnData::F32(v) => opt(v),
        ColumnData::F64(v) => opt(v),
        ColumnData::Bit(v) => v.map(|b| if b { "1".into() } else { "0".into() }),
        ColumnData::String(v) => v.as_ref().map(|s| s.to_string()),
        ColumnData::Guid(v) => v.map(|g| g.to_string().to_uppercase()),
        ColumnData::Binary(v) => v.as_ref().map(|b| {
            let mut s = String::with_capacity(2 + b.len() * 2);
            s.push_str("0x");
            for byte in b.iter() {
                s.push_str(&format!("{byte:02X}"));
            }
            s
        }),
        ColumnData::Numeric(v) => opt(v),
        ColumnData::Xml(v) => v.as_ref().map(|x| x.to_string()),
        ColumnData::DateTime(None)
        | ColumnData::SmallDateTime(None)
        | ColumnData::DateTime2(None)
        | ColumnData::Date(None)
        | ColumnData::Time(None)
        | ColumnData::DateTimeOffset(None) => None,
        ColumnData::DateTime(_) => fmt_temporal::<NaiveDateTime>(row, i, "%Y-%m-%d %H:%M:%S%.3f", None),
        ColumnData::SmallDateTime(_) => fmt_temporal::<NaiveDateTime>(row, i, "%Y-%m-%d %H:%M:%S", None),
        ColumnData::DateTime2(Some(v)) => {
            fmt_temporal::<NaiveDateTime>(row, i, "%Y-%m-%d %H:%M:%S", Some(v.time().scale()))
        }
        ColumnData::Date(_) => fmt_temporal::<NaiveDate>(row, i, "%Y-%m-%d", None),
        ColumnData::Time(Some(v)) => fmt_temporal::<NaiveTime>(row, i, "%H:%M:%S", Some(v.scale())),
        ColumnData::DateTimeOffset(Some(v)) => fmt_temporal::<DateTime<FixedOffset>>(
            row,
            i,
            "%Y-%m-%d %H:%M:%S",
            Some(v.datetime2().time().scale()),
        )
        .map(|s| {
            // Offset goes after the fractional seconds, as SSMS shows it.
            match row.try_get::<DateTime<FixedOffset>, usize>(i) {
                Ok(Some(d)) => format!("{s} {}", d.format("%:z")),
                _ => s,
            }
        }),
    }
}

trait Temporal {
    fn fmt_with(&self, f: &str) -> String;
    fn nanos(&self) -> u32;
}
macro_rules! temporal {
    ($($t:ty),*) => {$(
        impl Temporal for $t {
            fn fmt_with(&self, f: &str) -> String { self.format(f).to_string() }
            fn nanos(&self) -> u32 { self.format("%9f").to_string().parse().unwrap_or(0) }
        }
    )*};
}
temporal!(
    chrono::NaiveDateTime,
    chrono::NaiveDate,
    chrono::NaiveTime,
    chrono::DateTime<chrono::FixedOffset>
);

/// Formats a temporal cell; `scale` appends that many fractional-second digits (0-7), the way
/// SSMS shows datetime2(n) / time(n) values.
fn fmt_temporal<'a, T>(row: &'a Row, i: usize, f: &str, scale: Option<u8>) -> Cell
where
    T: tiberius::FromSql<'a> + Temporal,
{
    match row.try_get::<T, usize>(i) {
        Ok(Some(v)) => {
            let mut s = v.fmt_with(f);
            if let Some(scale) = scale.filter(|s| *s > 0) {
                let frac = format!("{:09}", v.nanos());
                s.push('.');
                s.push_str(&frac[..(scale as usize).min(9)]);
            }
            Some(s)
        }
        Ok(None) => None,
        Err(e) => Some(format!("<{e}>")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_names() {
        assert_eq!(
            parse_server("db1").unwrap(),
            ServerTarget { host: "db1".into(), port: None, instance: None }
        );
        assert_eq!(
            parse_server("tcp:db1.example.com,14330").unwrap(),
            ServerTarget { host: "db1.example.com".into(), port: Some(14330), instance: None }
        );
        assert_eq!(
            parse_server(r".\SQLEXPRESS").unwrap(),
            ServerTarget { host: "localhost".into(), port: None, instance: Some("SQLEXPRESS".into()) }
        );
        assert!(parse_server("host,notaport").is_err());
        assert!(parse_server("  ").is_err());
    }
}
