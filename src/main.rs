// No console window behind the GUI in Windows release builds.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::sync::Arc;

use eframe::egui;
use sqlkraft::{app, capture};

/// 256x256 RGBA rendered from packaging/sqlkraft.svg by build.rs.
const ICON_RGBA: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/icon-256.rgba"));

fn main() -> eframe::Result {
    if std::env::args().any(|a| a == "--version" || a == "-V") {
        println!("sqlkraft {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    if std::env::args().any(|a| a == "--mcp") {
        // MCP server on stdio for Claude Code: `claude mcp add sqlkraft -- sqlkraft --mcp`.
        let settings = sqlkraft::settings::Settings::load();
        let mut backend = sqlkraft::mcp::headless::Headless::from_settings(&settings);
        let stdin = std::io::stdin().lock();
        let stdout = std::io::stdout().lock();
        if let Err(e) = sqlkraft::mcp::serve(&mut backend, stdin, stdout) {
            eprintln!("sqlkraft --mcp: {e}");
        }
        return Ok(());
    }
    capture::install();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("sqlkraft-db")
        .build()
        .expect("failed to start tokio runtime");
    let handle = runtime.handle().clone();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("SQLKraft")
            .with_app_id("sqlkraft")
            .with_icon(Arc::new(egui::IconData {
                rgba: ICON_RGBA.to_vec(),
                width: 256,
                height: 256,
            }))
            .with_inner_size([1400.0, 900.0])
            .with_min_inner_size([800.0, 500.0]),
        ..Default::default()
    };
    let result = eframe::run_native(
        "SQLKraft",
        options,
        Box::new(move |cc| Ok(Box::new(app::SqlKraftApp::new(cc, handle)))),
    );
    runtime.shutdown_timeout(std::time::Duration::from_secs(2));
    result
}
