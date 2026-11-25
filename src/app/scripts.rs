//! Scripting jobs waiting on the server, turned into query tabs when they finish.

use tokio::sync::oneshot;

use super::SqlKraftApp;
use crate::db::{Cell, ConnectionInfo, FetchResult};
use crate::explorer::{self, NodeKind};
use crate::scripting::{self, ScriptMode, Syntax};

pub(super) enum ScriptJob {
    Definition { kind: NodeKind, rx: oneshot::Receiver<FetchResult>, mode: ScriptMode, syntax: Syntax },
    Table {
        kind: NodeKind,
        mode: ScriptMode,
        syntax: Syntax,
        cols: oneshot::Receiver<FetchResult>,
        keys: oneshot::Receiver<FetchResult>,
        cols_done: Option<Vec<Vec<Cell>>>,
        keys_done: Option<Vec<Vec<Cell>>>,
    },
    SelectTop { kind: NodeKind, rx: oneshot::Receiver<FetchResult>, run: bool },
}

pub(super) struct PendingScript {
    pub(super) info: ConnectionInfo,
    pub(super) job: ScriptJob,
}

impl SqlKraftApp {
    pub(super) fn pump_scripts(&mut self) {
        let mut done: Vec<(ConnectionInfo, String, bool)> = Vec::new();
        let mut errors = Vec::new();
        self.scripts.retain_mut(|p| {
            let finished = match &mut p.job {
                ScriptJob::Definition { kind, rx, mode, syntax } => match rx.try_recv() {
                    Ok(Ok((_, rows))) => {
                        let row = rows.first();
                        let def = row.and_then(|r| r.first().cloned().flatten());
                        let ty = row.and_then(|r| r.get(1).cloned().flatten()).unwrap_or_default();
                        match def {
                            Some(def) => done.push((
                                p.info.clone(),
                                scripting::module_script(kind, &def, &ty, *mode, *syntax),
                                false,
                            )),
                            None => errors.push("No definition available (encrypted, or no permission).".to_string()),
                        }
                        true
                    }
                    Ok(Err(e)) => {
                        errors.push(format!("{e:#}"));
                        true
                    }
                    Err(oneshot::error::TryRecvError::Empty) => false,
                    Err(_) => true,
                },
                ScriptJob::Table { kind, mode, syntax, cols, keys, cols_done, keys_done } => {
                    let mut failed = false;
                    if cols_done.is_none() {
                        match cols.try_recv() {
                            Ok(Ok((_, rows))) => *cols_done = Some(rows),
                            Ok(Err(e)) => {
                                errors.push(format!("{e:#}"));
                                failed = true;
                            }
                            Err(oneshot::error::TryRecvError::Empty) => {}
                            Err(_) => failed = true,
                        }
                    }
                    if keys_done.is_none() && !failed {
                        match keys.try_recv() {
                            Ok(Ok((_, rows))) => *keys_done = Some(rows),
                            Ok(Err(e)) => {
                                errors.push(format!("{e:#}"));
                                failed = true;
                            }
                            Err(oneshot::error::TryRecvError::Empty) => {}
                            Err(_) => failed = true,
                        }
                    }
                    if let (Some(c), Some(k)) = (cols_done.as_ref(), keys_done.as_ref()) {
                        let create = explorer::build_create_table(kind, c, k);
                        let script = match mode {
                            ScriptMode::DropAndCreate => {
                                scripting::drop_and_create_table(kind, &create, *syntax).unwrap_or(create)
                            }
                            _ => create,
                        };
                        done.push((p.info.clone(), script, false));
                        true
                    } else {
                        failed
                    }
                }
                ScriptJob::SelectTop { kind, rx, run } => match rx.try_recv() {
                    Ok(r) => {
                        let cols: Vec<String> = r
                            .map(|(_, rows)| rows.into_iter().filter_map(|r| r.into_iter().next().flatten()).collect())
                            .unwrap_or_default();
                        if let Some(sql) = explorer::select_top_script(kind, &cols) {
                            done.push((p.info.clone(), sql, *run));
                        }
                        true
                    }
                    Err(oneshot::error::TryRecvError::Empty) => false,
                    Err(_) => true,
                },
            };
            !finished
        });
        for e in errors {
            self.toast(format!("Scripting failed: {e}"));
        }
        for (info, text, run) in done {
            self.new_query(Some(info), text);
            if run {
                let idx = self.active;
                self.execute_tab(idx, true);
            }
        }
    }
}
