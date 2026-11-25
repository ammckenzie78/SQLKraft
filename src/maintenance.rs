//! Shrink Database / Shrink Files: the queries behind the dialog and the DBCC script builders.
//!
//! Every query that needs a database's context (`FILEPROPERTY`, `sys.database_files`) runs
//! through `[db].sys.sp_executesql`, so it never depends on the session's current database.

use crate::db::Cell;
use crate::sql::{quote_ident, quote_literal};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    Data,
    Log,
    Filestream,
}

impl FileType {
    pub const ALL: [FileType; 3] = [FileType::Data, FileType::Log, FileType::Filestream];

    pub fn label(self) -> &'static str {
        match self {
            FileType::Data => "Data",
            FileType::Log => "Log",
            FileType::Filestream => "FILESTREAM data",
        }
    }

    /// From `sys.database_files.type_desc`.
    pub fn from_type_desc(s: &str) -> FileType {
        match s {
            "LOG" => FileType::Log,
            "FILESTREAM" => FileType::Filestream,
            _ => FileType::Data,
        }
    }
}

/// One row of [`files_query`]. Sizes are in MB.
#[derive(Debug, Clone, PartialEq)]
pub struct DbFile {
    pub file_id: i32,
    pub name: String,
    pub physical_name: String,
    pub filegroup: String,
    pub file_type: FileType,
    pub size_mb: f64,
    /// `None` when `FILEPROPERTY` returns NULL (FILESTREAM containers, missing permission).
    pub used_mb: Option<f64>,
}

impl DbFile {
    pub fn free_mb(&self) -> Option<f64> {
        self.used_mb.map(|u| (self.size_mb - u).max(0.0))
    }

    pub fn free_pct(&self) -> Option<f64> {
        match self.free_mb() {
            Some(f) if self.size_mb > 0.0 => Some(f * 100.0 / self.size_mb),
            _ => None,
        }
    }
}

/// Files of `db` with size and space used, run in that database's context.
pub fn files_query(db: &str) -> String {
    let inner = "SELECT f.file_id, f.name, f.physical_name, ISNULL(fg.name, N''), f.type_desc, \
                 CAST(f.size / 128.0 AS decimal(18,2)), \
                 CAST(FILEPROPERTY(f.name, 'SpaceUsed') / 128.0 AS decimal(18,2)) \
                 FROM sys.database_files f LEFT JOIN sys.filegroups fg ON fg.data_space_id = f.data_space_id \
                 ORDER BY f.file_id";
    format!("EXEC {}.sys.sp_executesql {}", quote_ident(db), quote_literal(inner))
}

pub fn parse_files(rows: &[Vec<Cell>]) -> Vec<DbFile> {
    rows.iter()
        .map(|r| {
            let s = |i: usize| r.get(i).cloned().flatten().unwrap_or_default();
            DbFile {
                file_id: s(0).parse().unwrap_or(0),
                name: s(1),
                physical_name: s(2),
                filegroup: s(3),
                file_type: FileType::from_type_desc(&s(4)),
                size_mb: s(5).parse().unwrap_or(0.0),
                used_mb: r.get(6).cloned().flatten().and_then(|v| v.parse().ok()),
            }
        })
        .collect()
}

/// Why the log can't be reused, and the recovery model.
pub fn log_reuse_query(db: &str) -> String {
    format!("SELECT log_reuse_wait_desc, recovery_model_desc FROM sys.databases WHERE name = {}", quote_literal(db))
}

/// Number of VLFs (`sys.dm_db_log_info`, 2016 SP2+). Fails on older servers; callers show n/a.
pub fn vlf_count_query(db: &str) -> String {
    let inner = "SELECT COUNT(*) FROM sys.dm_db_log_info(DB_ID())";
    format!("EXEC {}.sys.sp_executesql {}", quote_ident(db), quote_literal(inner))
}

/// Whether a server error is a missing permission (Msg 229, 297, 300, 262, ...).
pub fn is_permission_error(message: &str) -> bool {
    let m = message.to_ascii_lowercase();
    m.contains("permission") && (m.contains("denied") || m.contains("does not have"))
}

