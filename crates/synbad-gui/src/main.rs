//! `synbad-gui` — egui-based configuration and status UI for Synbad.

use tracing_subscriber::EnvFilter;

mod app;
mod ipc_thread;
mod layout_editor;
mod tray;

use app::SynbadApp;

fn main() -> eframe::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Synbad")
            .with_inner_size([900.0, 600.0])
            .with_min_inner_size([640.0, 420.0]),
        ..Default::default()
    };

    // Install the tray first so its event loop (GTK on Linux) is up before
    // eframe takes the main thread. The handle must outlive `run_native`.
    let tray_handle = tray::install();
    let has_tray = tray_handle.is_some();

    let result = eframe::run_native(
        "Synbad",
        options,
        Box::new(move |cc| Box::new(SynbadApp::new(cc, has_tray))),
    );

    // Keep the tray alive until eframe returns.
    drop(tray_handle);
    result
}
