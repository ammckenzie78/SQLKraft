//! Back Up / Restore: server paths and the server-side file listing, RESTORE HEADERONLY /
//! FILELISTONLY parsing, msdb history, restore chains, and the BACKUP / RESTORE script builders.
//!
//! Paths are on the SQL Server machine, which may run a different OS than SQLKraft, so every
//! path operation goes through [`PathStyle`] (taken from `sys.dm_os_host_info`).
//!
//! Permissions, as checked against SQL Server 2019 with a `dbcreator`-only login:
//! - `sys.dm_os_enumerate_filesystem` (2017+): worked without VIEW SERVER STATE.
//! - `xp_dirtree`, `xp_fixeddrives`: no error, but return no rows unless sysadmin.
//! - `RESTORE HEADERONLY / FILELISTONLY / VERIFYONLY`: need CREATE DATABASE (dbcreator has it).
//! - `msdb.dbo.backupset` / `backupmediafamily`: readable.
//! - `xp_delete_file`: denied.

use crate::db::{Cell, ColumnMeta};
use crate::sql::{quote_ident, quote_literal};

// ---------------------------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PathStyle {
    #[default]
    Windows,
    Posix,
}

impl PathStyle {
    /// From `sys.dm_os_host_info.host_platform` (`Windows` / `Linux`).
    pub fn from_platform(platform: &str) -> PathStyle {
        if platform.eq_ignore_ascii_case("windows") { PathStyle::Windows } else { PathStyle::Posix }
    }

    /// Best guess from a path, for servers without `sys.dm_os_host_info` (pre-2017) and for
    /// physical names stored in a backup taken elsewhere.
    pub fn guess(path: &str) -> PathStyle {
        let b = path.as_bytes();
        if path.contains('\\') || (b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic()) {
            PathStyle::Windows
        } else {
            PathStyle::Posix
        }
    }

    pub fn sep(self) -> char {
        match self {
            PathStyle::Windows => '\\',
            PathStyle::Posix => '/',
        }
    }

    fn is_sep(self, c: char) -> bool {
        match self {
            PathStyle::Windows => c == '\\' || c == '/',
            PathStyle::Posix => c == '/',
        }
    }

    /// Length of the root prefix: `C:\`, `\\server\share\`, or `/`.
    fn root_len(self, p: &str) -> usize {
        match self {
            PathStyle::Posix => usize::from(p.starts_with('/')),
            PathStyle::Windows => {
                let b = p.as_bytes();
                if b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic() {
                    if b.len() >= 3 && self.is_sep(b[2] as char) { 3 } else { 2 }
                } else if p.starts_with("\\\\") {
                    // \\server\share\
                    let mut seps = 0;
                    for (i, c) in p.char_indices().skip(2) {
                        if self.is_sep(c) {
                            seps += 1;
                            if seps == 2 {
                                return i + 1;
                            }
                        }
                    }
                    p.len()
                } else {
                    0
                }
            }
        }
    }

    /// Trims trailing separators (keeping a root's), and turns `C:` into `C:\`.
    pub fn normalize_dir(self, path: &str) -> String {
        let p = path.trim();
        let root = self.root_len(p);
        let mut s = p.to_string();
        while s.len() > root && s.ends_with(|c| self.is_sep(c)) {
            s.pop();
        }
        if self == PathStyle::Windows && s.len() == 2 && s.ends_with(':') {
            s.push('\\');
        }
        s
    }

    pub fn join(self, dir: &str, name: &str) -> String {
        let d = self.normalize_dir(dir);
        if d.is_empty() {
            return name.to_string();
        }
        if d.ends_with(|c| self.is_sep(c)) { format!("{d}{name}") } else { format!("{d}{}{name}", self.sep()) }
    }

    pub fn is_root(self, path: &str) -> bool {
        let p = self.normalize_dir(path);
        !p.is_empty() && self.root_len(&p) == p.len()
    }

    /// The containing folder, or `None` at a root.
    pub fn parent(self, path: &str) -> Option<String> {
        let p = self.normalize_dir(path);
        let root = self.root_len(&p);
        if p.len() <= root {
            return None;
        }
        let cut = p[root..].rfind(|c| self.is_sep(c)).map(|i| i + root);
        let parent = match cut {
            Some(i) => &p[..i.max(root)],
            None if root > 0 => &p[..root],
            None => return None,
        };
        Some(self.normalize_dir(parent))
    }

