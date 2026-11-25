//! Shrink Database / Shrink Files against a live server (see `tests/common/mod.rs`). Each test
//! grows its own fixture database and shrinks it; nothing outside that database is touched.

mod common;

use common::Fixture;
use sqlkraft::db::Event;
use sqlkraft::maintenance::{self as m, DbFile, FileType, ShrinkFileAction};
use sqlkraft::sql::{quote_ident, quote_literal};

fn files(f: &Fixture) -> Vec<DbFile> {
    m::parse_files(&f.fetch(&m::files_query(&f.db)))
}

fn file_of(f: &Fixture, ty: FileType) -> DbFile {
    files(f).into_iter().find(|x| x.file_type == ty).expect("file of that type")
}

fn sensible(file: &DbFile) {
    let used = file.used_mb.expect("FILEPROPERTY SpaceUsed");
    assert!(used > 0.0 && used <= file.size_mb + 0.01, "used {used} of {} MB", file.size_mb);
    let pct = file.free_pct().unwrap();
    assert!((0.0..=100.0).contains(&pct), "{pct}");
}

fn grow(f: &Fixture, file: &DbFile, size_mb: u64) {
    f.exec(&format!(
        "ALTER DATABASE {} MODIFY FILE (NAME = {}, SIZE = {size_mb}MB)",
        quote_ident(&f.db),
        quote_literal(&file.name)
    ));
}

/// Runs a generated script the way the dialog's Run button does and checks it succeeded.
fn run(f: &mut Fixture, script: &str) {
    let ev = f.execute(script);
    assert!(matches!(ev.last(), Some(Event::ExecFinished { had_errors: false, cancelled: false, .. })), "{script}\n{ev:?}");
}

#[test]
fn shrink_data_file() {
    let mut f = Fixture::new(live_or_skip!());
    let db = f.db.clone();
    // Fill a table, then empty it, so there is used and then freed space; then grow the file.
    f.seed(
        &db,
        "CREATE TABLE dbo.Filler (id int IDENTITY PRIMARY KEY, pad char(8000) NOT NULL);\nGO\n\
         INSERT dbo.Filler (pad) SELECT TOP (600) 'x' FROM sys.all_objects;\nGO\n\
         DELETE dbo.Filler;\nGO\n",
    );
    let data = file_of(&f, FileType::Data);
    sensible(&data);
    let grown = (data.size_mb.ceil() as u64) + 40;
    grow(&f, &data, grown);
    let before = file_of(&f, FileType::Data);
    assert!(before.size_mb >= grown as f64 - 0.01, "{before:?}");
    sensible(&before);

    run(&mut f, &m::shrink_file_script(&db, &before.name, ShrinkFileAction::TruncateOnly, false));
    let after = file_of(&f, FileType::Data);
    assert!(after.size_mb < before.size_mb - 10.0, "TRUNCATEONLY released space: {before:?} -> {after:?}");
    sensible(&after);

    // Grow again, then reorganize and shrink to a target just above the space used.
    grow(&f, &after, grown);
    let before = file_of(&f, FileType::Data);
    let target = before.used_mb.unwrap().ceil() as u64 + 2;
    assert!(m::validate_target(&before, target).is_ok());
    run(&mut f, &m::shrink_file_script(&db, &before.name, ShrinkFileAction::Reorganize { target_mb: target }, false));
    let after = file_of(&f, FileType::Data);
    assert!(after.size_mb <= target as f64 + 1.0, "shrunk to about {target} MB: {after:?}");
    assert!(after.size_mb < before.size_mb, "{before:?} -> {after:?}");
    sensible(&after);

    // Shrink Database with a target percentage runs cleanly too.
    run(&mut f, &m::shrink_database_script(&db, 10, false, false));
    run(&mut f, &m::shrink_database_script(&db, 0, true, false));
}

#[test]
fn shrink_log_after_log_backup() {
    let mut f = Fixture::new(live_or_skip!());
    let db = f.db.clone();
    let q = quote_ident(&db);
    // On Windows, back up to the null device so no file is left behind. SQL Server on Linux
    // refuses /dev/null (OS error 31 resizing it), so there use a sqlkraft_test_ file in the
    // default backup folder, overwritten each time (WITH INIT, FORMAT).
    let windows = f.fetch("SELECT host_platform FROM sys.dm_os_host_info")[0][0].as_deref() == Some("Windows");
    let null = if windows {
        "NUL".to_string()
    } else {
        let dir = f.fetch("SELECT CAST(SERVERPROPERTY('InstanceDefaultBackupPath') AS nvarchar(4000))")[0][0]
            .clone()
            .unwrap_or_else(|| "/var/opt/mssql/data".into());
        format!("{}/{db}_log_test.bak", dir.trim_end_matches('/'))
    };
    let null = null.replace('\'', "''");
    f.exec(&format!("ALTER DATABASE {q} SET RECOVERY FULL"));
    f.exec(&format!("BACKUP DATABASE {q} TO DISK = N'{null}' WITH INIT, FORMAT"));

    let log = file_of(&f, FileType::Log);
    grow(&f, &log, (log.size_mb.ceil() as u64) + 32);
    f.seed(&db, "CREATE TABLE dbo.Churn (pad char(4000) NOT NULL);\nGO\nINSERT dbo.Churn SELECT TOP (500) 'x' FROM sys.all_objects;\nGO\nDELETE dbo.Churn;\nGO\n");
    let before = file_of(&f, FileType::Log);
    sensible(&before);

    let reuse = f.fetch(&m::log_reuse_query(&db));
    assert_eq!(reuse[0][1].as_deref(), Some("FULL"));
    let wait = reuse[0][0].clone().unwrap_or_default();
    assert!(!wait.is_empty(), "log_reuse_wait_desc is reported");
    // sys.dm_db_log_info needs VIEW SERVER STATE, which a dbcreator-only login lacks.
    match f.try_fetch(&m::vlf_count_query(&db)) {
        Ok(rows) => {
            let vlfs: i64 = rows[0][0].as_deref().unwrap().parse().unwrap();
            assert!(vlfs > 0, "VLF count");
        }
        Err(e) if m::is_permission_error(&format!("{e:#}")) => {
            eprintln!("note: VLF count not checked, the login lacks VIEW SERVER STATE: {e:#}")
        }
        Err(e) => panic!("VLF count failed: {e:#}"),
    }

    f.exec(&format!("BACKUP LOG {q} TO DISK = N'{null}' WITH INIT, FORMAT"));
    run(&mut f, &m::shrink_file_script(&db, &before.name, ShrinkFileAction::Reorganize { target_mb: 1 }, false));
    let after = file_of(&f, FileType::Log);
    assert!(after.size_mb < before.size_mb - 10.0, "log shrank after a log backup: {before:?} -> {after:?}");
}
