//! `synbad-gui` — egui-based configuration and status UI for Synbad.

use std::sync::{Arc, Mutex};

use tracing_subscriber::EnvFilter;

mod app;
mod ipc_thread;
mod layout_editor;
mod single_instance;
mod tray;

use app::SynbadApp;

type RepaintFn = Arc<dyn Fn() + Send + Sync>;

fn main() -> eframe::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // Single-instance check happens *before* we install a tray icon, so
    // a second launcher click doesn't even briefly add a second icon to
    // the systray. The repaint hook lives in a Mutex<Option<_>> because
    // we don't have the egui context yet — we install the real callback
    // once eframe gives us the CreationContext.
    let repaint_slot: Arc<Mutex<Option<RepaintFn>>> = Arc::new(Mutex::new(None));
    let repaint_for_instance: RepaintFn = {
        let slot = repaint_slot.clone();
        Arc::new(move || {
            if let Ok(g) = slot.lock() {
                if let Some(r) = g.as_ref() {
                    r();
                }
            }
        })
    };

    let show_rx = match single_instance::acquire(
        single_instance::default_socket_path(),
        repaint_for_instance,
    ) {
        single_instance::AcquireResult::Acquired(guard, rx) => {
            // Leak the guard so its Drop (socket cleanup) runs only on
            // process exit. Dropping mid-run would race with our own
            // listener thread.
            Box::leak(Box::new(guard));
            Some(rx)
        }
        single_instance::AcquireResult::Forwarded => {
            tracing::info!("another synbad-gui is running — raised existing window");
            return Ok(());
        }
        single_instance::AcquireResult::Unsupported => None,
    };

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
        Box::new(move |cc| {
            // Now that eframe has the egui context, plug the real repaint
            // hook into every background source that needs to wake the UI
            // loop: the single-instance listener (SHOW pings from a second
            // launcher) and the tray menu handler (clicks that arrive
            // while the window is hidden in close-to-tray — without the
            // wake-up, Show/Quit would never reach `update()`).
            let egui_ctx = cc.egui_ctx.clone();
            let repaint: RepaintFn = Arc::new(move || egui_ctx.request_repaint());
            *repaint_slot.lock().unwrap() = Some(repaint.clone());
            tray::set_repaint(repaint);
            Box::new(SynbadApp::new(cc, has_tray, show_rx))
        }),
    );

    // Keep the tray alive until eframe returns; binding holds it past the
    // call above so the OS handle (when the `tray` feature is enabled)
    // outlives the event loop.
    let _keep_alive = tray_handle;
    result
}