    pub fn file_name(self, path: &str) -> String {
        let p = self.normalize_dir(path);
        let root = self.root_len(&p);
        match p[root..].rfind(|c| self.is_sep(c)) {
            Some(i) => p[root + i + 1..].to_string(),
            None => p[root..].to_string(),
        }
    }

    /// (label, path) for each level, root first: `D:\`, `SQL`, `Backup`.
    pub fn breadcrumbs(self, path: &str) -> Vec<(String, String)> {
        let p = self.normalize_dir(path);
        let root = self.root_len(&p);
        let mut out = Vec::new();
        if root > 0 {
            out.push((p[..root].to_string(), self.normalize_dir(&p[..root])));
        }
        let mut cur = p[..root].to_string();
        for part in p[root..].split(|c| self.is_sep(c)).filter(|s| !s.is_empty()) {
            cur = if cur.is_empty() { part.to_string() } else { self.join(&cur, part) };
            out.push((part.to_string(), cur.clone()));
        }
        out
    }
}

/// `name` matches one of `;`-separated patterns with `*` and `?`, ignoring case.
pub fn matches_filter(name: &str, filter: &str) -> bool {
    let pats: Vec<&str> = filter.split(';').map(str::trim).filter(|p| !p.is_empty()).collect();
    pats.is_empty() || pats.iter().any(|p| glob(&p.to_lowercase(), &name.to_lowercase()))
}

