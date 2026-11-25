use super::*;

fn dt() -> chrono::NaiveDateTime {
    chrono::NaiveDate::from_ymd_opt(2026, 11, 25).unwrap().and_hms_opt(13, 4, 5).unwrap()
}

#[test]
fn windows_paths() {
    let w = PathStyle::Windows;
    assert_eq!(w.join(r"D:\SQL\Backup", "a.bak"), r"D:\SQL\Backup\a.bak");
    assert_eq!(w.join(r"D:\SQL\Backup\", "a.bak"), r"D:\SQL\Backup\a.bak");
    assert_eq!(w.join("D:", "a.bak"), r"D:\a.bak");
    assert_eq!(w.normalize_dir(r"D:\SQL\DATA\"), r"D:\SQL\DATA");
    assert_eq!(w.normalize_dir("d:"), r"d:\");
    assert_eq!(w.parent(r"D:\SQL\Backup"), Some(r"D:\SQL".into()));
    assert_eq!(w.parent(r"D:\SQL"), Some(r"D:\".into()));
    assert_eq!(w.parent(r"D:\"), None);
    assert!(w.is_root(r"C:\") && w.is_root("C:") && !w.is_root(r"C:\x"));
    assert_eq!(w.file_name(r"D:\SQL\db_1.bak"), "db_1.bak");
    assert_eq!(
        w.breadcrumbs(r"D:\SQL\Backup"),
        vec![("D:\\".into(), "D:\\".into()), ("SQL".into(), r"D:\SQL".into()), ("Backup".into(), r"D:\SQL\Backup".into())]
    );
    // UNC shares: the share is the root.
    assert_eq!(w.parent(r"\\nas\backups\sql"), Some(r"\\nas\backups\".into()));
    assert_eq!(w.parent(r"\\nas\backups\"), None);
    assert_eq!(w.join(r"\\nas\backups", "x.bak"), r"\\nas\backups\x.bak");
}

#[test]
fn posix_paths() {
    let p = PathStyle::Posix;
    assert_eq!(p.join("/var/opt/mssql/data/", "a.bak"), "/var/opt/mssql/data/a.bak");
    assert_eq!(p.join("/", "a.bak"), "/a.bak");
    assert_eq!(p.parent("/var/opt"), Some("/var".into()));
    assert_eq!(p.parent("/var"), Some("/".into()));
    assert_eq!(p.parent("/"), None);
    assert_eq!(p.file_name("/var/opt/mssql/data/x.mdf"), "x.mdf");
    // Backslashes are ordinary characters on Linux.
    assert_eq!(p.file_name(r"/tmp/a\b"), r"a\b");
    assert_eq!(p.breadcrumbs("/var/opt").iter().map(|b| b.1.as_str()).collect::<Vec<_>>(), ["/", "/var", "/var/opt"]);
    assert_eq!(PathStyle::from_platform("Linux"), PathStyle::Posix);
    assert_eq!(PathStyle::from_platform("Windows"), PathStyle::Windows);
    assert_eq!(PathStyle::guess("/var/opt/mssql/data/master.mdf"), PathStyle::Posix);
    assert_eq!(PathStyle::guess(r"C:\Program Files\x.mdf"), PathStyle::Windows);
}

#[test]
fn filters() {
    assert!(matches_filter("DB.BAK", DEFAULT_FILTER));
    assert!(matches_filter("log_1.trn", DEFAULT_FILTER));
    assert!(!matches_filter("db.mdf", DEFAULT_FILTER));
    assert!(matches_filter("anything", ""));
    assert!(matches_filter("a1.bak", "a?.bak"));
    assert!(!matches_filter("a12.bak", "a?.bak"));
}

#[test]
fn server_folders_with_fallbacks() {
    let row = |b: Option<&str>, d: Option<&str>, l: Option<&str>, m: &str| {
        vec![b.map(String::from), d.map(String::from), l.map(String::from), Some(m.to_string())]
    };
    let f = parse_server_folders(&row(Some(r"D:\B"), Some(r"D:\D\"), Some(r"D:\L\"), r"D:\D\master.mdf"), Some("Windows"));
    assert_eq!(f, ServerFolders { backup: r"D:\B".into(), data: r"D:\D".into(), log: r"D:\L".into(), style: PathStyle::Windows });
    // SQL Server 2016 on Linux-less Windows: no backup path, no dm_os_host_info.
    let f = parse_server_folders(&row(None, None, None, r"C:\SQL\DATA\master.mdf"), None);
    assert_eq!((f.backup.as_str(), f.data.as_str(), f.style), (r"C:\SQL\DATA", r"C:\SQL\DATA", PathStyle::Windows));
    let f = parse_server_folders(&row(Some("/var/opt/mssql/data"), None, None, "/var/opt/mssql/data/master.mdf"), Some("Linux"));
    assert_eq!(f.style, PathStyle::Posix);
    assert_eq!(f.log, "/var/opt/mssql/data");
}

#[test]
fn listing_queries() {
    assert_eq!(
        enumerate_query("D:\\it's"),
        "SELECT file_or_directory_name, CAST(is_directory AS int), size_in_bytes, \
         CONVERT(varchar(19), last_write_time, 120) FROM sys.dm_os_enumerate_filesystem(N'D:\\it''s', N'*') WHERE level = 0"
    );
    assert_eq!(dirtree_query("/var"), "EXEC master.sys.xp_dirtree N'/var', 1, 1");
    let e = parse_dirtree(&[vec![Some("a".into()), Some("1".into()), Some("1".into())], vec![Some("d".into()), Some("1".into()), Some("0".into())]]);
    assert!(!e[0].is_dir && e[1].is_dir);
}

#[test]
fn backup_scripts() {
    let mut o = BackupOptions {
        db: "Sales".into(),
        kind: BackupType::Full,
        copy_only: false,
        files: vec![r"D:\B\Sales.bak".into()],
        compression: Compression::ServerDefault,
        checksum: false,
        init: true,
        verify: false,
        certificate: None,
    };
    assert_eq!(
        backup_script(&o).unwrap(),
        "BACKUP DATABASE [Sales] TO DISK = N'D:\\B\\Sales.bak'\nWITH INIT, NAME = N'Sales-Full Database Backup', STATS = 5;\nGO\n"
    );
    o.kind = BackupType::Differential;
    o.copy_only = true;
    o.compression = Compression::On;
    o.checksum = true;
    o.files.push(r"E:\B\Sales_2.bak".into());
    o.verify = true;
    let s = backup_script(&o).unwrap();
    assert!(s.starts_with("BACKUP DATABASE [Sales] TO DISK = N'D:\\B\\Sales.bak', DISK = N'E:\\B\\Sales_2.bak'\nWITH DIFFERENTIAL, COPY_ONLY, INIT,"), "{s}");
    assert!(s.contains("COMPRESSION, CHECKSUM, STATS = 5;"), "{s}");
    assert!(s.contains("RESTORE VERIFYONLY FROM DISK = N'D:\\B\\Sales.bak', DISK = N'E:\\B\\Sales_2.bak' WITH FILE = 1, CHECKSUM;"), "{s}");

    o.kind = BackupType::Log;
    o.init = false;
    o.compression = Compression::Off;
    let s = backup_script(&o).unwrap();
    assert!(s.starts_with("BACKUP LOG [Sales]"), "{s}");
    assert!(s.contains("NOINIT") && s.contains("NO_COMPRESSION"), "{s}");
    assert!(s.contains("WITH FILE = @position, CHECKSUM"), "appended: verify the last set: {s}");

    o.certificate = Some("BackupCert".into());
    let s = backup_script(&o).unwrap();
    assert!(s.contains("FORMAT, INIT") && s.contains("ENCRYPTION (ALGORITHM = AES_256, SERVER CERTIFICATE = [BackupCert])"), "{s}");

    o.files = vec!["  ".into()];
    assert!(backup_script(&o).is_err());
    assert_eq!(default_file_name("a/b", BackupType::Full, dt()), "a_b_20261125_130405.bak");
    assert_eq!(default_file_name("x", BackupType::Log, dt()), "x_20261125_130405.trn");
}

fn set(kind: BackupKind, first: u128, last: u128, dblsn: u128, ckpt: u128) -> BackupSet {
    BackupSet {
        kind: Some(kind),
        files: vec![format!("{first}.bak")],
        position: 1,
        first_lsn: first,
        last_lsn: last,
        database_backup_lsn: dblsn,
        checkpoint_lsn: ckpt,
        ..Default::default()
    }
}

#[test]
fn restore_chain_full_diff_logs() {
    use BackupKind::*;
    let sets = vec![
        set(Full, 10, 20, 0, 15),         // 0: old full
        set(Log, 5, 25, 15, 15),          // 1: log spanning the old full
        set(Full, 100, 120, 15, 110),     // 2: latest full
        set(Log, 25, 130, 15, 110),       // 3: spans the latest full
        set(Differential, 140, 150, 110, 145), // 4: diff on the latest full
        set(Log, 130, 160, 110, 145),     // 5: spans the diff
        set(Log, 160, 170, 110, 145),     // 6
        set(Differential, 200, 210, 15, 205),  // 7: diff on the old full: ignored
        set(Log, 175, 180, 110, 145),     // 8: gap after 6: chain stops
    ];
    assert_eq!(restore_chain(&sets), vec![2, 4, 5, 6]);

    // Without the diff, the logs follow the full directly.
    let no_diff: Vec<BackupSet> = sets.iter().enumerate().filter(|(i, _)| *i != 4).map(|(_, s)| s.clone()).collect();
    let chain: Vec<u128> = restore_chain(&no_diff).iter().map(|&i| no_diff[i].first_lsn).collect();
    assert_eq!(chain, vec![100, 25, 130, 160]);

    // Copy-only logs are skipped; no full means no chain.
    let mut copy = set(Log, 120, 999, 110, 110);
    copy.copy_only = true;
    assert_eq!(restore_chain(&[set(Full, 100, 120, 0, 110), copy]), vec![0]);
    assert!(restore_chain(&[set(Log, 1, 2, 0, 0)]).is_empty());
}

fn opts(steps: Vec<RestoreStep>) -> RestoreOptions {
    RestoreOptions {
        target: "Sales_Copy".into(),
        steps,
        moves: vec![("Sales".into(), r"D:\D\Sales_Copy.mdf".into()), ("Sales_log".into(), r"D:\L\Sales_Copy_log.ldf".into())],
        replace: false,
        recovery: RecoveryState::Recovery,
        stop_at: None,
        close_connections: false,
        keep_replication: false,
        checksum: false,
    }
}

fn step(kind: BackupKind, file: &str, position: i32) -> RestoreStep {
    RestoreStep { kind, files: vec![file.into()], position }
}

#[test]
fn restore_full_with_move() {
    let s = restore_script(&opts(vec![step(BackupKind::Full, r"D:\B\Sales.bak", 1)])).unwrap();
    assert_eq!(
        s,
        "USE [master];\nGO\nRESTORE DATABASE [Sales_Copy] FROM DISK = N'D:\\B\\Sales.bak'\n\
         WITH FILE = 1,\n    MOVE N'Sales' TO N'D:\\D\\Sales_Copy.mdf',\n    MOVE N'Sales_log' TO N'D:\\L\\Sales_Copy_log.ldf',\n    \
         RECOVERY,\n    STATS = 5;\nGO\n"
    );
}

#[test]
fn restore_chain_script() {
    let mut o = opts(vec![
        step(BackupKind::Full, "/b/full.bak", 1),
        step(BackupKind::Differential, "/b/diff.dif", 2),
        step(BackupKind::Log, "/b/log.trn", 1),
        step(BackupKind::Log, "/b/log.trn", 2),
    ]);
    o.replace = true;
    o.close_connections = true;
    o.keep_replication = true;
    o.checksum = true;
    o.stop_at = Some("2026-11-25 13:00:00".into());
    let s = restore_script(&o).unwrap();
    assert!(s.contains("IF DB_ID(N'Sales_Copy') IS NOT NULL ALTER DATABASE [Sales_Copy] SET SINGLE_USER WITH ROLLBACK IMMEDIATE;"));
    let restores: Vec<&str> = s.split("GO\n").filter(|b| b.starts_with("RESTORE")).collect();
    assert_eq!(restores.len(), 4, "{s}");
    assert!(restores[0].contains("MOVE N'Sales'") && restores[0].contains("REPLACE") && restores[0].contains("NORECOVERY"));
    assert!(restores[1].starts_with("RESTORE DATABASE") && restores[1].contains("FILE = 2") && !restores[1].contains("MOVE"));
    assert!(restores[2].starts_with("RESTORE LOG") && restores[2].contains("STOPAT = N'2026-11-25 13:00:00'") && restores[2].contains("NORECOVERY"));
    assert!(restores[3].contains("KEEP_REPLICATION,\n    RECOVERY"), "{}", restores[3]);
    assert!(restores.iter().all(|r| r.contains("CHECKSUM") && r.contains("STATS = 5")));
    assert!(s.ends_with("ALTER DATABASE [Sales_Copy] SET MULTI_USER;\nGO\n"));

    o.recovery = RecoveryState::Standby("/b/undo.bak".into());
    let s = restore_script(&o).unwrap();
    assert!(s.contains("STANDBY = N'/b/undo.bak'") && !s.contains("KEEP_REPLICATION"));
    o.recovery = RecoveryState::NoRecovery;
    assert!(!restore_script(&o).unwrap().contains("MULTI_USER"));

    // Validation.
    assert!(restore_script(&opts(vec![])).is_err());
    assert!(restore_script(&opts(vec![step(BackupKind::Log, "x", 1)])).is_err());
    assert!(restore_script(&opts(vec![step(BackupKind::Full, "x", 1), step(BackupKind::Full, "y", 1)])).is_err());
    let mut o = opts(vec![step(BackupKind::Full, "x", 1)]);
    o.stop_at = Some("2026-01-01".into());
    assert!(restore_script(&o).is_err(), "STOPAT without logs");
    o.stop_at = None;
    o.target = " ".into();
    assert!(restore_script(&o).is_err());
}

#[test]
fn moves_rename_for_the_new_database() {
    let files = vec![
        LogicalFile { logical: "Sales".into(), physical: r"C:\Old\Sales.mdf".into(), kind: "D".into(), filegroup: "PRIMARY".into(), size: 0 },
        LogicalFile { logical: "Sales_log".into(), physical: r"C:\Old\sales_log.ldf".into(), kind: "L".into(), filegroup: String::new(), size: 0 },
        LogicalFile { logical: "Extra".into(), physical: r"C:\Old\data.ndf".into(), kind: "D".into(), filegroup: "FG".into(), size: 0 },
    ];
    let linux = ServerFolders { backup: "/b".into(), data: "/var/opt/mssql/data".into(), log: "/var/opt/mssql/log".into(), style: PathStyle::Posix };
    // A backup from Windows restored on Linux: the old name's folders don't matter.
    assert_eq!(
        default_moves(&files, "Sales", "Sales_Copy", &linux),
        vec![
            ("Sales".into(), "/var/opt/mssql/data/Sales_Copy.mdf".into()),
            ("Sales_log".into(), "/var/opt/mssql/log/Sales_Copy_log.ldf".into()),
            ("Extra".into(), "/var/opt/mssql/data/Sales_Copy_data.ndf".into()),
        ]
    );
    let win = ServerFolders { backup: r"D:\B".into(), data: r"D:\D".into(), log: r"D:\D".into(), style: PathStyle::Windows };
    let same = vec![files[0].clone(), LogicalFile { logical: "Sales2".into(), ..files[0].clone() }];
    assert_eq!(
        default_moves(&same, "Sales", "Sales", &win),
        vec![("Sales".into(), r"D:\D\Sales.mdf".into()), ("Sales2".into(), r"D:\D\Sales_2.mdf".into())]
    );
}

#[test]
fn header_parsing_by_column_name() {
    let names = ["BackupName", "BackupType", "Position", "DatabaseName", "FirstLSN", "LastLSN", "CheckpointLSN", "DatabaseBackupLSN", "IsCopyOnly", "BackupSize"];
    let cols: Vec<ColumnMeta> = names.iter().map(|n| ColumnMeta { name: n.to_string(), type_name: String::new(), numeric: false }).collect();
    let row: Vec<Cell> = ["n", "5", "3", "Sales", "37000000012300001", "37000000014500001", "37000000013000002", "37000000009900037", "0", "123"]
        .iter()
        .map(|v| Some(v.to_string()))
        .collect();
    let h = &parse_headers(&cols, &[row], &["a.bak".into()])[0];
    assert_eq!(h.kind(), BackupKind::Differential);
    assert_eq!(h.position, 3);
    assert_eq!(h.first_lsn, 37000000012300001);
    assert_eq!(h.database_backup_lsn, 37000000009900037);
    assert_eq!(h.files, vec!["a.bak".to_string()]);
    assert_eq!(headeronly_query(&["a".into(), "b'c".into()]), "RESTORE HEADERONLY FROM DISK = N'a', DISK = N'b''c'");
    assert_eq!(filelistonly_query(&["a".into()], 2), "RESTORE FILELISTONLY FROM DISK = N'a' WITH FILE = 2");
}

#[test]
fn history_groups_media_families() {
    let r = |id: &str, ty: &str, file: &str| -> Vec<Cell> {
        [id, ty, "1", "2026-11-25 10:00:00", "2026-11-25 10:01:00", "100", "200", "0", "150", "0", "10", file, "srv", "nm"]
            .iter()
            .map(|v| Some(v.to_string()))
            .collect()
    };
    let h = parse_history("Sales", &[r("1", "D", "a.bak"), r("1", "D", "b.bak"), r("2", "L", "c.trn")]);
    assert_eq!(h.len(), 2);
    assert_eq!(h[0].files, vec!["a.bak".to_string(), "b.bak".to_string()]);
    assert_eq!((h[0].kind(), h[1].kind()), (BackupKind::Full, BackupKind::Log));
    assert!(history_query("o'k").contains("bs.database_name = N'o''k'"));
}
