//! Fetching and caching for the experimental value suggestions (see
//! [`crate::completion::values`]).
//!
//! Lookups never run on the tab's session: each one opens its own short-lived session (in
//! `master`; the queries name the database), so they cannot disturb the tab's transaction or
//! `SET` options, and dropping a slow one leaves the tab alone. A lookup starts only after the
//! typing pauses ([`DEBOUNCE`]) or on Ctrl+Space, at most one runs at a time, and each query
//! gives up after [`TIMEOUT`] (and after `SET LOCK_TIMEOUT` when blocked).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use tokio::sync::oneshot::{self, error::TryRecvError};

use crate::completion::values::{self, LIMIT, Target, Value};
use crate::db::{ConnectionInfo, FetchResult, Repaint, Session};

pub(super) const DEBOUNCE: Duration = Duration::from_millis(300);
pub(super) const TIMEOUT: Duration = Duration::from_secs(5);
/// Typed texts remembered per column before the column's cache is cleared.
const PER_COLUMN: usize = 64;

/// The settings that drive value suggestions.
#[derive(Clone, Copy)]
pub(in crate::app) struct ValueOpts {
    pub on: bool,
    /// Rows read per lookup.
    pub scan_rows: u64,
    /// Larger tables only on Ctrl+Space.
    pub auto_max_table_rows: u64,
}

/// (connection, database, schema, table), lower-cased.
type TableKey = (String, String, String, String);
type ColKey = (TableKey, String);

struct Found {
    /// Lower-cased.
    typed: String,
    /// `None`: the lookup failed (timed out, blocked, no permission...).
    values: Option<Vec<Value>>,
}

#[derive(Clone, PartialEq)]
struct Want {
    key: ColKey,
    typed: String,
}

enum Stage {
    Size(oneshot::Receiver<FetchResult>),
    Values(oneshot::Receiver<FetchResult>),
}

struct Running {
    want: Want,
    target: Target,
    explicit: bool,
    cap: u64,
    auto_max: u64,
    stage: Stage,
    session: Session,
}

/// What to show for a value spot this frame.
#[derive(Default)]
pub(super) struct Offer {
    /// Ranked for the typed text.
    pub values: Vec<Value>,
    /// The table has more rows than were read, so counts are from a sample.
    pub approximate: bool,
    /// A lookup is waiting for the typing pause or running: repaint soon.
    pub busy: bool,
}

pub(super) struct ValueCache {
    rt: tokio::runtime::Handle,
    repaint: Repaint,
    /// Row counts from `sys.partitions`.
    sizes: HashMap<TableKey, u64>,
    found: HashMap<ColKey, Vec<Found>>,
    running: Option<Running>,
    /// The lookup wanted and since when (for the typing pause).
    wanted: Option<(Want, Instant)>,
}

fn table_key(conn: &str, t: &Target) -> TableKey {
    (conn.to_string(), t.db.to_lowercase(), t.schema.to_lowercase(), t.table.to_lowercase())
}

fn rows_of(r: FetchResult) -> anyhow::Result<Vec<Vec<crate::db::Cell>>> {
    r.map(|(_, rows)| rows)
}

impl ValueCache {
    pub(super) fn new(rt: tokio::runtime::Handle, repaint: Repaint) -> Self {
        Self { rt, repaint, sizes: HashMap::new(), found: HashMap::new(), running: None, wanted: None }
    }

    pub(super) fn forget_connection(&mut self, conn: &str) {
        self.sizes.retain(|k, _| k.0 != conn);
        self.found.retain(|k, _| k.0.0 != conn);
        if self.running.as_ref().is_some_and(|r| r.want.key.0.0 == conn) {
            self.running = None;
        }
    }