fn glob(p: &str, s: &str) -> bool {
    let (p, s): (Vec<char>, Vec<char>) = (p.chars().collect(), s.chars().collect());
    let (mut pi, mut si, mut star, mut mark) = (0, 0, None, 0);
    while si < s.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = si;
            pi += 1;
        } else if let Some(st) = star {
            pi = st + 1;
            mark += 1;
            si = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

pub const DEFAULT_FILTER: &str = "*.bak;*.trn;*.dif";

// ---------------------------------------------------------------------------------------------
// Server folders and listings
// ---------------------------------------------------------------------------------------------

/// Default folders, with fallbacks for servers older than 2019 (backup) / 2012 (data, log).
pub const SERVER_PATHS_QUERY: &str = "SELECT CAST(SERVERPROPERTY('InstanceDefaultBackupPath') AS nvarchar(4000)), \
     CAST(SERVERPROPERTY('InstanceDefaultDataPath') AS nvarchar(4000)), \
     CAST(SERVERPROPERTY('InstanceDefaultLogPath') AS nvarchar(4000)), \
     (SELECT physical_name FROM sys.master_files WHERE database_id = 1 AND file_id = 1)";

/// SQL Server 2017+; on older servers the path style is guessed instead.
pub const HOST_PLATFORM_QUERY: &str = "SELECT host_platform FROM sys.dm_os_host_info";

/// Drive roots that hold database files (Windows); visible to any login with CREATE DATABASE.
pub const FILE_DRIVES_QUERY: &str =
    "SELECT DISTINCT UPPER(LEFT(physical_name, 3)) FROM sys.master_files WHERE physical_name LIKE N'_:\\%'";

/// Fixed drives; returns nothing unless the login is sysadmin.
pub const FIXED_DRIVES_QUERY: &str = "EXEC master.sys.xp_fixeddrives";

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ServerFolders {
    pub backup: String,
    pub data: String,
    pub log: String,
    pub style: PathStyle,
}

pub fn parse_server_folders(row: &[Cell], host_platform: Option<&str>) -> ServerFolders {
    let s = |i: usize| row.get(i).cloned().flatten().unwrap_or_default();
    let master = s(3);
    let style = match host_platform {
        Some(p) if !p.is_empty() => PathStyle::from_platform(p),
        _ => PathStyle::guess(&master),
    };
    let master_dir = style.parent(&master).unwrap_or_default();
    let or = |v: String, d: &str| if v.trim().is_empty() { d.to_string() } else { style.normalize_dir(&v) };
    let data = or(s(1), &master_dir);
    let log = or(s(2), &data);
    let backup = or(s(0), &data);
    ServerFolders { backup, data, log, style }
}

#[derive(Debug, Clone, PartialEq)]
pub struct FsEntry {
    pub name: String,
    pub is_dir: bool,
    pub size: Option<i64>,
    pub modified: Option<String>,
}

/// One folder level (the DMV is recursive, hence `level = 0`).
pub fn enumerate_query(dir: &str) -> String {
    format!(
        "SELECT file_or_directory_name, CAST(is_directory AS int), size_in_bytes, \
         CONVERT(varchar(19), last_write_time, 120) \
         FROM sys.dm_os_enumerate_filesystem({}, N'*') WHERE level = 0",
        quote_literal(dir)
    )
}

/// Fallback for servers before 2017: names only.
pub fn dirtree_query(dir: &str) -> String {
    format!("EXEC master.sys.xp_dirtree {}, 1, 1", quote_literal(dir))
}

pub fn parse_enumerate(rows: &[Vec<Cell>]) -> Vec<FsEntry> {
    rows.iter()
        .map(|r| FsEntry {
            name: r.first().cloned().flatten().unwrap_or_default(),
            is_dir: r.get(1).cloned().flatten().as_deref() == Some("1"),
            size: r.get(2).cloned().flatten().and_then(|v| v.parse().ok()),
            modified: r.get(3).cloned().flatten(),
        })
        .collect()
}

/// `xp_dirtree` rows: subdirectory, depth, file (1 = file).
pub fn parse_dirtree(rows: &[Vec<Cell>]) -> Vec<FsEntry> {
    rows.iter()
        .map(|r| FsEntry {
            name: r.first().cloned().flatten().unwrap_or_default(),
            is_dir: r.get(2).cloned().flatten().as_deref() != Some("1"),
            size: None,
            modified: None,
        })
        .collect()
}

/// Whether an error means the DMV doesn't exist (pre-2017), so `xp_dirtree` is worth a try.
pub fn is_missing_object(err: &str) -> bool {
    err.contains("Invalid object name") || err.contains("is not a recognized")
}

// ---------------------------------------------------------------------------------------------
// Backup sets: RESTORE HEADERONLY / FILELISTONLY and msdb history
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackupKind {
    Full,
    Differential,
    Log,
    Other,
}

impl BackupKind {
    /// `RESTORE HEADERONLY` BackupType.
    pub fn from_header(t: &str) -> BackupKind {
        match t {
            "1" => BackupKind::Full,
            "2" => BackupKind::Log,
            "5" => BackupKind::Differential,
            _ => BackupKind::Other,
        }
    }

    /// `msdb.dbo.backupset.type`.
    pub fn from_msdb(t: &str) -> BackupKind {
        match t {
            "D" => BackupKind::Full,
            "I" => BackupKind::Differential,
            "L" => BackupKind::Log,
            _ => BackupKind::Other,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            BackupKind::Full => "Full",
            BackupKind::Differential => "Differential",
            BackupKind::Log => "Transaction Log",
            BackupKind::Other => "Other",
        }
    }
}

/// A backup set from a header or from msdb history.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct BackupSet {
    pub kind: Option<BackupKind>,
    /// The media set's files (several for a striped backup).
    pub files: Vec<String>,
    pub position: i32,
    pub database: String,
    pub server: String,
    pub name: String,
    pub start: String,
    pub finish: String,
    pub size: i64,
    pub first_lsn: u128,
    pub last_lsn: u128,
    pub database_backup_lsn: u128,
    pub checkpoint_lsn: u128,
    pub copy_only: bool,
    pub compressed: bool,
    pub recovery_model: String,
}

impl BackupSet {
    pub fn kind(&self) -> BackupKind {
        self.kind.unwrap_or(BackupKind::Other)
    }
}

fn from_disk(files: &[String]) -> String {
    files.iter().map(|f| format!("DISK = {}", quote_literal(f))).collect::<Vec<_>>().join(", ")
}

pub fn headeronly_query(files: &[String]) -> String {
    format!("RESTORE HEADERONLY FROM {}", from_disk(files))
}