/// What a `log_reuse_wait_desc` value means for shrinking the log, if it blocks it.
pub fn log_reuse_hint(wait: &str) -> Option<&'static str> {
    Some(match wait {
        "NOTHING" | "" => return None,
        "LOG_BACKUP" => "The log is waiting for a log backup. Back up the log first, then shrink.",
        "ACTIVE_TRANSACTION" => "An open transaction is holding the log. Commit or roll it back first.",
        "CHECKPOINT" => "The log is waiting for a checkpoint. Run CHECKPOINT in the database, then shrink.",
        "ACTIVE_BACKUP_OR_RESTORE" => "A backup or restore is running. Wait for it to finish.",
        "REPLICATION" => "Transactions are waiting to be read by replication or CDC.",
        "DATABASE_MIRRORING" | "AVAILABILITY_REPLICA" => "A mirroring partner or replica has not caught up yet.",
        "OLDEST_PAGE" => "Indirect checkpoint has not caught up yet. Run CHECKPOINT and try again.",
        _ => "The log can't be truncated right now, so a shrink may release little space.",
    })
}

/// `percent_complete` of a running request, read from another connection.
pub fn progress_query(spid: i32) -> String {
    format!("SELECT CAST(percent_complete AS float), command FROM sys.dm_exec_requests WHERE session_id = {spid}")
}

/// `SERVERPROPERTY('ProductVersion')` major version, e.g. 16 for SQL Server 2022.
pub fn major_version(product_version: &str) -> u32 {
    product_version.split('.').next().and_then(|v| v.parse().ok()).unwrap_or(0)
}

/// `WAIT_AT_LOW_PRIORITY` for DBCC SHRINK* arrived in SQL Server 2022 (16.x).
pub fn supports_low_priority(product_version: &str) -> bool {
    major_version(product_version) >= 16
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ShrinkFileAction {
    /// Release unused space at the end of the file (`TRUNCATEONLY`).
    TruncateOnly,
    /// Move pages to the front, then shrink to this size in MB.
    Reorganize { target_mb: u64 },
    /// Move everything to other files in the filegroup (`EMPTYFILE`).
    EmptyFile,
}

fn low_priority_clause(low_priority: bool) -> &'static str {
    if low_priority { " WITH WAIT_AT_LOW_PRIORITY" } else { "" }
}

/// `DBCC SHRINKFILE` for one file, with a `USE` so it runs in the right database.
pub fn shrink_file_script(db: &str, file: &str, action: ShrinkFileAction, low_priority: bool) -> String {
    let arg = match action {
        ShrinkFileAction::TruncateOnly => "TRUNCATEONLY".to_string(),
        ShrinkFileAction::Reorganize { target_mb } => target_mb.to_string(),
        ShrinkFileAction::EmptyFile => "EMPTYFILE".to_string(),
    };
    format!(
        "USE {};\nGO\nDBCC SHRINKFILE ({}, {arg}){};\nGO\n",
        quote_ident(db),
        quote_literal(file),
        low_priority_clause(low_priority)
    )
}

/// `DBCC SHRINKDATABASE`, leaving `target_percent_free` percent free in each file.
/// `truncate_only` only releases space at the end of the files and ignores the target.
pub fn shrink_database_script(db: &str, target_percent_free: u8, truncate_only: bool, low_priority: bool) -> String {
    let args = if truncate_only {
        "TRUNCATEONLY".to_string()
    } else {
        target_percent_free.min(99).to_string()
    };
    format!(
        "DBCC SHRINKDATABASE ({}, {args}){};\nGO\n",
        quote_ident(db),
        low_priority_clause(low_priority)
    )
}

/// Checks a Reorganize target against the file: `Err` explains why it can't be run.
pub fn validate_target(file: &DbFile, target_mb: u64) -> Result<(), String> {
    if (target_mb as f64) >= file.size_mb {
        return Err(format!("The target must be below the current size ({:.0} MB).", file.size_mb));
    }
    if let Some(used) = file.used_mb
        && (target_mb as f64) < used.ceil()
    {
        return Err(format!("The target can't be below the space used ({:.0} MB).", used.ceil()));
    }
    Ok(())
}

/// Whether `EMPTYFILE` can work: there must be another file of the same type in its filegroup.
pub fn can_empty(files: &[DbFile], file: &DbFile) -> bool {
    files.iter().any(|f| f.file_id != file.file_id && f.file_type == file.file_type && f.filegroup == file.filegroup)
}

