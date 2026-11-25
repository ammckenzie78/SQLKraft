//! The native Open / Save file pickers, behind a trait so tests can stub them.

use std::path::PathBuf;

/// Asks the user for a file. `None` means they cancelled.
pub trait FileDialogs {
    /// File > Open: a `.sql` file to open.
    fn open_sql(&self) -> Option<PathBuf>;
    /// Save / Save As: where to write a `.sql` file, suggesting `file_name`.
    fn save_sql(&self, file_name: &str) -> Option<PathBuf>;
    /// Export Results to CSV: where to write the file, suggesting `file_name`.
    fn save_csv(&self, file_name: &str) -> Option<PathBuf>;
}

/// The platform's dialogs, through `rfd`.
pub struct NativeFileDialogs;

impl FileDialogs for NativeFileDialogs {
    fn open_sql(&self) -> Option<PathBuf> {
        rfd::FileDialog::new().add_filter("SQL files", &["sql"]).add_filter("All files", &["*"]).pick_file()
    }

    fn save_sql(&self, file_name: &str) -> Option<PathBuf> {
        rfd::FileDialog::new().add_filter("SQL files", &["sql"]).set_file_name(file_name).save_file()
    }

    fn save_csv(&self, file_name: &str) -> Option<PathBuf> {
        rfd::FileDialog::new().add_filter("CSV", &["csv"]).set_file_name(file_name).save_file()
    }
}