pub fn filelistonly_query(files: &[String], position: i32) -> String {
    format!("RESTORE FILELISTONLY FROM {} WITH FILE = {}", from_disk(files), position.max(1))
}

fn lsn(v: &str) -> u128 {
    v.split('.').next().unwrap_or("").trim().parse().unwrap_or(0)
}

fn by_name<'a>(cols: &[ColumnMeta], row: &'a [Cell], name: &str) -> &'a str {
    cols.iter()
        .position(|c| c.name.eq_ignore_ascii_case(name))
        .and_then(|i| row.get(i))
        .and_then(|c| c.as_deref())
        .unwrap_or("")
}

/// Parses `RESTORE HEADERONLY` (columns by name, which vary between versions).
pub fn parse_headers(cols: &[ColumnMeta], rows: &[Vec<Cell>], files: &[String]) -> Vec<BackupSet> {
    rows.iter()
        .map(|r| {
            let g = |n: &str| by_name(cols, r, n).to_string();
            BackupSet {
                kind: Some(BackupKind::from_header(&g("BackupType"))),
                files: files.to_vec(),
                position: g("Position").parse().unwrap_or(1),
                database: g("DatabaseName"),
                server: g("ServerName"),
                name: g("BackupName"),
                start: g("BackupStartDate"),
                finish: g("BackupFinishDate"),
                size: g("BackupSize").parse().unwrap_or(0),
                first_lsn: lsn(&g("FirstLSN")),
                last_lsn: lsn(&g("LastLSN")),
                database_backup_lsn: lsn(&g("DatabaseBackupLSN")),
                checkpoint_lsn: lsn(&g("CheckpointLSN")),
                copy_only: g("IsCopyOnly") == "1",
                compressed: g("Compressed") == "1",
                recovery_model: g("RecoveryModel"),
            }
        })
        .collect()
}

/// A database file inside a backup (`RESTORE FILELISTONLY`).
#[derive(Debug, Clone, PartialEq)]
pub struct LogicalFile {
    pub logical: String,
    pub physical: String,
    /// D data, L log, S FILESTREAM / memory-optimized, F full-text catalog.
    pub kind: String,
    pub filegroup: String,
    pub size: i64,
}

pub fn parse_filelist(cols: &[ColumnMeta], rows: &[Vec<Cell>]) -> Vec<LogicalFile> {
    rows.iter()
        .map(|r| LogicalFile {
            logical: by_name(cols, r, "LogicalName").to_string(),
            physical: by_name(cols, r, "PhysicalName").to_string(),
            kind: by_name(cols, r, "Type").to_string(),
            filegroup: by_name(cols, r, "FileGroupName").to_string(),
            size: by_name(cols, r, "Size").parse().unwrap_or(0),
        })
        .collect()
}

pub const HISTORY_DATABASES_QUERY: &str =
    "SELECT DISTINCT database_name FROM msdb.dbo.backupset WHERE database_name IS NOT NULL ORDER BY database_name";

/// Disk backups of `db` from msdb, one row per media family.
pub fn history_query(db: &str) -> String {
    format!(
        "SELECT bs.backup_set_id, bs.type, bs.position, CONVERT(varchar(19), bs.backup_start_date, 120), \
         CONVERT(varchar(19), bs.backup_finish_date, 120), CAST(bs.first_lsn AS varchar(30)), \
         CAST(bs.last_lsn AS varchar(30)), CAST(bs.database_backup_lsn AS varchar(30)), \
         CAST(bs.checkpoint_lsn AS varchar(30)), CAST(bs.is_copy_only AS int), \
         CAST(bs.backup_size AS bigint), mf.physical_device_name, bs.server_name, bs.name \
         FROM msdb.dbo.backupset bs JOIN msdb.dbo.backupmediafamily mf ON mf.media_set_id = bs.media_set_id \
         WHERE bs.database_name = {} AND mf.device_type = 2 AND ISNULL(mf.mirror, 0) = 0 \
         ORDER BY bs.backup_set_id, mf.family_sequence_number",
        quote_literal(db)
    )
}

