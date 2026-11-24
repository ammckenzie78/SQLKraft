use sqlkraft::{app, capture};

fn main() -> eframe::Result {
    capture::install();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .thread_name("sqlkraft-db")
        .build()
        .expect("failed to start tokio runtime");
    let handle = runtime.handle().clone();

    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_title("SQLKraft")
            .with_app_id("sqlkraft")
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
