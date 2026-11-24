//! Captures server messages that tiberius does not surface through its public API.
//!
//! tiberius decodes INFO tokens (PRINT output, RAISERROR with severity < 11, "Changed database
//! context to ..." etc.) and DONE tokens (row counts) but only reports them through `tracing`
//! events. We install a tracing layer that forwards those events to whichever session is
//! currently being polled, found through a tokio task-local.

use std::fmt::Write as _;

use tokio::sync::mpsc::UnboundedSender;
use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

#[derive(Debug, Clone)]
pub enum Captured {
    /// Informational message (PRINT, low-severity RAISERROR, env changes).
    Info(String),
    /// A statement completed; `Some(n)` when the server reported a row count.
    Done(Option<u64>),
    /// The connection's current database changed (USE, or login).
    Database(String),
}

/// Environment-change notices that tiberius logs at DEBUG and that should not be shown.
const ENV_NOISE: &[&str] = &[
    "Packet size change from",
    "SQL collation change",
    "Begin transaction",
    "Commit transaction",
    "Rollback transaction",
    "Defect transaction",
    "Server requested routing",
    "Fallback mirror server",
    "Ignored env change",
];

tokio::task_local! {
    pub static SINK: UnboundedSender<Captured>;
}

pub struct CaptureLayer;

struct MessageVisitor(String);

impl Visit for MessageVisitor {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            let _ = write!(self.0, "{value:?}");
        }
    }
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.0.push_str(value);
        }
    }
}

impl<S: Subscriber> Layer<S> for CaptureLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        if !meta.target().starts_with("tiberius") {
            return;
        }
        let mut v = MessageVisitor(String::new());
        event.record(&mut v);
        let msg = v.0;
        let captured = if let Some(rest) = msg.strip_prefix("Done with status ") {
            parse_done(rest)
        } else if *meta.level() == tracing::Level::DEBUG && meta.target().contains("token") {
            if let Some(rest) = msg.strip_prefix("Database change from '") {
                rest.split("' to '")
                    .nth(1)
                    .map(|db| Captured::Database(db.trim_end_matches('\'').to_string()))
            } else if ENV_NOISE.iter().any(|p| msg.starts_with(p)) {
                None
            } else {
                Some(Captured::Info(msg))
            }
        } else {
            None
        };
        if let Some(c) = captured {
            let _ = SINK.try_with(|tx| tx.send(c));
        }
    }
}

/// Parses tiberius' `Display` for `TokenDone`, e.g.
/// `BitFlags<DoneStatus>(0b10000, Count) (3 rows left)`.
fn parse_done(rest: &str) -> Option<Captured> {
    if !rest.contains("Count") {
        return None;
    }
    let rows = rest
        .rfind(" (")
        .and_then(|i| rest[i + 2..].split_whitespace().next())
        .and_then(|n| n.parse::<u64>().ok())
        .unwrap_or(0);
    Some(Captured::Done(Some(rows)))
}

pub fn install() {
    use tracing_subscriber::prelude::*;
    let filter = tracing_subscriber::filter::filter_fn(|m| m.target().starts_with("tiberius"));
    let _ = tracing_subscriber::registry()
        .with(CaptureLayer.with_filter(filter))
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn done_parsing() {
        assert!(matches!(
            parse_done("BitFlags<DoneStatus>(0b10000, Count) (3 rows left)"),
            Some(Captured::Done(Some(3)))
        ));
        assert!(matches!(
            parse_done("BitFlags<DoneStatus>(0b10000, Count) (1 row left)"),
            Some(Captured::Done(Some(1)))
        ));
        assert!(matches!(
            parse_done("BitFlags<DoneStatus>(0b10000, Count)"),
            Some(Captured::Done(Some(0)))
        ));
        assert!(parse_done("BitFlags<DoneStatus>(0b1, More)").is_none());
    }
}