pub fn parse_history(db: &str, rows: &[Vec<Cell>]) -> Vec<BackupSet> {
    let mut out: Vec<(String, BackupSet)> = Vec::new();
    for r in rows {
        let s = |i: usize| r.get(i).cloned().flatten().unwrap_or_default();
        let id = s(0);
        if let Some((_, set)) = out.iter_mut().find(|(i, _)| *i == id) {
            set.files.push(s(11));
            continue;
        }
        out.push((
            id,
            BackupSet {
                kind: Some(BackupKind::from_msdb(&s(1))),
                files: vec![s(11)],
                position: s(2).parse().unwrap_or(1),
                database: db.to_string(),
                server: s(12),
                name: s(13),
                start: s(3),
                finish: s(4),
                size: s(10).parse().unwrap_or(0),
                first_lsn: lsn(&s(5)),
                last_lsn: lsn(&s(6)),
                database_backup_lsn: lsn(&s(7)),
                checkpoint_lsn: lsn(&s(8)),
                copy_only: s(9) == "1",
                ..Default::default()
            },
        ));
    }
    out.into_iter().map(|(_, s)| s).collect()
}

/// Indices of the sets to restore, in order: the latest full, the latest differential based on
/// it, then the unbroken chain of log backups after that.
pub fn restore_chain(sets: &[BackupSet]) -> Vec<usize> {
    let Some(full) = (0..sets.len()).filter(|&i| sets[i].kind() == BackupKind::Full).max_by_key(|&i| sets[i].last_lsn)
    else {
        return Vec::new();
    };
    let mut chain = vec![full];
    let base = &sets[full];
    let diff = (0..sets.len())
        .filter(|&i| {
            let s = &sets[i];
            s.kind() == BackupKind::Differential
                && s.database_backup_lsn == base.checkpoint_lsn
                && s.last_lsn > base.last_lsn
        })
        .max_by_key(|&i| sets[i].last_lsn);
    if let Some(d) = diff {
        chain.push(d);
    }
    let mut last = sets[*chain.last().unwrap()].last_lsn;
    let mut logs: Vec<usize> = (0..sets.len()).filter(|&i| sets[i].kind() == BackupKind::Log && !sets[i].copy_only).collect();
    logs.sort_by_key(|&i| sets[i].first_lsn);
    let mut first = true;
    for i in logs {
        let s = &sets[i];
        let fits = if first { s.first_lsn <= last && s.last_lsn > last } else { s.first_lsn == last };
        if fits {
            chain.push(i);
            last = s.last_lsn;
            first = false;
        }
    }
    chain
}

// ---------------------------------------------------------------------------------------------
// BACKUP
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackupType {
    Full,
    Differential,
    Log,
}