    /// The values for `typed` in `target`'s column: from the cache (an earlier search for a
    /// part of `typed` that returned everything it matched answers it too), otherwise after a
    /// lookup that starts once the typing pauses, or at once when `invoked` (Ctrl+Space this
    /// frame, which also retries a failed lookup). `explicit` (Ctrl+Space at this spot) also
    /// lifts the table size threshold.
    pub(super) fn offer(
        &mut self,
        info: &ConnectionInfo,
        target: &Target,
        typed: &str,
        explicit: bool,
        invoked: bool,
        opts: &ValueOpts,
    ) -> Offer {
        let tk = table_key(&super::conn_key(info), target);
        let key: ColKey = (tk.clone(), target.column.name.to_lowercase());
        let typed_l = typed.to_lowercase();
        let size = self.sizes.get(&tk).copied();
        let approximate = size.is_some_and(|n| n > opts.scan_rows);
        if !explicit && size.is_some_and(|n| n > opts.auto_max_table_rows) {
            return Offer::default();
        }
        if invoked && let Some(list) = self.found.get_mut(&key) {
            list.retain(|f| f.values.is_some());
        }
        let mut shown = Vec::new();
        if let Some(list) = self.found.get(&key) {
            if let Some(f) = list.iter().find(|f| f.typed == typed_l) {
                let values = f.values.as_deref().map(|v| values::rank(v, typed)).unwrap_or_default();
                return Offer { values, approximate, busy: false };
            }
            // The broadest earlier search this one narrows, preferring a complete one.
            let broader = list
                .iter()
                .filter(|f| f.values.is_some() && typed_l.contains(&f.typed))
                .max_by_key(|f| (f.values.as_ref().is_some_and(|v| v.len() < LIMIT), f.typed.len()));
            if let Some(f) = broader {
                let v = f.values.as_deref().unwrap_or_default();
                shown = values::rank(v, typed);
                if v.len() < LIMIT {
                    return Offer { values: shown, approximate, busy: false };
                }
            }
        }

        let want = Want { key, typed: typed_l };
        if self.running.as_ref().is_some_and(|r| r.want == want) {
            return Offer { values: shown, approximate, busy: true };
        }
        let now = Instant::now();
        let since = match &self.wanted {
            Some((w, t)) if *w == want => *t,
            _ => now,
        };
        self.wanted = Some((want.clone(), since));
        if invoked || now.duration_since(since) >= DEBOUNCE {
            self.wanted = None;
            // A newer lookup replaces a running one; dropping its session closes it.
            let session = Session::spawn(&self.rt, info.with_database("master"), self.repaint.clone());
            let stage = match size {
                Some(_) => Stage::Values(session.fetch_within(values::value_query(target, &want.typed, opts.scan_rows), TIMEOUT)),
                None => Stage::Size(session.fetch_within(values::size_query(target), TIMEOUT)),
            };
            self.running = Some(Running {
                want,
                target: target.clone(),
                explicit,
                cap: opts.scan_rows,
                auto_max: opts.auto_max_table_rows,
                stage,
                session,
            });
        }
        Offer { values: shown, approximate, busy: true }
    }

    fn store(&mut self, want: Want, values: Option<Vec<Value>>) {
        let list = self.found.entry(want.key).or_default();
        if list.len() >= PER_COLUMN {
            list.clear();
        }
        list.retain(|f| f.typed != want.typed);
        list.push(Found { typed: want.typed, values });
    }

    /// Collects a finished lookup step; called every frame.
    pub(super) fn poll(&mut self) {
        let Some(run) = &mut self.running else { return };
        let rx = match &mut run.stage {
            Stage::Size(rx) | Stage::Values(rx) => rx,
        };
        let result = match rx.try_recv() {
            Err(TryRecvError::Empty) => return,
            Ok(r) => rows_of(r),
            Err(TryRecvError::Closed) => Err(anyhow::anyhow!("the lookup connection closed")),
        };
        let Some(mut run) = self.running.take() else { return };
        match (&run.stage, result) {
            (Stage::Size(_), Ok(rows)) => {
                let n = rows.first().and_then(|r| r.first().cloned().flatten()).and_then(|n| n.parse().ok()).unwrap_or(0);
                self.sizes.insert(run.want.key.0.clone(), n);
                if run.explicit || n <= run.auto_max {
                    let q = values::value_query(&run.target, &run.want.typed, run.cap);
                    run.stage = Stage::Values(run.session.fetch_within(q, TIMEOUT));
                    self.running = Some(run);
                }
            }
            (Stage::Values(_), Ok(rows)) => self.store(run.want, Some(values::from_rows(&rows))),
            (_, Err(_)) => self.store(run.want, None),
        }
    }
}
