//! Back Up / Restore against a live server (see `tests/common/mod.rs`). Backups go to the
//! server's default backup folder as `sqlkraft_test_*` files, which the fixture tries to delete;
//! restored databases are registered with the fixture and dropped with it.

mod common;

use common::Fixture;
use sqlkraft::backup::{self as b, BackupKind, BackupOptions, BackupSet, BackupType, Compression, RecoveryState, RestoreOptions, RestoreStep, ServerFolders};
use sqlkraft::db::{Event, MsgKind};
use sqlkraft::maintenance::is_permission_error;
use sqlkraft::sql::{quote_ident, quote_literal};

fn folders(f: &Fixture) -> ServerFolders {
    let row = f.fetch(b::SERVER_PATHS_QUERY).remove(0);
    let platform = f.try_fetch(b::HOST_PLATFORM_QUERY).ok().and_then(|r| r.first().and_then(|r| r[0].clone()));
    b::parse_server_folders(&row, platform.as_deref())
}

/// Runs a script like the dialog's Run button, returning the server's info messages.
fn run(f: &mut Fixture, script: &str) -> Vec<String> {
    let ev = f.execute(script);
    let errors: Vec<_> = ev.iter().filter_map(|e| if let Event::Message(m) = e { (m.kind == MsgKind::Error).then_some(&m.text) } else { None }).collect();
    assert!(matches!(ev.last(), Some(Event::ExecFinished { had_errors: false, cancelled: false, .. })), "{script}\n{errors:?}");
    ev.into_iter().filter_map(|e| if let Event::Message(m) = e { Some(m.text) } else { None }).collect()
}

fn backup_options(db: &str, kind: BackupType, files: Vec<String>) -> BackupOptions {
    BackupOptions { db: db.into(), kind, copy_only: false, files, compression: Compression::ServerDefault, checksum: true, init: true, verify: true, certificate: None }
}

fn headers(f: &Fixture, files: &[String]) -> Vec<BackupSet> {
    let (cols, rows) = f.fetch_with_columns(&b::headeronly_query(files)).expect("RESTORE HEADERONLY");
    b::parse_headers(&cols, &rows, files)
}

fn user_objects(f: &Fixture, db: &str) -> Vec<String> {
    let d = quote_ident(db);
    f.fetch(&format!(
        "SELECT s.name, o.name, o.type FROM {d}.sys.objects o JOIN {d}.sys.schemas s ON s.schema_id = o.schema_id \
         WHERE o.is_ms_shipped = 0 AND o.parent_object_id = 0 ORDER BY s.name, o.name"
    ))
    .into_iter()
    .map(|r| r.iter().map(|c| c.clone().unwrap_or_default()).collect::<Vec<_>>().join(" "))
    .collect()
}

#[test]
fn back_up_then_restore_under_a_new_name() {
    let mut f = Fixture::new(live_or_skip!());
    let db = f.db.clone();
    let folders = folders(&f);
    assert!(!folders.backup.is_empty(), "{folders:?}");
    let name = b::default_file_name(&db, BackupType::Full, chrono::Local::now().naive_local());
    let file = folders.style.join(&folders.backup, &name);
    f.register_file(&file);

    let msgs = run(&mut f, &b::backup_script(&backup_options(&db, BackupType::Full, vec![file.clone()])).unwrap());
    assert!(msgs.iter().any(|m| m.trim().ends_with("percent processed.")), "STATS progress arrives as messages: {msgs:?}");
    assert!(msgs.iter().any(|m| m.contains("is valid")), "VERIFYONLY ran: {msgs:?}");

    // The file browser's listing sees the new file (when the login may enumerate).
    match f.try_fetch(&b::enumerate_query(&folders.backup)) {
        Ok(rows) => {
            let entries = b::parse_enumerate(&rows);
            let e = entries.iter().find(|e| e.name == name).unwrap_or_else(|| panic!("{name} not listed"));
            assert!(!e.is_dir && e.size.unwrap_or(0) > 0 && e.modified.is_some(), "{e:?}");
            assert!(b::matches_filter(&e.name, b::DEFAULT_FILTER));
        }
        Err(e) if is_permission_error(&format!("{e:#}")) => eprintln!("note: listing not checked: {e:#}"),
        Err(e) => panic!("{e:#}"),
    }

    let sets = headers(&f, std::slice::from_ref(&file));
    assert_eq!(sets.len(), 1);
    let h = &sets[0];
    assert_eq!((h.kind(), h.position, h.database.as_str()), (BackupKind::Full, 1, db.as_str()));
    assert!(h.first_lsn > 0 && h.last_lsn >= h.first_lsn && h.checkpoint_lsn > 0, "{h:?}");
    assert!(h.size > 0 && !h.finish.is_empty(), "{h:?}");

    let (cols, rows) = f.fetch_with_columns(&b::filelistonly_query(std::slice::from_ref(&file), 1)).unwrap();
    let logical = b::parse_filelist(&cols, &rows);
    assert_eq!(logical.iter().map(|l| l.kind.as_str()).collect::<Vec<_>>(), ["D", "L"], "{logical:?}");

    let target = format!("{db}_restored");
    f.register_database(&target);
    let moves = b::default_moves(&logical, &db, &target, &folders);
    assert!(moves.iter().all(|(_, p)| p.contains(&target)), "{moves:?}");
    let opts = RestoreOptions {
        target: target.clone(),
        steps: vec![RestoreStep::from(h)],
        moves: moves.clone(),
        replace: false,
        recovery: RecoveryState::Recovery,
        stop_at: None,
        close_connections: true,
        keep_replication: false,
        checksum: true,
    };
    run(&mut f, &b::restore_script(&opts).unwrap());

    let state = f.fetch(&format!("SELECT state_desc, user_access_desc FROM sys.databases WHERE name = {}", quote_literal(&target)));
    assert_eq!(state[0][0].as_deref(), Some("ONLINE"));
    assert_eq!(state[0][1].as_deref(), Some("MULTI_USER"));
    assert_eq!(user_objects(&f, &target), user_objects(&f, &db), "same objects");
    let n = f.fetch(&format!("SELECT COUNT(*) FROM {}.dbo.Customers", quote_ident(&target)));
    assert_eq!(n[0][0].as_deref(), Some("2"));
    let mut physical: Vec<String> = f
        .fetch(&format!("SELECT physical_name FROM sys.master_files WHERE database_id = DB_ID({}) ORDER BY file_id", quote_literal(&target)))
        .into_iter()
        .map(|r| r[0].clone().unwrap_or_default().to_lowercase())
        .collect();
    let mut wanted: Vec<String> = moves.iter().map(|(_, p)| p.to_lowercase()).collect();
    physical.sort();
    wanted.sort();
    assert_eq!(physical, wanted, "files went WITH MOVE targets");
}