impl BackupType {
    pub fn label(self) -> &'static str {
        match self {
            BackupType::Full => "Full",
            BackupType::Differential => "Differential",
            BackupType::Log => "Transaction Log",
        }
    }

    fn extension(self) -> &'static str {
        match self {
            BackupType::Full => "bak",
            BackupType::Differential => "dif",
            BackupType::Log => "trn",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    ServerDefault,
    On,
    Off,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BackupOptions {
    pub db: String,
    pub kind: BackupType,
    pub copy_only: bool,
    pub files: Vec<String>,
    pub compression: Compression,
    pub checksum: bool,
    /// Overwrite the media set's backup sets (INIT) rather than append (NOINIT).
    pub init: bool,
    pub verify: bool,
    /// Server certificate for `ENCRYPTION (ALGORITHM = AES_256, ...)`.
    pub certificate: Option<String>,
}

/// `<db>_<yyyyMMdd_HHmmss>.bak` (`.dif` / `.trn` for differential / log), safe as a file name.
pub fn default_file_name(db: &str, kind: BackupType, now: chrono::NaiveDateTime) -> String {
    let safe: String = db.chars().map(|c| if "\\/:*?\"<>|".contains(c) { '_' } else { c }).collect();
    format!("{safe}_{}.{}", now.format("%Y%m%d_%H%M%S"), kind.extension())
}

pub fn backup_script(o: &BackupOptions) -> Result<String, String> {
    if o.files.iter().all(|f| f.trim().is_empty()) {
        return Err("Add at least one destination file.".into());
    }
    let files: Vec<String> = o.files.iter().map(|f| f.trim().to_string()).filter(|f| !f.is_empty()).collect();
    let db = quote_ident(&o.db);
    let verb = if o.kind == BackupType::Log { "LOG" } else { "DATABASE" };
    let mut with = Vec::new();
    if o.kind == BackupType::Differential {
        with.push("DIFFERENTIAL".to_string());
    }
    if o.copy_only {
        with.push("COPY_ONLY".into());
    }
    if o.certificate.is_some() {
        // An encrypted backup can't be appended to an existing media set.
        with.push("FORMAT".into());
        with.push("INIT".into());
    } else {
        with.push(if o.init { "INIT" } else { "NOINIT" }.into());
    }
    with.push(format!("NAME = {}", quote_literal(&format!("{}-{} Database Backup", o.db, o.kind.label()))));
    match o.compression {
        Compression::ServerDefault => {}
        Compression::On => with.push("COMPRESSION".into()),
        Compression::Off => with.push("NO_COMPRESSION".into()),
    }
    if o.checksum {
        with.push("CHECKSUM".into());
    }
    if let Some(cert) = &o.certificate {
        with.push(format!("ENCRYPTION (ALGORITHM = AES_256, SERVER CERTIFICATE = {})", quote_ident(cert)));
    }
    with.push("STATS = 5".into());
    let mut s = format!("BACKUP {verb} {db} TO {}\nWITH {};\nGO\n", from_disk(&files), with.join(", "));
    if o.verify {
        let checksum = if o.checksum { ", CHECKSUM" } else { "" };
        if o.init || o.certificate.is_some() {
            s.push_str(&format!("RESTORE VERIFYONLY FROM {} WITH FILE = 1{checksum};\nGO\n", from_disk(&files)));
        } else {
            // Appended: verify the backup set just written, as SSMS does.
            s.push_str(&format!(
                "DECLARE @position int = ISNULL((SELECT position FROM msdb.dbo.backupset WHERE backup_set_id = \
                 (SELECT MAX(backup_set_id) FROM msdb.dbo.backupset WHERE database_name = {})), 1);\n\
                 RESTORE VERIFYONLY FROM {} WITH FILE = @position{checksum};\nGO\n",
                quote_literal(&o.db),
                from_disk(&files)
            ));
        }
    }
    Ok(s)
}

pub fn recovery_model_query(db: &str) -> String {
    format!("SELECT recovery_model_desc FROM sys.databases WHERE name = {}", quote_literal(db))
}

/// Certificates that can encrypt a backup (with a private key, not system ones).
pub const CERTIFICATES_QUERY: &str = "SELECT name FROM master.sys.certificates \
     WHERE pvt_key_encryption_type <> 'NA' AND name NOT LIKE N'##%' ORDER BY name";

// ---------------------------------------------------------------------------------------------
// RESTORE
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum RecoveryState {
    Recovery,
    NoRecovery,
    /// Read-only between log restores, with this undo file.
    Standby(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct RestoreStep {
    pub kind: BackupKind,
    pub files: Vec<String>,
    pub position: i32,
}

impl From<&BackupSet> for RestoreStep {
    fn from(s: &BackupSet) -> Self {
        RestoreStep { kind: s.kind(), files: s.files.clone(), position: s.position }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RestoreOptions {
    pub target: String,
    pub steps: Vec<RestoreStep>,
    /// (logical name, new physical path) for `WITH MOVE`.
    pub moves: Vec<(String, String)>,
    pub replace: bool,
    pub recovery: RecoveryState,
    /// Point in time for log restores, e.g. `2026-11-25 13:00:00`.
    pub stop_at: Option<String>,
    /// `SET SINGLE_USER WITH ROLLBACK IMMEDIATE` first, `MULTI_USER` after.
    pub close_connections: bool,
    pub keep_replication: bool,
    pub checksum: bool,
}

pub fn restore_script(o: &RestoreOptions) -> Result<String, String> {
    if o.target.trim().is_empty() {
        return Err("Enter the target database name.".into());
    }
    let Some(first) = o.steps.first() else { return Err("Select at least one backup set to restore.".into()) };
    if first.kind != BackupKind::Full {
        return Err("The first backup set to restore must be a full backup.".into());
    }
    for (kind, what) in [(BackupKind::Full, "full"), (BackupKind::Differential, "differential")] {
        if o.steps.iter().filter(|s| s.kind == kind).count() > 1 {
            return Err(format!("Select only one {what} backup."));
        }
    }
    if o.stop_at.is_some() && !o.steps.iter().any(|s| s.kind == BackupKind::Log) {
        return Err("A point in time needs at least one log backup.".into());
    }
    let db = quote_ident(&o.target);
    let lit = quote_literal(&o.target);
    let mut s = String::from("USE [master];\nGO\n");
    if o.close_connections {
        s.push_str(&format!("IF DB_ID({lit}) IS NOT NULL ALTER DATABASE {db} SET SINGLE_USER WITH ROLLBACK IMMEDIATE;\nGO\n"));
    }
    let last = o.steps.len() - 1;
    for (i, step) in o.steps.iter().enumerate() {
        let verb = if step.kind == BackupKind::Log { "LOG" } else { "DATABASE" };
        let mut with = vec![format!("FILE = {}", step.position.max(1))];
        if i == 0 {
            for (logical, physical) in &o.moves {
                with.push(format!("MOVE {} TO {}", quote_literal(logical), quote_literal(physical)));
            }
            if o.replace {
                with.push("REPLACE".into());
            }
        }
        if o.checksum {
            with.push("CHECKSUM".into());
        }
        if step.kind == BackupKind::Log
            && let Some(t) = &o.stop_at
        {
            with.push(format!("STOPAT = {}", quote_literal(t)));
        }
        if i == last {
            match &o.recovery {
                RecoveryState::Recovery => {
                    if o.keep_replication {
                        with.push("KEEP_REPLICATION".into());
                    }
                    with.push("RECOVERY".into());
                }
                RecoveryState::NoRecovery => with.push("NORECOVERY".into()),
                RecoveryState::Standby(f) => with.push(format!("STANDBY = {}", quote_literal(f))),
            }
        } else {
            with.push("NORECOVERY".into());
        }
        with.push("STATS = 5".into());
        s.push_str(&format!("RESTORE {verb} {db} FROM {}\nWITH {};\nGO\n", from_disk(&step.files), with.join(",\n    ")));
    }
    if o.close_connections && o.recovery != RecoveryState::NoRecovery {
        s.push_str(&format!("ALTER DATABASE {db} SET MULTI_USER;\nGO\n"));
    }
    Ok(s)
}

/// `WITH MOVE` targets: data files to the data folder and logs to the log folder, renamed
/// for the new database, never two files on the same path.
pub fn default_moves(files: &[LogicalFile], source_db: &str, target_db: &str, folders: &ServerFolders) -> Vec<(String, String)> {
    let mut used: Vec<String> = Vec::new();
    files
        .iter()
        .map(|f| {
            let base = PathStyle::guess(&f.physical).file_name(&f.physical);
            let base = if base.is_empty() { format!("{}.mdf", f.logical) } else { base };
            let renamed = if source_db.eq_ignore_ascii_case(target_db) || target_db.is_empty() {
                base
            } else {
                match find_ci(&base, source_db) {
                    Some(i) if !source_db.is_empty() => format!("{}{target_db}{}", &base[..i], &base[i + source_db.len()..]),
                    _ => format!("{target_db}_{base}"),
                }
            };
            let dir = if f.kind == "L" { &folders.log } else { &folders.data };
            let mut path = folders.style.join(dir, &renamed);
            let mut n = 2;
            while used.iter().any(|u| u.eq_ignore_ascii_case(&path)) {
                let (stem, ext) = match renamed.rfind('.') {
                    Some(i) => (&renamed[..i], &renamed[i..]),
                    None => (renamed.as_str(), ""),
                };
                path = folders.style.join(dir, &format!("{stem}_{n}{ext}"));
                n += 1;
            }
            used.push(path.clone());
            (f.logical.clone(), path)
        })
        .collect()
}

fn find_ci(hay: &str, needle: &str) -> Option<usize> {
    hay.to_lowercase().find(&needle.to_lowercase()).filter(|_| hay.len() == hay.to_lowercase().len())
}

#[cfg(test)]
mod tests;