/// Totals for the "before / after" summary.
pub fn total_size_mb(files: &[DbFile]) -> f64 {
    files.iter().map(|f| f.size_mb).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(id: i32, ty: FileType, fg: &str, size: f64, used: Option<f64>) -> DbFile {
        DbFile {
            file_id: id,
            name: format!("f{id}"),
            physical_name: String::new(),
            filegroup: fg.into(),
            file_type: ty,
            size_mb: size,
            used_mb: used,
        }
    }

    #[test]
    fn shrink_file_scripts() {
        assert_eq!(
            shrink_file_script("My]Db", "data's", ShrinkFileAction::TruncateOnly, false),
            "USE [My]]Db];\nGO\nDBCC SHRINKFILE (N'data''s', TRUNCATEONLY);\nGO\n"
        );
        assert_eq!(
            shrink_file_script("d", "f", ShrinkFileAction::Reorganize { target_mb: 64 }, false),
            "USE [d];\nGO\nDBCC SHRINKFILE (N'f', 64);\nGO\n"
        );
        assert_eq!(
            shrink_file_script("d", "f", ShrinkFileAction::EmptyFile, true),
            "USE [d];\nGO\nDBCC SHRINKFILE (N'f', EMPTYFILE) WITH WAIT_AT_LOW_PRIORITY;\nGO\n"
        );
    }

    #[test]
    fn shrink_database_scripts() {
        assert_eq!(shrink_database_script("d", 10, false, false), "DBCC SHRINKDATABASE ([d], 10);\nGO\n");
        assert_eq!(shrink_database_script("d", 120, false, false), "DBCC SHRINKDATABASE ([d], 99);\nGO\n");
        assert_eq!(
            shrink_database_script("d", 10, true, true),
            "DBCC SHRINKDATABASE ([d], TRUNCATEONLY) WITH WAIT_AT_LOW_PRIORITY;\nGO\n"
        );
    }

    #[test]
    fn queries_run_in_the_database() {
        let q = files_query("a'b]");
        assert!(q.starts_with("EXEC [a'b]]].sys.sp_executesql N'SELECT"), "{q}");
        assert!(q.contains("FILEPROPERTY(f.name, ''SpaceUsed'')"), "{q}");
        assert!(vlf_count_query("x").starts_with("EXEC [x].sys.sp_executesql"));
        assert!(log_reuse_query("o'k").ends_with("WHERE name = N'o''k'"));
    }

    #[test]
    fn parse_and_free_space() {
        let rows = vec![vec![
            Some("1".into()),
            Some("db".into()),
            Some("C:\\db.mdf".into()),
            Some("PRIMARY".into()),
            Some("ROWS".into()),
            Some("100.00".into()),
            Some("25.50".into()),
        ]];
        let f = &parse_files(&rows)[0];
        assert_eq!(f.file_type, FileType::Data);
        assert_eq!(f.free_mb(), Some(74.5));
        assert_eq!(f.free_pct(), Some(74.5));
        let fs = file(3, FileType::Filestream, "FS", 0.0, None);
        assert_eq!(fs.free_pct(), None);
    }

    #[test]
    fn target_validation() {
        let f = file(1, FileType::Data, "PRIMARY", 100.0, Some(20.2));
        assert!(validate_target(&f, 50).is_ok());
        assert!(validate_target(&f, 21).is_ok());
        assert!(validate_target(&f, 20).is_err(), "below space used");
        assert!(validate_target(&f, 100).is_err(), "not smaller");
    }

    #[test]
    fn empty_file_needs_a_sibling() {
        let a = file(1, FileType::Data, "PRIMARY", 10.0, None);
        let b = file(3, FileType::Data, "PRIMARY", 10.0, None);
        let log = file(2, FileType::Log, "", 10.0, None);
        assert!(!can_empty(&[a.clone(), log.clone()], &a));
        assert!(can_empty(&[a.clone(), b, log.clone()], &a));
        assert!(!can_empty(&[a, log.clone()], &log));
    }

    #[test]
    fn versions_and_hints() {
        assert!(supports_low_priority("16.0.1000.6"));
        assert!(!supports_low_priority("15.0.2000.5"));
        assert_eq!(major_version(""), 0);
        assert!(log_reuse_hint("NOTHING").is_none());
        assert!(log_reuse_hint("LOG_BACKUP").unwrap().contains("log backup"));
        assert!(log_reuse_hint("ACTIVE_TRANSACTION").unwrap().contains("transaction"));
        assert!(is_permission_error("VIEW SERVER STATE permission was denied on object 'server'"));
        assert!(is_permission_error("User 'dbo' does not have permission to run DBCC LOGINFO."));
        assert!(!is_permission_error("Invalid object name 'x'."));
    }
}