#[test]
fn restore_chain_from_history_with_a_striped_full() {
    let mut f = Fixture::new(live_or_skip!());
    let db = f.db.clone();
    let folders = folders(&f);
    let now = chrono::Local::now().naive_local();
    let path = |suffix: &str| folders.style.join(&folders.backup, &format!("{db}_{}_{suffix}", now.format("%H%M%S")));
    let full = vec![path("full_1.bak"), path("full_2.bak")];
    let diff = path("diff.dif");
    let log = path("log.trn");
    for p in full.iter().chain([&diff, &log]) {
        f.register_file(p);
    }
    f.exec(&format!("ALTER DATABASE {} SET RECOVERY FULL", quote_ident(&db)));
    run(&mut f, &b::backup_script(&backup_options(&db, BackupType::Full, full.clone())).unwrap());
    f.seed(&db, "INSERT dbo.Customers (Name, Email) VALUES (N'Cleo', 'cleo@example.com');");
    run(&mut f, &b::backup_script(&backup_options(&db, BackupType::Differential, vec![diff.clone()])).unwrap());
    f.seed(&db, "INSERT dbo.Customers (Name, Email) VALUES (N'Dev', 'dev@example.com');");
    run(&mut f, &b::backup_script(&backup_options(&db, BackupType::Log, vec![log.clone()])).unwrap());

    // A striped media set: HEADERONLY needs every file.
    assert_eq!(headers(&f, &full)[0].kind(), BackupKind::Full);

    let history = match f.try_fetch(&b::history_query(&db)) {
        Ok(rows) => b::parse_history(&db, &rows),
        Err(e) if is_permission_error(&format!("{e:#}")) => {
            eprintln!("note: msdb history not readable, chain built from file headers instead: {e:#}");
            [full.clone(), vec![diff.clone()], vec![log.clone()]].iter().flat_map(|m| headers(&f, m)).collect()
        }
        Err(e) => panic!("{e:#}"),
    };
    let chain = b::restore_chain(&history);
    let kinds: Vec<BackupKind> = chain.iter().map(|&i| history[i].kind()).collect();
    assert_eq!(kinds, [BackupKind::Full, BackupKind::Differential, BackupKind::Log], "{history:?}");
    let mut full_files = history[chain[0]].files.clone();
    full_files.sort();
    assert_eq!(full_files.len(), 2, "both stripes: {full_files:?}");

    let (cols, rows) = f.fetch_with_columns(&b::filelistonly_query(&history[chain[0]].files, history[chain[0]].position)).unwrap();
    let logical = b::parse_filelist(&cols, &rows);
    let target = format!("{db}_chain");
    f.register_database(&target);
    let opts = RestoreOptions {
        target: target.clone(),
        steps: chain.iter().map(|&i| RestoreStep::from(&history[i])).collect(),
        moves: b::default_moves(&logical, &db, &target, &folders),
        replace: false,
        recovery: RecoveryState::Recovery,
        stop_at: None,
        close_connections: false,
        keep_replication: false,
        checksum: true,
    };
    let script = b::restore_script(&opts).unwrap();
    assert_eq!(script.matches("NORECOVERY").count(), 2, "{script}");
    run(&mut f, &script);
    let names = f.fetch(&format!("SELECT Name FROM {}.dbo.Customers ORDER BY Id", quote_ident(&target)));
    let names: Vec<_> = names.iter().map(|r| r[0].clone().unwrap_or_default()).collect();
    assert_eq!(names, ["Ada", "Brian", "Cleo", "Dev"], "the diff and the log were applied");
}
